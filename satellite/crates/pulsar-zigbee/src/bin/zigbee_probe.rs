//! Zigbee probe: the Midi's radio without the daemon.
//! usage: zigbee_probe info               the running firmware (0x0900)
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
        _ => {
            eprintln!("usage: zigbee_probe info | flash <image.ybl>");
            ExitCode::from(2)
        }
    }
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
