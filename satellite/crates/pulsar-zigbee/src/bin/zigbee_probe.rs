//! Zigbee probe: the Midi's radio without the daemon.
//! usage: zigbee_probe [port]             the coordinator (pulsar's coordinator firmware must be
//!                                        in the radio) and its deCONZ server (default 6639; the
//!                                        daemon serves 6638), for zigpy / Zigbee2MQTT
//!        zigbee_probe info               the running firmware (0x0900)
//!        zigbee_probe flash <image.ybl>  write a radio firmware image
//! The daemon must not run its Zigbee role at the same time: both would drive /dev/ttyS2.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use log::{debug, error, info};
use pulsar_zigbee::RadioLines;
use pulsar_zigbee::bytes::{get_be16, get_be32};
use pulsar_zigbee::coordinator::{ApsFrame, Coordinator, Hooks};
use pulsar_zigbee::deconz::DeconzServer;
use pulsar_zigbee::flasher::{radio_flash, ybl_load};
use pulsar_zigbee::link::{Link, LinkEvent};

const APP_INFO_REQ: u16 = 0x0900;
const APP_INFO_RSP: u16 = 0x8900;
/// Reset + boot + 0x0900 normally take < 1 s (the daemon waits as long).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    pulsar_core::init_logger();
    pulsar_core::add_own_target(module_path!());
    let model = pulsar_core::model::detect_model().filter(|m| !m.zigbee_tty.is_empty());
    let Some(model) = model else {
        error!("no Zigbee radio on this model");
        return ExitCode::FAILURE;
    };
    let lines = RadioLines {
        tty: model.zigbee_tty.into(),
        reset: model.zigbee_reset.into(),
        boot: model.zigbee_boot.into(),
    };
    // OS strings: an image path need not be UTF-8.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match &args[..] {
        [cmd] if cmd == "info" => info_mode(&lines),
        [cmd, image] if cmd == "flash" => flash_mode(&lines, &PathBuf::from(image)),
        [] => server_mode(&lines, 6639),
        [port] => server_mode(&lines, atoi(&port.to_string_lossy()) as u16),
        _ => {
            eprintln!("usage: zigbee_probe [port] | info | flash <image.ybl>");
            ExitCode::from(2)
        }
    }
}

/// C atoi, as the C++ probe read its port: leading digits (with a sign), else 0.
fn atoi(s: &str) -> i32 {
    let s = s.trim_start();
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let v = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0i32, |v, d| {
            v.wrapping_mul(10).wrapping_add(i32::from(d - b'0'))
        });
    if neg { v.wrapping_neg() } else { v }
}

/// The coordinator's hooks: log, then the deCONZ server's.
struct ProbeHooks<'a> {
    server: &'a mut DeconzServer,
}

impl Hooks for ProbeHooks<'_> {
    fn received(&mut self, coord: &mut Coordinator, f: &ApsFrame) {
        info!(
            "rx 0x{:04x}:{} -> {}0x{:04x}:{} profile 0x{:04x} cluster 0x{:04x} {} bytes lqi {} rssi {}",
            f.src,
            f.src_ep,
            if f.group { "group " } else { "" },
            f.dst,
            f.dst_ep,
            f.profile,
            f.cluster,
            f.data.len(),
            f.lqi,
            f.rssi
        );
        self.server.received(coord, f);
    }

    fn sent(&mut self, coord: &mut Coordinator, handle: u32, status: u8) {
        info!("sent {handle:08x}: status 0x{status:02x}");
        self.server.sent(coord, handle, status);
    }

    fn changed(&mut self, coord: &mut Coordinator) {
        let n = coord.network();
        info!(
            "ready {} formed {} PAN 0x{:04x} channel {} IEEE {:016x}",
            u8::from(coord.ready()),
            u8::from(n.formed),
            n.pan,
            n.channel,
            n.ieee
        );
        self.server.changed(coord);
    }

    fn formed(&mut self, coord: &mut Coordinator, ok: bool) {
        self.server.formed(coord, ok);
    }
}

/// Runs the link, the coordinator and a deCONZ server on `port` until killed.
fn server_mode(lines: &RadioLines, port: u16) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async {
        // The port first: without it the radio is not touched (as the C++ probe).
        let mut server = DeconzServer::new(port);
        if !server.start() {
            return ExitCode::FAILURE;
        }
        let Some(link) = Link::open(lines) else {
            return ExitCode::FAILURE;
        };
        let mut coord = Coordinator::new(link);
        server.attach(&mut coord);
        coord.start(&mut ProbeHooks {
            server: &mut server,
        });
        loop {
            tokio::select! {
                w = coord.next() => coord.handle(w, &mut ProbeHooks { server: &mut server }),
                w = server.next() => server.handle(w, &mut coord),
            }
        }
    })
}

/// Resets the radio and asks its application which firmware it is; prints
/// `version=… family=… manufacturer=… image_type=… size=…`.
fn info_mode(lines: &RadioLines) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async {
        let Some(mut link) = Link::open(lines) else {
            return ExitCode::FAILURE;
        };
        let timeout = tokio::time::sleep(ANSWER_TIMEOUT);
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                ev = link.next() => match ev {
                    LinkEvent::Boot => link.send(APP_INFO_REQ, &[], None),
                    // [version BE32][manufacturer BE16][image type BE16][size BE32], the same
                    // from every firmware
                    LinkEvent::Frame { ty: APP_INFO_RSP, payload } if payload.len() >= 12 => {
                        let version = get_be32(&payload);
                        let line = format!(
                            "version=0x{version:08x} family=0x{:02x} manufacturer=0x{:04x} image_type=0x{:04x} size={}",
                            version >> 24,
                            get_be16(&payload[4..]),
                            get_be16(&payload[6..]),
                            get_be32(&payload[8..])
                        );
                        // println! would panic (abort) on a closed pipe.
                        let _ = writeln!(std::io::stdout(), "{line}");
                        return ExitCode::SUCCESS;
                    }
                    _ => {}
                },
                () = &mut timeout => {
                    error!("the radio does not identify its firmware");
                    return ExitCode::FAILURE;
                }
            }
        }
    })
}

fn flash_mode(lines: &RadioLines, path: &Path) -> ExitCode {
    let (image, info) = match ybl_load(path) {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    info!(
        "writing {} (version 0x{:08x})",
        path.display(),
        info.version
    );
    match radio_flash(lines, &image, &mut |p| debug!("{} %", (p * 100.0) as i32)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}
