//! Offline evaluation of a stock spotter model on a recording made on the device: raw capture
//! WAV (S16, layout of the detected model, e.g. `arecord -D hw:0,1 -c 8 -r 48000` on the Lite 2,
//! or `frontend_probe --record`) -> Yandex VQE -> spotter. The spotter channel layout is derived
//! from the model like the daemon does.
//! usage: ww_offline <wav> [model_dir]   (default: .../spotter_models/activation/alisa)
//! Prints every spotter event with its time; "HIT" lines are confirmations (events 2 and 5), and
//! per second the RMS of the speaker reference and of the VQE output (echo cancellation check).
//! Experiments: VT=<type> feeds every spotter channel from that VQE output; SPK=<ch>[,<ch>...]
//! overrides the speaker reference capture channels; unlike the C++ tool, it must name as many
//! channels as the model's VQE preset takes (the library is never given other counts).
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::fs::File;
use std::io::{BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pulsar_audio::engine::{
    Lib, LoadError, SPOTTER_RATE, SPOTTER_TO_VQE, SPOTTER_TYPES, Spotter, SpotterConf, VQE_MODELS,
    Vqe, VqeStep, preload_deps, spotter_layout,
};

const DEFAULT_MODEL: &str = "/system/vendor/quasar/spotter_models/activation/alisa";
const WAV_HEADER: u64 = 44;

// Output is line by line, as the C++ tool's unbuffered stdout; a closed pipe is ignored.
macro_rules! out {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

fn main() -> ExitCode {
    ExitCode::from(run())
}

fn run() -> u8 {
    pulsar_core::init_logger();
    pulsar_core::add_own_target(module_path!());
    let Some(m) = pulsar_core::model::detect_model() else {
        return 1;
    };
    pulsar_alsa::init(m);
    let spk_channels = match std::env::var_os("SPK") {
        Some(s) => parse_channels(&s.to_string_lossy()),
        None => m.spk_channels.to_vec(),
    };
    let cap = m.capture_channels as usize;
    if let Some(&c) = spk_channels.iter().find(|&&c| c as usize >= cap) {
        let _ = writeln!(
            std::io::stderr(),
            "SPK: no capture channel {c} ({cap} channels)"
        );
        return 2;
    }
    // The VQE preset fixes the number of speaker reference channels; SPK only picks which
    // capture channels feed them.
    if spk_channels.len() != m.spk_channels.len() {
        let _ = writeln!(
            std::io::stderr(),
            "SPK: {} channels given, the {} preset takes {}",
            spk_channels.len(),
            m.vqe_preset,
            m.spk_channels.len()
        );
        return 2;
    }
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() < 2 {
        let argv0 = args
            .first()
            .map(|a| a.to_string_lossy())
            .unwrap_or_default();
        let _ = writeln!(std::io::stderr(), "usage: {argv0} wav [model_dir]");
        return 2;
    }
    let model_dir = args
        .get(2)
        .map_or_else(|| PathBuf::from(DEFAULT_MODEL), PathBuf::from);
    // Unlike the C++ tool, also runs on the Lite 2 without LD_LIBRARY_PATH (libiconv.so).
    let _ = preload_deps();
    let lib = match Lib::load() {
        Ok(lib) => lib,
        Err(LoadError::Open(e)) => {
            out!("dlopen {e}");
            return 1;
        }
        Err(LoadError::Missing(name)) => {
            out!("missing {name}");
            return 1;
        }
    };
    let mut f = match File::open(&args[1]) {
        Ok(f) => BufReader::new(f),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "wav: {}", strerror(&e));
            return 1;
        }
    };
    // As fseek: a stream that cannot seek is read from where it is.
    let _ = f.seek(SeekFrom::Start(WAV_HEADER));

    let (nmic, nspk) = (m.mic_channels.len(), spk_channels.len());
    let mut vqe = match Vqe::new(&lib, m.vqe_preset, Path::new(VQE_MODELS), nmic, nspk) {
        Ok(v) => v,
        Err((VqeStep::Config, e)) => {
            out!("cfg: {e}");
            return 1;
        }
        Err((VqeStep::Create, e)) => {
            out!("vqe: {e}");
            return 1;
        }
    };
    let frames = match usize::try_from(vqe.chunk()) {
        Ok(n @ 1..) => n,
        _ => {
            out!("cfg: input chunk size {}", vqe.chunk());
            return 1;
        }
    };
    // Samples per spotter channel and chunk: 512, 256 on the Mini 3 and Mini.
    let frame = match usize::try_from(vqe.frame()) {
        Ok(n @ 1..) => n,
        _ => {
            out!("cfg: processing frame {}", vqe.frame());
            return 1;
        }
    };

    let conf = match SpotterConf::read(&lib, &model_dir) {
        Ok(c) => c,
        Err(e) => {
            out!("conf: {}", e.as_deref().unwrap_or("ok"));
            return 1;
        }
    };
    let mut counts = [0i64; SPOTTER_TYPES];
    for (t, n) in counts.iter_mut().enumerate() {
        *n = conf.channel_count(t as i64).unwrap_or(0);
    }
    let mut layout = spotter_layout(&counts, SPOTTER_TO_VQE[0]);
    // VT=<type>: every channel from that VQE output (experiments).
    if let Some(vt) = std::env::var_os("VT") {
        layout.vqe.fill(atoi(&vt.to_string_lossy()));
    }
    let mut sp = match Spotter::create(&conf, SPOTTER_RATE, &layout.types) {
        (Some(sp), _) => sp,
        (None, e) => {
            out!("spotter: {}", e.as_deref().unwrap_or("ok"));
            return 1;
        }
    };
    out!(
        "model {}: {} channels",
        model_dir.display(),
        layout.types.len()
    );
    if lib.event_name(0).is_none() {
        out!("missing event_id_to_str");
        return 1;
    }

    let nch = layout.types.len();
    let (mic_ch, spk_ch) = (m.mic_channels, spk_channels.as_slice());
    let (nmic, nspk) = (mic_ch.len(), spk_ch.len());
    let mut raw = vec![0u8; frames * cap * 2];
    let mut input = vec![0i16; frames * cap];
    let mut sbuf = vec![0i16; frame * nch];
    let mut mics = vec![0f32; frames * nmic];
    let mut spk = vec![0f32; frames * nspk];
    let (mut t, mut spk_e, mut out_e, mut next_report) = (0f64, 0f64, 0f64, 1f64);
    let (mut spk_n, mut out_n) = (0usize, 0usize);
    let mut hits = 0;
    while f.read_exact(&mut raw).is_ok() {
        for (s, b) in input.iter_mut().zip(raw.as_chunks::<2>().0) {
            *s = i16::from_le_bytes(*b);
        }
        for (i, fr) in input.chunks_exact(cap).enumerate() {
            for (c, &ch) in mic_ch.iter().enumerate() {
                mics[i * nmic + c] = f32::from(fr[ch as usize]) / 32768.0;
            }
            for (c, &ch) in spk_ch.iter().enumerate() {
                let v = f32::from(fr[ch as usize]) / 32768.0;
                spk[i * nspk + c] = v;
                // Fused on the station build of the C++ tool (clang -ffp-contract=on).
                spk_e = f64::from(v).mul_add(f64::from(v), spk_e);
                spk_n += 1;
            }
        }
        if let Err(e) = vqe.process(&mics, &spk) {
            out!("process: {e}");
            return 1;
        }
        for (k, &ty) in layout.vqe.iter().enumerate() {
            let d = match vqe.output(ty) {
                Ok(d) => d,
                Err(e) => {
                    out!("out {ty}: {e}");
                    return 1;
                }
            };
            for (i, &x) in d.iter().take(frame).enumerate() {
                let v = x * 32768.0;
                // Truncated, unlike the daemon's lrint: the C++ tool's conversion.
                sbuf[i * nch + k] = if v > 32767.0 {
                    32767
                } else if v < -32768.0 {
                    -32768
                } else {
                    v as i16
                };
                if k == 0 {
                    out_e = f64::from(x).mul_add(f64::from(x), out_e);
                    out_n += 1;
                }
            }
        }
        if let Err(e) = sp.submit(&sbuf) {
            out!("submit: {e}");
            return 1;
        }
        for _ in 0..16 {
            let ev = match sp.next_event() {
                Ok(ev) if !matches!(ev, 0 | 4 | 7 | 9) => ev,
                _ => break,
            };
            let (phrase, _) = sp.phrase();
            let hit = ev == 2 || ev == 5;
            hits += i32::from(hit);
            out!(
                "t={t:6.2}s event {ev} {} phrase={}{}",
                lib.event_name(ev).unwrap_or_default(),
                phrase.unwrap_or_default(),
                if hit { "  HIT" } else { "" }
            );
        }
        t += frames as f64 / f64::from(m.capture_rate);
        if t >= next_report {
            out!(
                "t={t:6.2}s rms dBFS: speaker ref {:6.1}, vqe out {:6.1}",
                10.0 * (spk_e / spk_n.max(1) as f64 + 1e-12).log10(),
                10.0 * (out_e / out_n.max(1) as f64 + 1e-12).log10()
            );
            (spk_e, out_e, spk_n, out_n) = (0.0, 0.0, 0, 0);
            next_report += 1.0;
        }
    }
    out!("done: {t:.1}s, hits={hits}");
    0
}

/// perror's text: the C library message without Rust's " (os error N)".
fn strerror(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}

/// Leading whitespace, sign and decimal digits of `s`: (value, bytes used); (0, 0) without
/// digits, as strtoul / atoi.
fn c_number(s: &[u8]) -> (i64, usize) {
    let mut i = s
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r'))
        .count();
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digits = s[i..].iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return (0, 0);
    }
    let v = s[i..i + digits].iter().fold(0i64, |v, &d| {
        v.saturating_mul(10).saturating_add(i64::from(d - b'0'))
    });
    (if neg { v.wrapping_neg() } else { v }, i + digits)
}

/// atoi.
fn atoi(s: &str) -> i32 {
    c_number(s.as_bytes()).0 as i32
}

/// SPK=<ch>[,<ch>...] as the C++ tool's strtoul loop: "4,6" -> [4, 6], "4,,6" -> [4, 0, 6],
/// "4," -> [4]. A character that is neither a digit nor a comma ends the list (the C++ loop
/// spun there forever).
fn parse_channels(s: &str) -> Vec<u32> {
    let b = s.as_bytes();
    let mut v = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let (n, used) = c_number(&b[i..]);
        v.push(n as u32);
        i += used;
        if b.get(i) == Some(&b',') {
            i += 1;
        } else if used == 0 {
            break;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spk_list() {
        assert_eq!(parse_channels("6"), [6]);
        assert_eq!(parse_channels("4,6"), [4, 6]);
        assert_eq!(parse_channels("4,,6"), [4, 0, 6]);
        assert_eq!(parse_channels("4,"), [4]);
        assert_eq!(parse_channels(" 4, 5"), [4, 5]);
        assert_eq!(parse_channels(""), Vec::<u32>::new());
        assert_eq!(parse_channels("4x"), [4, 0]);
    }

    #[test]
    fn vt_atoi() {
        assert_eq!(atoi("6"), 6);
        assert_eq!(atoi(" 9 "), 9);
        assert_eq!(atoi("-2"), -2);
        assert_eq!(atoi("+3x"), 3);
        assert_eq!(atoi(""), 0);
        assert_eq!(atoi("x"), 0);
    }
}
