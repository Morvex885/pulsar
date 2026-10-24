use std::collections::HashSet;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::test_util::{Lines, assert_golden, hex};

fn pattern(n: usize, mul: usize, add: usize) -> Vec<u8> {
    (0..n).map(|i| (i * mul + add) as u8).collect()
}

fn put32(v: &mut [u8], at: usize, x: u32) {
    v[at..at + 4].copy_from_slice(&x.to_le_bytes());
}

/// `size` bytes of image (header fields filled in) + CRC-32 trailer, as the C++ harness built it.
fn ybl(size: usize, version: u32, manufacturer: u16, image_type: u16) -> Vec<u8> {
    let mut v = pattern(size, 7, 3);
    put32(&mut v, 2, version);
    v[0x12..0x14].copy_from_slice(&manufacturer.to_le_bytes());
    v[0x14..0x16].copy_from_slice(&image_type.to_le_bytes());
    put32(&mut v, 0x18, size as u32);
    v[0x20..0x24].copy_from_slice(b"KNLT");
    v.resize(size + 4, 0);
    let crc = crc32(&v[..size]);
    put32(&mut v, size, crc);
    v
}

/// CRC-32, CRC-16 and every `ybl_parse` branch against the C++ functions
/// (tests/golden/crc_ybl.txt).
#[test]
fn crc_and_ybl_parse_match_cxx_golden() {
    let mut out = Vec::new();
    for input in [
        vec![],
        b"123456789".to_vec(),
        pattern(256, 1, 0),
        pattern(1000, 31, 7),
        vec![0xFF],
        vec![0x00],
    ] {
        out.push(format!(
            "crc {} crc32={:08x} crc16={:04x}",
            hex(&input),
            crc32(&input),
            crc16_xmodem(&input)
        ));
    }
    let mut case = |name: &str, f: &[u8]| {
        out.push(match ybl_parse(f) {
            Ok(i) => format!(
                "ybl {name} ok version={:08x} manufacturer={:04x} image_type={:04x}",
                i.version, i.manufacturer, i.image_type
            ),
            Err(e) => format!("ybl {name} err {e}"),
        })
    };
    case("valid", &ybl(0x200, 0x5001_0203, 0x140A, 0x0580));
    case("valid-min", &ybl(0x40, 0x5100_0001, 0x140A, 0x0580));
    case("valid-max", &ybl(0x6F000, 0x1500_0351, 0x140A, 0x0580));
    case("empty", &[]);
    case("too-short", &[0; 0x43]);
    let mut f = ybl(0x200, 0x5001_0203, 0x140A, 0x0580);
    f.push(0);
    case("size-mismatch-longer", &f);
    f.truncate(f.len() - 2);
    case("size-mismatch-shorter", &f);
    case("too-large", &ybl(0x6F001, 0x5001_0203, 0x140A, 0x0580));
    let mut f = ybl(0x200, 0x5001_0203, 0x140A, 0x0580);
    f[0x21] = b'X';
    case("no-start-flag", &f);
    let mut f = ybl(0x200, 0x5001_0203, 0x140A, 0x0580);
    f[0x100] ^= 1;
    case("crc-mismatch", &f);
    let mut f = ybl(0x200, 0x5001_0203, 0x140A, 0x0580);
    f[0x200] ^= 1;
    case("crc-trailer-mismatch", &f);
    case(
        "wrong-manufacturer",
        &ybl(0x200, 0x5001_0203, 0x140B, 0x0580),
    );
    case("wrong-image-type", &ybl(0x200, 0x5001_0203, 0x140A, 0x0581));
    let mut f = ybl(0x6F004, 0x5001_0203, 0x140A, 0x0580);
    f[0x20] = 0;
    case("too-large-no-flag", &f);
    assert_golden(
        include_str!("../../tests/golden/crc_ybl.txt"),
        &out.join("\n"),
    );
}

#[test]
fn ybl_load_names_the_path() {
    let lines = Lines::new(); // a scratch directory
    let dir = lines.dir().to_path_buf();
    let good = dir.join("good.ybl");
    std::fs::write(&good, ybl(0x200, 0x5001_0203, 0x140A, 0x0580)).unwrap();
    let (file, info) = ybl_load(&good).unwrap();
    assert_eq!(file.len(), 0x204);
    assert_eq!(info.version, 0x5001_0203);
    let bad = dir.join("bad.ybl");
    std::fs::write(&bad, b"nope").unwrap();
    assert_eq!(
        ybl_load(&bad).unwrap_err(),
        format!("{}: file too short", bad.display())
    );
    let missing = dir.join("missing.ybl");
    assert_eq!(
        ybl_load(&missing).unwrap_err(),
        format!("cannot read {}", missing.display())
    );
    // A directory opens and reads as nothing, as with the C++ stream.
    assert_eq!(
        ybl_load(&dir).unwrap_err(),
        format!("{}: file too short", dir.display())
    );
}

#[test]
fn refuses_a_path_that_is_not_a_tty() {
    let lines = Lines::new();
    let mut l = lines.radio_lines();
    l.tty = lines
        .dir()
        .join("image-sink")
        .to_string_lossy()
        .into_owned();
    std::fs::write(&l.tty, "").unwrap();
    let err = radio_flash(&l, &[1, 2, 3], &mut |_| {}).unwrap_err();
    assert_eq!(
        err,
        format!("cannot open {}: Inappropriate ioctl for device", l.tty)
    );
    assert_eq!(std::fs::read(&l.tty).unwrap(), b"");
    assert_eq!(lines.get(), ("x".into(), "x".into()), "lines untouched");
}

#[derive(Clone, Copy)]
struct Script {
    name: &'static str,
    image_size: usize,
    start: u8,
    nak_once: &'static [u8],
    can_once: &'static [u8],
    can_block: u8,
    nak_always: u8,
    eot_naks: u32,
    no_prompt: bool,
    no_xmodem: bool,
}

const BASE: Script = Script {
    name: "",
    image_size: 128,
    start: b'C',
    nak_once: &[],
    can_once: &[],
    can_block: 0,
    nak_always: 0,
    eot_naks: 0,
    no_prompt: false,
    no_xmodem: false,
};

/// The bootloader as the C++ harness emulated it; returns every byte the flasher wrote.
fn emulate(master: &File, s: &Script, done: &AtomicBool) -> Vec<u8> {
    let mut rx = Vec::new();
    let put = |t: &[u8]| {
        let mut m = master;
        m.write_all(t).unwrap();
    };
    let get = |rx: &mut Vec<u8>, mut ms: i32| -> Option<u8> {
        loop {
            if crate::sys::wait_readable(master, 50) {
                let mut c = [0u8];
                let mut m = master;
                return match m.read(&mut c) {
                    Ok(1) => {
                        rx.push(c[0]);
                        Some(c[0])
                    }
                    _ => None,
                };
            }
            if done.load(Ordering::Relaxed) {
                return None;
            }
            ms -= 50;
            if ms <= 0 {
                return None;
            }
        }
    };
    // Menu on every newline.
    loop {
        match get(&mut rx, 10_000) {
            None => return rx,
            Some(b'\n') if s.no_prompt => put(b"garbage\r\nmore\x01\r\n"),
            Some(b'\n') => {
                put(b"\r\nYandex Bootloader v1.0.02\r\n1. upload ybl\r\n2. run\r\nBL > ");
                break;
            }
            Some(_) => {}
        }
    }
    loop {
        match get(&mut rx, 10_000) {
            None => return rx,
            Some(b'1') => break,
            Some(_) => {}
        }
    }
    if s.no_xmodem {
        loop {
            match get(&mut rx, 15_000) {
                None | Some(b'2') => return rx,
                Some(_) => {}
            }
        }
    }
    put(&[s.start]);
    let pkt = if s.start == b'C' { 133 } else { 132 };
    let mut seen = HashSet::new();
    let mut eots = 0;
    loop {
        let Some(c) = get(&mut rx, 10_000) else {
            return rx;
        };
        if c == EOT {
            if eots < s.eot_naks {
                eots += 1;
                put(&[NAK]);
                continue;
            }
            put(&[ACK]);
            put(b"\r\nupload ok\r\nBL > ");
            break;
        }
        if c != SOH {
            continue; // the sender's CAN
        }
        for _ in 1..pkt {
            if get(&mut rx, 1000).is_none() {
                return rx;
            }
        }
        let block = rx[rx.len() - pkt + 1];
        let first = seen.insert(block);
        if block == s.can_block {
            put(&[CAN, CAN]);
        } else if block == s.nak_always || (first && s.nak_once.contains(&block)) {
            put(&[NAK]);
        } else if first && s.can_once.contains(&block) {
            put(&[CAN, b'x']);
        } else {
            put(&[ACK]);
        }
    }
    loop {
        match get(&mut rx, 10_000) {
            None | Some(b'2') => return rx,
            Some(_) => {}
        }
    }
}

/// radio_flash against the emulated bootloader on a pseudo-terminal: result, progress, the reset
/// / boot lines and the byte stream must be those of the C++ flasher (tests/golden/flash.txt).
fn flash_case(s: Script) {
    let (master, slave) = crate::sys::open_pty().unwrap();
    let lines = Lines::new();
    let mut l = lines.radio_lines();
    l.tty = slave;
    let done = Arc::new(AtomicBool::new(false));
    let emu = {
        let done = done.clone();
        let master = master.try_clone().unwrap();
        std::thread::spawn(move || emulate(&master, &s, &done))
    };
    let image = pattern(s.image_size, 31, 5);
    let mut progress = String::new();
    let result = radio_flash(&l, &image, &mut |p| progress += &format!(" {p:.6}"));
    done.store(true, Ordering::Relaxed);
    let rx = emu.join().unwrap();
    let (boot, reset) = lines.get();
    let out = [
        format!("flash {}", s.name),
        match result {
            Ok(()) => "result ok".into(),
            Err(e) => format!("result err {e}"),
        },
        format!("progress{progress}"),
        format!("lines boot={boot} reset={reset}"),
        format!("stream {}", hex(&rx)),
    ]
    .join("\n");
    let golden = include_str!("../../tests/golden/flash.txt");
    let start = golden
        .find(&format!("flash {}\n", s.name))
        .expect("scenario in the golden file");
    let block: Vec<&str> = golden[start..].lines().take(5).collect();
    assert_golden(&block.join("\n"), &out);
}

#[test]
fn flash_crc_with_retries() {
    flash_case(Script {
        name: "crc",
        image_size: 300,
        nak_once: &[2],
        can_once: &[3],
        eot_naks: 2,
        ..BASE
    });
}

#[test]
fn flash_checksum() {
    flash_case(Script {
        name: "checksum",
        image_size: 256,
        start: NAK,
        ..BASE
    });
}

#[test]
fn flash_cancelled_by_bootloader() {
    flash_case(Script {
        name: "cancel",
        image_size: 200,
        can_block: 1,
        ..BASE
    });
}

#[test]
fn flash_block_never_acknowledged() {
    flash_case(Script {
        name: "nak-all",
        image_size: 130,
        start: NAK,
        nak_always: 2,
        ..BASE
    });
}

#[test]
fn flash_end_never_acknowledged() {
    flash_case(Script {
        name: "eot-fail",
        eot_naks: 100,
        ..BASE
    });
}

#[test]
fn flash_without_bootloader_prompt() {
    flash_case(Script {
        name: "no-prompt",
        no_prompt: true,
        ..BASE
    });
}

#[test]
fn flash_without_xmodem_start() {
    flash_case(Script {
        name: "no-xmodem",
        no_xmodem: true,
        ..BASE
    });
}

#[test]
fn flash_refuses_undrivable_lines() {
    let (mut master, slave) = crate::sys::open_pty().unwrap();
    let l = RadioLines {
        tty: slave,
        reset: "/nonexistent/zb-reset/value".into(),
        boot: "/nonexistent/zb-boot/value".into(),
    };
    let err = radio_flash(&l, &[1, 2, 3], &mut |_| {}).unwrap_err();
    assert_eq!(err, "cannot drive the radio's reset/boot lines");
    // Nothing reached the radio: the closed slave reads as EIO (or end), never as data.
    let mut buf = [0u8; 16];
    assert!(!matches!(std::io::Read::read(&mut master, &mut buf), Ok(n) if n > 0));
}
