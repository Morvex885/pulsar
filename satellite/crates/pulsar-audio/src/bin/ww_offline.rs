//! Офлайн-прогон стоковой модели спотера по записи, сделанной на станции: сырой WAV захвата
//! (S16, раскладка определённой модели, например `arecord -D hw:0,1 -c 8 -r 48000` на Лайт 2
//! или `frontend_probe --record`) -> VQE Яндекса -> спотер. Раскладка каналов спотера берётся
//! из модели, как в демоне.
//! usage: ww_offline <wav> [model_dir]   (по умолчанию .../spotter_models/activation/alisa)
//! Печатает каждое событие спотера со временем; раз в секунду — RMS референса динамика и выхода
//! VQE (проверка эхоподавления). Строки "HIT" — подтверждения (события 2 и 5) с полями `end` —
//! сэмплов на канал, поданных спотеру, включая эту подачу; `unhandled` —
//! spotter_get_unhandled_nbytes сразу после подтверждения (`-` без символа или при ошибке);
//! `at` = `end − unhandled/(2·каналов)` — точка хита в сэмплах от начала записи (`-`, если
//! значения нет, оно отрицательно или не кратно кадру всех каналов).
//! Эксперименты: VT=<type> подаёт на все каналы спотера этот выход VQE; LEGACY=1 — прежнюю
//! раскладку wake-спотера pulsar (`LEGACY_SPOTTER_TO_VQE`); SPK=<ch>[,<ch>...] заменяет каналы
//! захвата референса динамика; в отличие от C++-утилиты, их должно быть столько, сколько берёт
//! пресет VQE модели (библиотека не получает других количеств).
//! OUT=<wav>: выход VQE_OMNI_WITH_GAIN (его же демон кладёт в кольцо пре-ролла, с тем же
//! преобразованием в s16) пишется WAV 16 кГц моно, по кадру на чанк; сэмпл `at` в нём — точка хита.
//! SUBMIT=<n>: подавать спотеру куски по `n` сэмплов на канал (по умолчанию — кадр VQE), события
//! читаются после каждой подачи; остаток в конце файла не подаётся.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::fs::File;
use std::io::{BufReader, BufWriter, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pulsar_audio::engine::{
    LEGACY_SPOTTER_TO_VQE, Lib, LoadError, SPOTTER_RATE, SPOTTER_TO_VQE, SPOTTER_TYPES, Spotter,
    SpotterConf, VQE_MODELS, VQE_OMNI_WITH_GAIN, Vqe, VqeStep, preload_deps, spotter_layout,
    to_s16,
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
    let table = if std::env::var_os("LEGACY").is_some() {
        &LEGACY_SPOTTER_TO_VQE
    } else {
        &SPOTTER_TO_VQE
    };
    let mut layout = spotter_layout(&counts, table);
    // VT=<type>: every channel from that VQE output (experiments).
    if let Some(vt) = std::env::var_os("VT") {
        layout.vqe.fill(atoi(&vt.to_string_lossy()));
    }
    out!("channels [{layout}]");
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
    let submit = std::env::var("SUBMIT")
        .ok()
        .map(|s| atoi(&s))
        .filter(|&n| n > 0)
        .map_or(frame, |n| n as usize);
    let mut wav_out = match std::env::var_os("OUT") {
        Some(p) => match File::create(&p).and_then(|f| {
            let mut w = BufWriter::new(f);
            w.write_all(&wav_header(0))?;
            Ok(w)
        }) {
            Ok(w) => Some(w),
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "OUT: {}", strerror(&e));
                return 1;
            }
        },
        None => None,
    };

    let nch = layout.types.len();
    // Без каналов цикл подачи по SUBMIT не продвигался бы.
    if nch == 0 {
        out!("spotter: no channels in the model");
        return 1;
    }
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
    // Поданные спотеру сэмплы на канал и ещё не поданные (чередованные).
    let (mut end, mut out_samples) = (0u64, 0u64);
    let mut pending: Vec<i16> = Vec::with_capacity((frame + submit) * nch);
    loop {
        match f.read_exact(&mut raw) {
            Ok(()) => {}
            // Конец записи, в том числе неполный последний чанк.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                out!("wav: {}", strerror(&e));
                return 1;
            }
        }
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
            // Утилита для замеров не подменяет каналы молча.
            if d.is_empty() {
                out!("out {ty}: none in the preset");
                return 1;
            }
            for (i, &x) in d.iter().take(frame).enumerate() {
                sbuf[i * nch + k] = to_s16(x);
                if k == 0 {
                    out_e = f64::from(x).mul_add(f64::from(x), out_e);
                    out_n += 1;
                }
            }
        }
        if let Some(w) = wav_out.as_mut() {
            // Тот же выход и то же преобразование, что у кольца пре-ролла демона.
            let d = match vqe.output(VQE_OMNI_WITH_GAIN) {
                Ok(d) if d.len() >= frame => d,
                Ok(d) => {
                    out!("out {VQE_OMNI_WITH_GAIN}: {} of {frame} samples", d.len());
                    return 1;
                }
                Err(e) => {
                    out!("out {VQE_OMNI_WITH_GAIN}: {e}");
                    return 1;
                }
            };
            let bytes: Vec<u8> = d[..frame]
                .iter()
                .flat_map(|&v| to_s16(v).to_le_bytes())
                .collect();
            if let Err(e) = w.write_all(&bytes) {
                out!("OUT: {}", strerror(&e));
                return 1;
            }
            out_samples += (bytes.len() / 2) as u64;
        }
        pending.extend_from_slice(&sbuf);
        while pending.len() >= submit * nch {
            if let Err(e) = sp.submit(&pending[..submit * nch]) {
                out!("submit: {e}");
                return 1;
            }
            pending.drain(..submit * nch);
            end += submit as u64;
            for _ in 0..16 {
                let ev = match sp.next_event() {
                    Ok(ev) if !matches!(ev, 0 | 4 | 7 | 9) => ev,
                    _ => break,
                };
                let (phrase, _) = sp.phrase();
                let hit = ev == 2 || ev == 5;
                hits += i32::from(hit);
                let name = lib.event_name(ev).unwrap_or_default();
                let phrase = phrase.unwrap_or_default();
                if !hit {
                    out!("t={t:6.2}s event {ev} {name} phrase={phrase}");
                    continue;
                }
                // Сразу после phrase, до следующего события: в другой момент библиотека делает
                // abort.
                let u = match sp.unhandled_bytes() {
                    Some(Ok(v)) => Some(v),
                    Some(Err(e)) => {
                        out!("unhandled: {e}");
                        None
                    }
                    None => None,
                };
                let at = u
                    .and_then(|v| u64::try_from(v).ok())
                    .filter(|v| v % (2 * nch as u64) == 0)
                    .and_then(|v| end.checked_sub(v / (2 * nch as u64)));
                out!(
                    "t={t:6.2}s event {ev} {name} phrase={phrase}  HIT end={end} unhandled={} at={}",
                    dash(u),
                    dash(at)
                );
            }
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
    if let Some(mut w) = wav_out {
        let data = u32::try_from(out_samples * 2).unwrap_or(u32::MAX);
        if let Err(e) = w
            .seek(SeekFrom::Start(0))
            .and_then(|_| w.write_all(&wav_header(data)))
            .and_then(|()| w.flush())
        {
            out!("OUT: {}", strerror(&e));
            return 1;
        }
    }
    0
}

/// Значение или "-".
fn dash<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

/// 44-байтный заголовок WAV PCM S16LE, 16 кГц моно.
fn wav_header(data_bytes: u32) -> [u8; 44] {
    let rate = SPOTTER_RATE as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&data_bytes.saturating_add(36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    h
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
