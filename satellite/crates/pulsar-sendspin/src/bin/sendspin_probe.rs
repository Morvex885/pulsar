//! Sendspin player without the daemon, for tools/sendspin_server.py (run with the
//! station's service stopped).
//! usage: sendspin_probe [port=18928] [name=pulsar-test] [seconds=120] [state_dir=/tmp/pulsar-test/sendspin]
//!
//! Starts the service and prints its events (stdout; the service logs to stderr). 3 s after a
//! server connects it sends the transport command NEXT once. On hardware-volume models the
//! device's "Master Playback Volume" is capped at raw 82 (0.2 on the Mixer scale,
//! docs/stations.md) for the run and restored afterwards; the Mini has no such control and uses
//! the same software volume 0.2 through the model's dmix sink.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use pulsar_alsa::{Ctl, Pcm, Stream};
use pulsar_core::model::Model;
use pulsar_core::{MusicFrame, actors};
use pulsar_sendspin::{SendspinConfig, SendspinEvent, Transport};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;

const MASTER: &str = "Master Playback Volume";
const MAX_RAW: i64 = 82;
const TRANSPORT_AFTER: Duration = Duration::from_secs(3);

struct Args {
    port: u16,
    name: String,
    seconds: u64,
    state_dir: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() > 4 || a.first().is_some_and(|s| s == "-h" || s == "--help") {
        return Err(
            "usage: sendspin_probe [port=18928] [name=pulsar-test] [seconds=120] \
                    [state_dir=/tmp/pulsar-test/sendspin]"
                .into(),
        );
    }
    let arg = |i: usize, default: &str| a.get(i).map_or(default, String::as_str).to_owned();
    Ok(Args {
        port: arg(0, "18928").parse().map_err(|e| format!("port: {e}"))?,
        name: arg(1, "pulsar-test"),
        seconds: arg(2, "120").parse().map_err(|e| format!("seconds: {e}"))?,
        state_dir: PathBuf::from(arg(3, "/tmp/pulsar-test/sendspin")),
    })
}

/// "Master Playback Volume" capped for the run, restored on drop.
struct VolumeCap {
    ctl: Ctl,
    orig: [i64; 2],
}

impl VolumeCap {
    /// Ok(None): no such control (the Mini's software volume), nothing to cap.
    fn apply(model: &Model) -> Result<Option<Self>, String> {
        if model.software_volume {
            println!("volume: software (0.2)");
            return Ok(None);
        }
        let ctl = match Ctl::open("hw:0") {
            Ok(ctl) => ctl,
            Err(e) => {
                println!("volume: hw:0: {}, not capped", pulsar_alsa::strerror(e));
                return Ok(None);
            }
        };
        if !ctl.mixer_exists(MASTER) {
            // Opening the softvol PCM once creates the control.
            drop(Pcm::open(model.playback_pcm, Stream::Playback, true));
            if !ctl.mixer_exists(MASTER) {
                println!("volume: no '{MASTER}' on hw:0, not capped");
                return Ok(None);
            }
        }
        let read = |i| {
            ctl.read_integer(MASTER, i)
                .map_err(|e| format!("volume: read: {}", pulsar_alsa::strerror(e)))
        };
        let orig = [read(0)?, read(1)?];
        let capped = orig.map(|v| v.min(MAX_RAW));
        ctl.write_integers(MASTER, &capped)
            .map_err(|e| format!("volume: write: {}", pulsar_alsa::strerror(e)))?;
        println!(
            "volume: master raw {}/{} -> {}/{}",
            orig[0], orig[1], capped[0], capped[1]
        );
        Ok(Some(Self { ctl, orig }))
    }
}

impl Drop for VolumeCap {
    fn drop(&mut self) {
        match self.ctl.write_integers(MASTER, &self.orig) {
            Ok(()) => println!(
                "volume: master raw restored to {}/{}",
                self.orig[0], self.orig[1]
            ),
            Err(e) => println!("volume: restore: {}", pulsar_alsa::strerror(e)),
        }
    }
}

#[derive(Default)]
struct Counts {
    starts: u32,
    ends: u32,
    volume: u32,
    mute: u32,
    music: u32,
    transport: Option<bool>,
}

fn main() -> ExitCode {
    pulsar_core::init_logger();
    pulsar_core::add_own_target(module_path!());
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let Some(model) = pulsar_core::model::detect_model() else {
        return ExitCode::FAILURE;
    };
    pulsar_alsa::init(model);
    // As in the daemon (the player sets it): alsa-lib's own messages only at debug level.
    pulsar_alsa::quiet_error_handler();
    let cap = match VolumeCap::apply(model) {
        Ok(cap) => cap,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let local = tokio::task::LocalSet::new();
    let result = local.block_on(&rt, run(&args, model));
    local.block_on(&rt, actors::join_all(Duration::from_secs(3)));
    drop(local);
    rt.shutdown_timeout(Duration::from_secs(1));
    drop(cap);
    match result {
        Ok(c) => {
            println!(
                "summary: stream start {} end {}, volume {}, mute {}, music frames {}, transport next {}",
                c.starts,
                c.ends,
                c.volume,
                c.mute,
                c.music,
                match c.transport {
                    Some(true) => "sent",
                    Some(false) => "not offered",
                    None => "not tried",
                }
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &Args, model: &'static Model) -> Result<Counts, String> {
    if let Some(parent) = args.state_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let cfg = SendspinConfig {
        name: args.name.clone(),
        product_name: "pulsar sendspin_probe".into(),
        manufacturer: "pulsar".into(),
        software_version: "probe".into(),
        mac: String::new(),
        state_dir: args.state_dir.clone(),
        model,
        port: args.port,
    };
    let volume = 0.2;
    let master = pulsar_core::volume::master_gain(model, volume);
    let (handle, mut events) = pulsar_sendspin::spawn(cfg, volume, false, master)?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| format!("signal: {e}"))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| format!("signal: {e}"))?;
    println!(
        "sendspin_probe: ws://0.0.0.0:{}/sendspin as '{}' for {} s",
        args.port, args.name, args.seconds
    );

    let t0 = Instant::now();
    let stamp = || format!("[{:8.3}]", t0.elapsed().as_secs_f64());
    let end = tokio::time::sleep(Duration::from_secs(args.seconds));
    tokio::pin!(end);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut c = Counts::default();
    let mut connected = false;
    let mut connected_at: Option<Instant> = None;
    let (mut second, mut frames, mut beats) = (Instant::now(), 0u32, 0u32);
    let mut last_frame: Option<MusicFrame> = None;
    loop {
        tokio::select! {
            () = &mut end => break,
            _ = term.recv() => break,
            _ = int.recv() => break,
            ev = events.recv() => match ev {
                None => break,
                Some(SendspinEvent::Stream(on)) => {
                    if on { c.starts += 1 } else { c.ends += 1 }
                    println!("{} stream {} (streaming={})", stamp(), if on { "start" } else { "end" },
                             handle.streaming());
                }
                Some(SendspinEvent::Volume(v)) => {
                    c.volume += 1;
                    println!("{} volume {v:.2}", stamp());
                }
                Some(SendspinEvent::Mute(m)) => {
                    c.mute += 1;
                    println!("{} mute {}", stamp(), u8::from(m));
                }
                Some(SendspinEvent::PairingCode(code)) => println!("{} pairing code '{code}'", stamp()),
                Some(SendspinEvent::PairingWindow(open)) => println!("{} pairing window {}", stamp(),
                                                                      if open { "open" } else { "closed" }),
                Some(SendspinEvent::Music(f)) => {
                    c.music += 1;
                    frames += 1;
                    beats += u32::from(f.beat);
                    last_frame = Some(f);
                }
            },
            _ = tick.tick() => {
                let now = handle.connected();
                if now != connected {
                    connected = now;
                    println!("{} {}", stamp(), if now { "connected" } else { "disconnected" });
                    if now && connected_at.is_none() {
                        connected_at = Some(Instant::now());
                    }
                }
                if c.transport.is_none() && connected && connected_at.is_some_and(|t| t.elapsed() >= TRANSPORT_AFTER) {
                    let ok = handle.transport(Transport::Next);
                    c.transport = Some(ok);
                    println!("{} transport next: {}", stamp(), if ok { "sent" } else { "not offered" });
                }
                if second.elapsed() >= Duration::from_secs(1) {
                    if let Some(f) = last_frame.take() {
                        let bands: Vec<String> = f.bands.iter().map(|b| format!("{b:.2}")).collect();
                        println!("{} music {frames} frames/s, {beats} beats, loudness {:.2}, bands [{}]",
                                 stamp(), f.loudness, bands.join(" "));
                    }
                    (second, frames, beats) = (Instant::now(), 0, 0);
                }
            }
        }
    }
    println!(
        "{} stopping (streaming={}, connected={})",
        stamp(),
        handle.streaming(),
        handle.connected()
    );
    Ok(c)
}
