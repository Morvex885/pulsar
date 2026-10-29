//! No audio is played: the null PCM exercises ALSA's near/get pointer ABI without hardware.
//! Run on versioned Midi and unversioned Lite 2 firmware after cross-building this tool.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::process::ExitCode;

use pulsar_alsa::{Format, HwRequest, Pcm, Stream};

fn main() -> ExitCode {
    if let Some(model) = pulsar_core::model::detect_model() {
        pulsar_alsa::init(model);
    }
    if let Err(e) = pulsar_alsa::load() {
        eprintln!("load libasound: {e}");
        return ExitCode::FAILURE;
    }
    let mut pcm = match Pcm::open("null", Stream::Playback, false) {
        Ok(pcm) => pcm,
        Err(err) => {
            eprintln!("open null PCM: {}", pulsar_alsa::strerror(err));
            return ExitCode::FAILURE;
        }
    };
    let t = pcm.set_hw_params_traced(&HwRequest {
        format: Format::S16Le,
        channels: 2,
        rate: 96000,
        rate_exact: false,
        period_frames: Some(3072),
        buffer_frames: Some(24576),
    });
    if let Some((what, err)) = t.failed {
        eprintln!("{what}: {}", pulsar_alsa::strerror(err));
    }
    let (near, actual) = (t.near, t.actual);
    let ok = t.failed.is_none()
        && near.rate == 96000
        && near.period_frames == 3072
        && near.buffer_frames == 24576
        && actual.period_frames == near.period_frames
        && actual.buffer_frames == near.buffer_frames;
    // alias=no: the library has only the current ABI (unversioned, no __snd_* exports).
    println!(
        "rate={} period={}/{} buffer={}/{} alias={}: {}",
        near.rate,
        near.period_frames,
        actual.period_frames,
        near.buffer_frames,
        actual.buffer_frames,
        if pulsar_alsa::has_modern_aliases() {
            "yes"
        } else {
            "no"
        },
        if ok { "PASS" } else { "FAIL" }
    );
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
