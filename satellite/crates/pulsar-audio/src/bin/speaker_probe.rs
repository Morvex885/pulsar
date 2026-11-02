//! Замер узнавания голоса на станции: отпечатки WAV-файлов (16 кГц, моно, s16) и их попарное
//! сходство. Нужен только `speaker_probe a.wav b.wav ...`; демон и звук не затрагивает.
#![deny(unsafe_code)]

use std::process::ExitCode;
use std::time::Instant;

use pulsar_audio::speaker::Embedder;

/// Отсчёты чанка `data`; заголовок разбирается только для такого WAV, как пишет отладка HA.
fn read_wav(path: &str) -> Result<Vec<i16>, String> {
    let b = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(format!("{path}: not a WAV file"));
    }
    let mut at = 12;
    while at + 8 <= b.len() {
        let size = u32::from_le_bytes([b[at + 4], b[at + 5], b[at + 6], b[at + 7]]) as usize;
        let body = at + 8;
        if &b[at..at + 4] == b"data" {
            let end = body.saturating_add(size).min(b.len());
            let (samples, _) = b[body..end].as_chunks::<2>();
            return Ok(samples.iter().map(|&c| i16::from_le_bytes(c)).collect());
        }
        at = body.saturating_add(size + (size & 1));
    }
    Err(format!("{path}: no data chunk"))
}

fn main() -> ExitCode {
    let files: Vec<String> = std::env::args().skip(1).collect();
    let mut embedder = match Embedder::open() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let mut embeddings = Vec::new();
    for f in &files {
        let pcm = match read_wav(f) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
        let t = Instant::now();
        match embedder.embed(&pcm) {
            Ok(Some(e)) => {
                println!(
                    "{f}: {:.2} s of audio, {} ms",
                    pcm.len() as f32 / 16000.0,
                    t.elapsed().as_millis()
                );
                embeddings.push((f, e));
            }
            Ok(None) => println!("{f}: too little speech"),
            Err(e) => {
                eprintln!("{f}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    for (i, (a, ea)) in embeddings.iter().enumerate() {
        for (b, eb) in &embeddings[i + 1..] {
            let score: f32 = ea.iter().zip(eb).map(|(x, y)| x * y).sum();
            println!("{a} {b} {score:.3}");
        }
    }
    ExitCode::SUCCESS
}
