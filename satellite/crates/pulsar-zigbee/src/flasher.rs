//! Writes an application image into the Midi's Zigbee radio through its resident bootloader
//! ("Yandex Bootloader v1.0.02": boot line held while reset is pulsed, then a text menu on the
//! same UART at 115200: "1. upload ybl", "2. run", prompt "BL > "; the image goes over XMODEM,
//! 128-byte blocks, CRC-16 or checksum as the receiver asks). This is maind's own update path
//! (ncp_manager.cpp updateFirmware); the bootloader itself (flash 0..0x7FFF) is never written.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use log::{error, info};
use pulsar_core::sysfs;

use crate::{RadioLines, os_error_text, sys};

const SOH: u8 = 0x01;
const EOT: u8 = 0x04;
const ACK: u8 = 0x06;
const NAK: u8 = 0x15;
const CAN: u8 = 0x18;
/// Yandex, as in gateway.ybl.
const MANUFACTURER: u16 = 0x140A;
/// B91, gateway, bootloader layout.
const IMAGE_TYPE: u16 = 0x0580;
/// Application slot below the gateway's OTA area.
const MAX_IMAGE: usize = 0x77000 - 0x8000;

/// Header of a .ybl (Telink image + zlib CRC-32 trailer, see radio/ybl.c).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct YblInfo {
    pub version: u32,
    pub manufacturer: u16,
    pub image_type: u16,
}

fn le32(p: &[u8]) -> u32 {
    u32::from_le_bytes([p[0], p[1], p[2], p[3]])
}

/// zlib's CRC-32 (reflected 0xEDB88320).
pub(crate) fn crc32(p: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in p {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}

/// XMODEM's CRC-16 (CCITT 0x1021, initial 0).
pub(crate) fn crc16_xmodem(p: &[u8]) -> u16 {
    let mut c = 0u16;
    for &b in p {
        c ^= u16::from(b) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x1021
            } else {
                c << 1
            };
        }
    }
    c
}

/// Checks a .ybl the way maind does before uploading (size field, Telink start flag, CRC-32,
/// manufacturer 0x140A, image type 0x0580).
pub fn ybl_parse(f: &[u8]) -> Result<YblInfo, &'static str> {
    if f.len() < 0x40 + 4 {
        return Err("file too short");
    }
    let size = le32(&f[0x18..]) as usize;
    if size.checked_add(4) != Some(f.len()) {
        return Err("size field does not match the file");
    }
    if size > MAX_IMAGE {
        return Err("image too large");
    }
    if &f[0x20..0x24] != b"KNLT" {
        return Err("no Telink start flag");
    }
    if crc32(&f[..size]) != le32(&f[size..]) {
        return Err("CRC-32 mismatch");
    }
    let i = YblInfo {
        version: le32(&f[2..]),
        manufacturer: u16::from_le_bytes([f[0x12], f[0x13]]),
        image_type: u16::from_le_bytes([f[0x14], f[0x15]]),
    };
    if i.manufacturer != MANUFACTURER || i.image_type != IMAGE_TYPE {
        return Err("not an image for this radio");
    }
    Ok(i)
}

/// Reads and checks an image file. Errors name the path.
pub fn ybl_load(path: &Path) -> Result<(Vec<u8>, YblInfo), String> {
    let Ok(mut f) = File::open(path) else {
        return Err(format!("cannot read {}", path.display()));
    };
    // What a failing read left is checked as it is (a directory reads as empty), as the C++
    // stream did.
    let mut file = Vec::new();
    let _ = f.read_to_end(&mut file);
    match ybl_parse(&file) {
        Ok(info) => Ok((file, info)),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

struct Port {
    file: File,
}

impl Port {
    fn open(tty: &str) -> io::Result<Port> {
        let file = sys::open_tty(tty, false)?;
        // Never write menu keys or an image into a regular file.
        sys::ensure_tty(&file)?;
        // As the C++ flasher: a port that refuses the settings is still tried.
        let _ = sys::set_raw_115200(&file, false);
        sys::flush_io(&file);
        Ok(Port { file })
    }

    fn put(&mut self, mut b: &[u8]) -> bool {
        while !b.is_empty() {
            match self.file.write(b) {
                Ok(n @ 1..) => b = &b[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                _ => return false,
            }
        }
        true
    }

    /// Next byte within `ms`.
    fn get(&mut self, ms: i32) -> Option<u8> {
        if !sys::wait_readable(&self.file, ms) {
            return None;
        }
        let mut c = [0u8];
        match self.file.read(&mut c) {
            Ok(1) => Some(c[0]),
            _ => None,
        }
    }

    /// Reads until `token` shows up (true) or nothing arrives for `quiet_ms`; printable bytes
    /// and newlines go to `seen`.
    fn expect(&mut self, token: &[u8], quiet_ms: i32, seen: &mut Vec<u8>) -> bool {
        loop {
            let Some(c) = self.get(quiet_ms) else {
                return false;
            };
            if c >= 0x20 || c == b'\n' {
                seen.push(c);
            }
            // Only the byte just added can complete the token.
            if seen.ends_with(token) {
                return true;
            }
        }
    }
}

fn printable(seen: &[u8]) -> String {
    String::from_utf8_lossy(seen).replace('\n', "|")
}

fn fail(why: String) -> Result<(), String> {
    error!("zigbee flash: {why}");
    Err(why)
}

/// Blocking (~25 s for an image), with no way to stop it: run it off the runtime thread, with
/// nothing else holding the tty. `progress` gets 0..1. Errors are logged. After an upload, or
/// when XMODEM does not start, the radio is told to run its application (menu item 2); without
/// a bootloader prompt it gets a reset. After a cancel by the bootloader, a block or the EOT
/// that is never acknowledged, it stays in the bootloader until the next reset (the service's
/// next bring-up).
pub fn radio_flash(
    lines: &RadioLines,
    image: &[u8],
    progress: &mut dyn FnMut(f32),
) -> Result<(), String> {
    let mut port = match Port::open(&lines.tty) {
        Ok(p) => p,
        Err(e) => return fail(format!("cannot open {}: {}", lines.tty, os_error_text(&e))),
    };

    // Bootloader entry exactly as maind (zb_control): boot and reset asserted, reset released
    // after 100 ms, boot 100 ms later.
    if !sysfs::write_file(&lines.boot, "1") || !sysfs::write_file(&lines.reset, "1") {
        return fail("cannot drive the radio's reset/boot lines".into());
    }
    sleep(Duration::from_millis(100));
    sysfs::write_file(&lines.reset, "0");
    sleep(Duration::from_millis(100));
    sysfs::write_file(&lines.boot, "0");

    let mut seen = Vec::new();
    let mut menu = false;
    for i in 0..3 {
        if menu {
            break;
        }
        if i > 0 {
            port.put(b"\n");
        }
        menu = port.expect(b"BL >", 1000, &mut seen);
    }
    if !menu {
        // Not in the bootloader: the application keeps running (or a reset brings it back).
        sysfs::write_file(&lines.reset, "1");
        sleep(Duration::from_millis(100));
        sysfs::write_file(&lines.reset, "0");
        return fail(format!("no bootloader prompt (got '{}')", printable(&seen)));
    }
    info!("zigbee flash: bootloader: {}", printable(&seen));

    // "1. upload ybl", then XMODEM: the receiver asks with 'C' (CRC-16) or NAK (checksum).
    port.put(b"1");
    let deadline = Instant::now() + Duration::from_secs(10);
    let use_crc = loop {
        match port.get(100) {
            Some(b'C') => break true,
            Some(NAK) => break false,
            _ => {}
        }
        if Instant::now() > deadline {
            port.put(b"2");
            return fail("bootloader did not start XMODEM".into());
        }
    };

    let blocks = image.len().div_ceil(128);
    let mut pkt = [0u8; 133];
    for (b, chunk) in image.chunks(128).enumerate() {
        pkt[0] = SOH;
        // XMODEM block numbers start at 1 and wrap at 256.
        pkt[1] = (b + 1) as u8;
        pkt[2] = !pkt[1];
        pkt[3..3 + chunk.len()].copy_from_slice(chunk);
        pkt[3 + chunk.len()..131].fill(0);
        if chunk.len() < 128 {
            pkt[3 + chunk.len()] = 0x1A; // as maind pads the last block
        }
        let len = if use_crc {
            let crc = crc16_xmodem(&pkt[3..131]);
            pkt[131..133].copy_from_slice(&crc.to_be_bytes());
            133
        } else {
            pkt[131] = pkt[3..131].iter().fold(0u8, |s, &x| s.wrapping_add(x));
            132
        };
        let mut acked = false;
        for _ in 0..25 {
            port.put(&pkt[..len]);
            match port.get(3000) {
                Some(ACK) => {
                    acked = true;
                    break;
                }
                Some(CAN) if port.get(1000) == Some(CAN) => {
                    return fail(format!(
                        "bootloader cancelled the transfer at block {}",
                        b + 1
                    ));
                }
                _ => {}
            }
        }
        if !acked {
            for _ in 0..3 {
                port.put(&[CAN]);
            }
            return fail(format!("block {} not acknowledged", b + 1));
        }
        progress((b + 1) as f32 / blocks as f32);
    }
    let mut done = false;
    for _ in 0..10 {
        port.put(&[EOT]);
        done = port.get(2000) == Some(ACK);
        if done {
            break;
        }
    }
    if !done {
        return fail("end of transfer not acknowledged".into());
    }

    seen.clear();
    port.expect(b"BL >", 5000, &mut seen);
    info!("zigbee flash: upload done: {}", printable(&seen));
    port.put(b"2"); // run
    Ok(())
}

#[cfg(test)]
mod tests;
