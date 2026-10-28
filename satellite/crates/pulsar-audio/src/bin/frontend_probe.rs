//! Runs the microphone front-end without the daemon (stop the service first: the capture device
//! has one user) and prints its events, plus once a second: VQE chunks, audio chunks, music
//! frames, the captured rate and how far the processing thread is behind (FIFO).
//! usage: frontend_probe [seconds=30] [--wake alisa|yandex] [--stream] [--music] [--commands]
//!                       [--record <wav> <seconds>]
//! Without --commands only the wake word spotter runs (the daemon runs the command spotter too,
//! unless the microphone is muted). --record writes the first <seconds> of the raw capture (all
//! channels, S16) as a WAV for ww_offline. No sound is played.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::fs::File;
use std::io::{BufWriter, Seek as _, SeekFrom, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use pulsar_audio::{FrontendEvent, FrontendStats};
use pulsar_core::model::Model;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::{Instant, MissedTickBehavior};

const MODELS: &str = "/system/vendor/quasar/spotter_models/activation/";
const COMMAND_MODEL: &str = "/system/vendor/quasar/spotter_models/command";
const USAGE: &str = "usage: frontend_probe [seconds=30] [--wake alisa|yandex] [--stream] [--music] [--commands] [--record <wav> <seconds>]";

macro_rules! out {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

struct Args {
    seconds: u64,
    wake: String,
    stream: bool,
    music: bool,
    commands: bool,
    record: Option<(PathBuf, u64)>,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        seconds: 30,
        wake: "alisa".into(),
        stream: false,
        music: false,
        commands: false,
        record: None,
    };
    let secs = |s: Option<String>| -> Result<u64, String> {
        let s = s.ok_or("missing seconds")?;
        s.parse()
            .ok()
            .filter(|&n| n > 0)
            .ok_or(format!("bad seconds: {s}"))
    };
    let mut seconds_seen = false;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--wake" => match it.next().as_deref() {
                Some(w @ ("alisa" | "yandex")) => a.wake = w.into(),
                other => return Err(format!("bad --wake: {}", other.unwrap_or(""))),
            },
            "--stream" => a.stream = true,
            "--music" => a.music = true,
            "--commands" => a.commands = true,
            "--record" => {
                let path = it.next().ok_or("--record: missing wav")?;
                a.record = Some((PathBuf::from(path), secs(it.next())?));
            }
            s if !seconds_seen && !s.starts_with('-') => {
                a.seconds = secs(Some(arg))?;
                seconds_seen = true;
            }
            s => return Err(format!("unknown argument: {s}")),
        }
    }
    Ok(a)
}

/// 44-byte header of a PCM S16LE WAV.
fn wav_header(channels: u16, rate: u32, data_bytes: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let block = channels * 2;
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&data_bytes.saturating_add(36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * u32::from(block)).to_le_bytes());
    h[32..34].copy_from_slice(&block.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    h
}

/// Raw capture into a WAV; the header is written with the final size.
struct Recorder {
    file: BufWriter<File>,
    path: PathBuf,
    channels: u16,
    rate: u32,
    left: u64,
    samples: u64,
}

impl Recorder {
    fn create(path: PathBuf, m: &Model, seconds: u64) -> std::io::Result<Recorder> {
        let mut file = BufWriter::new(File::create(&path)?);
        let channels = m.capture_channels as u16;
        file.write_all(&wav_header(channels, m.capture_rate, 0))?;
        Ok(Recorder {
            file,
            path,
            channels,
            rate: m.capture_rate,
            left: seconds * u64::from(m.capture_rate) * u64::from(channels),
            samples: 0,
        })
    }

    /// Appends up to the requested length; false once complete.
    fn write(&mut self, block: &[i16]) -> std::io::Result<bool> {
        let n = block
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let mut bytes = Vec::with_capacity(n * 2);
        for s in &block[..n] {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.left -= n as u64;
        self.samples += n as u64;
        Ok(self.left > 0)
    }

    fn finish(mut self) -> std::io::Result<()> {
        let data = u32::try_from(self.samples * 2).unwrap_or(u32::MAX);
        self.file.seek(SeekFrom::Start(0))?;
        self.file
            .write_all(&wav_header(self.channels, self.rate, data))?;
        self.file.flush()?;
        let frames = self.samples / u64::from(self.channels);
        out!(
            "recorded {}: {frames} frames, {:.2} s, {} ch @ {} Hz",
            self.path.display(),
            frames as f64 / f64::from(self.rate),
            self.channels,
            self.rate
        );
        Ok(())
    }
}

fn main() -> ExitCode {
    pulsar_core::init_logger();
    pulsar_core::add_own_target(module_path!());
    let Some(m) = pulsar_core::model::detect_model() else {
        return ExitCode::from(1);
    };
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(run(m, args)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "frontend_probe: {e}");
            ExitCode::from(1)
        }
    }
}

/// Ok(false): no VQE chunk was processed.
async fn run(m: &'static Model, a: Args) -> std::io::Result<bool> {
    out!(
        "frontend_probe: {} ({}), {} s, wake {}, stream {}, music {}, commands {}{}",
        m.model_name,
        m.codename,
        a.seconds,
        a.wake,
        on(a.stream),
        on(a.music),
        on(a.commands),
        a.record.as_ref().map_or(String::new(), |(p, s)| format!(
            ", record {} s to {}",
            s,
            p.display()
        ))
    );
    let mut recorder = match a.record {
        Some((path, secs)) => Some(Recorder::create(path, m, secs)?),
        None => None,
    };
    let (h, mut rx) = pulsar_audio::spawn(
        m,
        PathBuf::from(format!("{MODELS}{}", a.wake)),
        Some(PathBuf::from(COMMAND_MODEL)),
    );
    h.set_commands(a.commands);
    h.set_streaming(a.stream);
    h.set_music_analysis(a.music);
    let (tap_tx, mut tap_rx) = unbounded_channel();
    if recorder.is_some() {
        h.set_raw_tap(Some(tap_tx));
    } else {
        drop(tap_tx);
    }

    let start = Instant::now();
    let end = tokio::time::sleep(Duration::from_secs(a.seconds));
    tokio::pin!(end);
    let mut tick = tokio::time::interval_at(start + Duration::from_secs(1), Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let (mut audio, mut music, mut beats, mut wakes, mut commands) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut audio_samples_ok, mut loudness) = (true, 0f32);
    let mut prev = FrontendStats::default();
    let mut prev_counts = (0u64, 0u64);
    let mut first: Option<(Instant, FrontendStats, u64, u64)> = None;
    loop {
        tokio::select! {
            () = &mut end => break,
            ev = rx.recv() => match ev {
                Some(FrontendEvent::Wake { phrase, level_db, preroll }) => {
                    wakes += 1;
                    out!(
                        "t={:6.2}s WAKE '{phrase}' level {level_db:.1} dB preroll {} ms",
                        secs(start),
                        preroll.len() / 16
                    );
                    h.wake_handled();
                }
                Some(FrontendEvent::Command { word }) => {
                    commands += 1;
                    out!("t={:6.2}s COMMAND '{word}'", secs(start));
                }
                Some(FrontendEvent::Audio(pcm)) => {
                    audio += 1;
                    audio_samples_ok &= pcm.len() == 512;
                }
                Some(FrontendEvent::Music(f)) => {
                    music += 1;
                    beats += u64::from(f.beat);
                    loudness = f.loudness;
                }
                None => break,
            },
            block = tap_rx.recv(), if recorder.is_some() => {
                if let (Some(block), Some(r)) = (block, recorder.as_mut())
                    && !r.write(&block)?
                {
                    h.set_raw_tap(None);
                    if let Some(r) = recorder.take() {
                        r.finish()?;
                    }
                }
            },
            _ = tick.tick() => {
                let s = h.stats();
                if first.is_none() && s.chunks > 0 {
                    first = Some((Instant::now(), s, audio, music));
                }
                out!(
                    "t={:3.0}s chunks {:3} audio {:3} music {:3} (beats {beats}, loudness {loudness:.2}) captured {} frames fifo {} frames ({:.1} ms) dropped {}",
                    secs(start),
                    s.chunks - prev.chunks,
                    audio - prev_counts.0,
                    music - prev_counts.1,
                    s.captured_frames - prev.captured_frames,
                    s.fifo_frames,
                    s.fifo_frames as f64 * 1000.0 / f64::from(m.capture_rate),
                    s.dropped_frames,
                );
                prev = s;
                prev_counts = (audio, music);
            }
        }
    }
    let s = h.stats();
    drop(h);
    if let Some(r) = recorder.take() {
        // Ended before the requested length.
        r.finish()?;
    }
    out!(
        "done: {:.1} s, chunks {}, audio {}{}, music {} (beats {beats}), wake {wakes}, command {commands}, captured {} frames, dropped {} frames",
        secs(start),
        s.chunks,
        audio,
        if audio_samples_ok {
            ""
        } else {
            " (not 512 samples each)"
        },
        music,
        s.captured_frames,
        s.dropped_frames
    );
    // Rates from the first second that had chunks (the capture start is not counted).
    if let Some((t0, s0, audio0, music0)) = first {
        let dt = t0.elapsed().as_secs_f64();
        if dt > 0.5 {
            out!(
                "rates over {dt:.1} s: chunks {:.2}/s, audio {:.2}/s, music {:.2}/s, captured {:.1} frames/s (model {} Hz)",
                (s.chunks - s0.chunks) as f64 / dt,
                (audio - audio0) as f64 / dt,
                (music - music0) as f64 / dt,
                (s.captured_frames - s0.captured_frames) as f64 / dt,
                m.capture_rate
            );
        }
    }
    Ok(s.chunks > 0)
}

fn secs(start: Instant) -> f64 {
    start.elapsed().as_secs_f64()
}

fn on(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Result<Args, String> {
        parse_args(s.split_whitespace().map(String::from))
    }

    #[test]
    fn arguments() {
        let a = args("").unwrap();
        assert_eq!(
            (a.seconds, a.wake.as_str(), a.stream, a.music, a.commands),
            (30, "alisa", false, false, false)
        );
        assert!(a.record.is_none());
        let a = args("20 --stream --music --commands --wake yandex --record /tmp/x.wav 5").unwrap();
        assert_eq!(
            (a.seconds, a.wake.as_str(), a.stream, a.music, a.commands),
            (20, "yandex", true, true, true)
        );
        assert_eq!(a.record, Some((PathBuf::from("/tmp/x.wav"), 5)));
        assert!(args("--wake bob").is_err());
        assert!(args("0").is_err());
        assert!(args("10 20").is_err());
        assert!(args("--record x.wav").is_err());
        assert!(args("--loud").is_err());
    }

    #[test]
    fn wav_header_layout() {
        let h = wav_header(8, 48000, 768_000);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 768_036);
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 8);
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 48000);
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 768_000);
        assert_eq!(u16::from_le_bytes([h[32], h[33]]), 16);
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 768_000);
    }
}
