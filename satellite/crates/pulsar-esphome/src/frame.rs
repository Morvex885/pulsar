//! Framing of the native API. The first byte a client sends chooses it: 0x00 plaintext
//! (`[0x00][len varint][type varint][message]`), 0x01 Noise.
//!
//! ESPHome's Noise transport: frames are `[0x01][len BE16][payload]`. The first client frame is
//! a "client hello" (contents reserved, bound into the handshake through the prologue), the
//! server answers with a server hello, then one NNpsk0 round trip whose frames start with a
//! status byte (0 = ok, 1 = reject + reason text). Data frames carry an encrypted
//! `[type BE16][len BE16][message]`.

use std::ops::Range;

use snow::{HandshakeState, TransportState};

use crate::key::Psk;
use crate::proto::{get_varint, put_varint};

/// Larger plaintext frames are absurd: the client is dropped.
pub const MAX_FRAME: usize = 1 << 20;
pub const NOISE_PROTOCOL: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
const NOISE_MAC: usize = 16;
const NOISE_MAX_FRAME: usize = 0xffff;
/// Hello and handshake frames are short (NNpsk0: 48-byte messages); a longer one is refused.
const HANDSHAKE_MAX: usize = 128;
const DH_LEN: usize = 32;
/// "varint len + varint type" can take 20 bytes after the indicator; a header that does not
/// parse within them never will.
const PLAIN_HEADER_MAX: usize = 21;

#[derive(Debug, PartialEq, Eq)]
pub enum Plain<'a> {
    /// Wait for more bytes.
    Incomplete,
    BadPreamble,
    MalformedHeader,
    TooLarge,
    Frame {
        msg_type: u32,
        msg: &'a [u8],
        used: usize,
    },
}

/// Parses one plaintext frame at the start of `p` (non-empty).
pub fn parse_plain(p: &[u8]) -> Plain<'_> {
    if p[0] != 0x00 {
        return Plain::BadPreamble;
    }
    let mut q = 1;
    let (Some(len), Some(msg_type)) = (get_varint(p, &mut q), get_varint(p, &mut q)) else {
        return if p.len() > PLAIN_HEADER_MAX {
            Plain::MalformedHeader
        } else {
            Plain::Incomplete
        };
    };
    let Some(len) = usize::try_from(len).ok().filter(|&l| l <= MAX_FRAME) else {
        return Plain::TooLarge;
    };
    if p.len() - q < len {
        return Plain::Incomplete;
    }
    Plain::Frame {
        msg_type: msg_type as u32,
        msg: &p[q..q + len],
        used: q + len,
    }
}

pub fn append_plain(out: &mut Vec<u8>, msg_type: u32, msg: &[u8]) {
    out.push(0x00);
    put_varint(out, msg.len() as u64);
    put_varint(out, u64::from(msg_type));
    out.extend_from_slice(msg);
}

#[derive(Debug, PartialEq, Eq)]
pub enum NoiseFrame<'a> {
    Incomplete,
    BadIndicator,
    BadHandshakeLen,
    Frame { body: &'a [u8], used: usize },
}

/// Parses one Noise frame at the start of `p` (non-empty).
pub fn parse_noise(p: &[u8], in_handshake: bool) -> NoiseFrame<'_> {
    if p[0] != 0x01 {
        return NoiseFrame::BadIndicator;
    }
    if p.len() < 3 {
        return NoiseFrame::Incomplete;
    }
    let len = usize::from(u16::from_be_bytes([p[1], p[2]]));
    if in_handshake && len > HANDSHAKE_MAX {
        return NoiseFrame::BadHandshakeLen;
    }
    if p.len() - 3 < len {
        return NoiseFrame::Incomplete;
    }
    NoiseFrame::Frame {
        body: &p[3..3 + len],
        used: 3 + len,
    }
}

/// A frame outside the cipher: hellos, handshake messages, rejections.
pub fn append_noise_raw(out: &mut Vec<u8>, payload: &[&[u8]]) {
    let len: usize = payload.iter().map(|p| p.len()).sum();
    out.push(0x01);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    for p in payload {
        out.extend_from_slice(p);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// Reject with this text.
    Reject(&'static str),
    /// The client's handshake message did not verify: a wrong key (`mac`) or a protocol error.
    ReadFailed {
        mac: bool,
    },
    SplitFailed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EncryptError {
    /// The handshake has not finished: the message is dropped.
    NotReady,
    TooLarge,
    Failed,
}

/// The responder side of one connection.
pub enum Noise {
    ClientHello,
    Handshake(Box<HandshakeState>),
    Data(Box<TransportState>),
}

impl Noise {
    pub fn in_handshake(&self) -> bool {
        !matches!(self, Noise::Data(_))
    }

    /// Client hello: queues the server hello (chosen protocol 0x01, node name and MAC, which
    /// aioesphomeapi checks against what HA expects before the handshake) and prepares the
    /// NNpsk0 responder with the prologue "NoiseAPIInit" + BE16 length + the client hello.
    pub fn start(
        &mut self,
        client_hello: &[u8],
        psk: &Psk,
        name: &str,
        mac: &str,
        out: &mut Vec<u8>,
    ) -> Result<(), snow::Error> {
        append_noise_raw(
            out,
            &[b"\x01", name.as_bytes(), b"\0", mac.as_bytes(), b"\0"],
        );
        let mut prologue = Vec::with_capacity(14 + client_hello.len());
        prologue.extend_from_slice(b"NoiseAPIInit");
        prologue.extend_from_slice(&(client_hello.len() as u16).to_be_bytes());
        prologue.extend_from_slice(client_hello);
        let hs = snow::Builder::new(NOISE_PROTOCOL.parse()?)
            .psk(0, psk)?
            .prologue(&prologue)?
            .build_responder()?;
        *self = Noise::Handshake(Box::new(hs));
        Ok(())
    }

    /// The initiator's `psk, e` message (after its 0x00 status byte); queues our `e, ee` and
    /// switches to transport mode.
    pub fn handshake(&mut self, body: &[u8], out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        let Noise::Handshake(hs) = self else {
            return Err(HandshakeError::Reject("Handshake error"));
        };
        if hs.is_my_turn() {
            return Err(HandshakeError::Reject("Handshake error"));
        }
        let Some((&status, msg)) = body.split_first() else {
            return Err(HandshakeError::Reject("Empty handshake message"));
        };
        if status != 0x00 {
            return Err(HandshakeError::Reject("Bad handshake error byte"));
        }
        // noise-c (the C++ server) refused a truncated message and a null ephemeral key before
        // any MAC check: those are protocol errors, not a wrong key. snow would report the
        // first as a decryption failure and accept the second.
        if msg.len() < DH_LEN + NOISE_MAC || msg[..DH_LEN].iter().all(|&b| b == 0) {
            return Err(HandshakeError::ReadFailed { mac: false });
        }
        let mut payload = [0u8; HANDSHAKE_MAX];
        if let Err(e) = hs.read_message(msg, &mut payload) {
            return Err(HandshakeError::ReadFailed {
                mac: matches!(e, snow::Error::Decrypt),
            });
        }
        if !hs.is_my_turn() {
            return Err(HandshakeError::Reject("Handshake error"));
        }
        let mut reply = [0u8; HANDSHAKE_MAX];
        let n = match hs.write_message(&[], &mut reply[1..]) {
            Ok(n) if hs.is_handshake_finished() => n,
            _ => return Err(HandshakeError::Reject("Handshake error")),
        };
        append_noise_raw(out, &[&reply[..n + 1]]);
        let Noise::Handshake(hs) = std::mem::replace(self, Noise::ClientHello) else {
            unreachable!("checked above");
        };
        let ts = hs
            .into_transport_mode()
            .map_err(|_| HandshakeError::SplitFailed)?;
        *self = Noise::Data(Box::new(ts));
        Ok(())
    }

    /// Decrypts a data frame into `buf`; returns the message type and the message's range in
    /// `buf`, or the reason to drop the client.
    pub fn decrypt(
        &mut self,
        body: &[u8],
        buf: &mut Vec<u8>,
    ) -> Result<(u32, Range<usize>), &'static str> {
        let Noise::Data(ts) = self else {
            return Err("noise decrypt failed");
        };
        if buf.len() < body.len() {
            buf.resize(body.len(), 0);
        }
        let n = ts
            .read_message(body, &mut buf[..body.len()])
            .map_err(|_| "noise decrypt failed")?;
        if n < 4 {
            return Err("short noise message");
        }
        let msg_type = u32::from(u16::from_be_bytes([buf[0], buf[1]]));
        let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
        if len > n - 4 {
            return Err("bad noise message length");
        }
        Ok((msg_type, 4..4 + len))
    }

    /// Appends one encrypted data frame; `scratch` holds the plaintext meanwhile.
    pub fn append_encrypted(
        &mut self,
        out: &mut Vec<u8>,
        msg_type: u32,
        msg: &[u8],
        scratch: &mut Vec<u8>,
    ) -> Result<(), EncryptError> {
        let Noise::Data(ts) = self else {
            return Err(EncryptError::NotReady);
        };
        let body = 4 + msg.len();
        if body + NOISE_MAC > NOISE_MAX_FRAME {
            return Err(EncryptError::TooLarge);
        }
        scratch.clear();
        scratch.extend_from_slice(&(msg_type as u16).to_be_bytes());
        scratch.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        scratch.extend_from_slice(msg);
        let start = out.len();
        out.resize(start + 3 + body + NOISE_MAC, 0);
        match ts.write_message(scratch, &mut out[start + 3..]) {
            Ok(n) => {
                out[start] = 0x01;
                out[start + 1..start + 3].copy_from_slice(&(n as u16).to_be_bytes());
                out.truncate(start + 3 + n);
                Ok(())
            }
            Err(_) => {
                out.truncate(start);
                Err(EncryptError::Failed)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const NAME: &str = "yandex-midi-test";
    pub const MAC: &str = "aabbccddeeff";

    /// The client side of ESPHome's Noise transport (aioesphomeapi's role), on snow.
    pub struct Initiator {
        pub hs: Option<HandshakeState>,
        pub ts: Option<TransportState>,
    }

    impl Initiator {
        /// Client hello (empty) + the first handshake message, as aioesphomeapi sends them.
        pub fn new(psk: &Psk) -> (Self, Vec<u8>) {
            let mut prologue = b"NoiseAPIInit".to_vec();
            prologue.extend_from_slice(&[0, 0]);
            let mut hs = snow::Builder::new(NOISE_PROTOCOL.parse().unwrap())
                .psk(0, psk)
                .unwrap()
                .prologue(&prologue)
                .unwrap()
                .build_initiator()
                .unwrap();
            let mut msg = [0u8; 128];
            msg[0] = 0x00;
            let n = hs.write_message(&[], &mut msg[1..]).unwrap();
            let mut out = Vec::new();
            append_noise_raw(&mut out, &[]);
            append_noise_raw(&mut out, &[&msg[..n + 1]]);
            (
                Self {
                    hs: Some(hs),
                    ts: None,
                },
                out,
            )
        }

        /// Reads the server's handshake message (status byte first).
        pub fn finish(&mut self, body: &[u8]) -> Result<(), String> {
            if body.first() != Some(&0x00) {
                return Err(String::from_utf8_lossy(&body[1..]).into_owned());
            }
            let mut hs = self.hs.take().unwrap();
            let mut payload = [0u8; 128];
            hs.read_message(&body[1..], &mut payload)
                .map_err(|e| e.to_string())?;
            self.ts = Some(hs.into_transport_mode().unwrap());
            Ok(())
        }

        pub fn encrypt(&mut self, msg_type: u16, msg: &[u8]) -> Vec<u8> {
            let mut plain = msg_type.to_be_bytes().to_vec();
            plain.extend_from_slice(&(msg.len() as u16).to_be_bytes());
            plain.extend_from_slice(msg);
            let mut ct = vec![0u8; plain.len() + 16];
            let n = self
                .ts
                .as_mut()
                .unwrap()
                .write_message(&plain, &mut ct)
                .unwrap();
            let mut out = Vec::new();
            append_noise_raw(&mut out, &[&ct[..n]]);
            out
        }

        pub fn decrypt(&mut self, body: &[u8]) -> (u16, Vec<u8>) {
            let mut plain = vec![0u8; body.len()];
            let n = self
                .ts
                .as_mut()
                .unwrap()
                .read_message(body, &mut plain)
                .unwrap();
            let t = u16::from_be_bytes([plain[0], plain[1]]);
            let len = usize::from(u16::from_be_bytes([plain[2], plain[3]]));
            assert_eq!(len, n - 4);
            (t, plain[4..n].to_vec())
        }
    }

    /// Splits a byte stream of Noise frames.
    pub fn noise_bodies(mut p: &[u8]) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        while !p.is_empty() {
            let NoiseFrame::Frame { body, used } = parse_noise(p, false) else {
                panic!("bad frame {p:?}");
            };
            v.push(body.to_vec());
            p = &p[used..];
        }
        v
    }

    fn server_frames(psk: &Psk, input: &[u8]) -> (Noise, Vec<u8>, Result<(), HandshakeError>) {
        let mut noise = Noise::ClientHello;
        let mut out = Vec::new();
        let NoiseFrame::Frame { body, used } = parse_noise(input, true) else {
            panic!()
        };
        noise.start(body, psk, NAME, MAC, &mut out).unwrap();
        let NoiseFrame::Frame { body, .. } = parse_noise(&input[used..], true) else {
            panic!()
        };
        let r = noise.handshake(body, &mut out);
        (noise, out, r)
    }

    #[test]
    fn plain_frames() {
        let mut out = Vec::new();
        append_plain(&mut out, 1, b"hi");
        append_plain(&mut out, 300, &[7u8; 200]);
        assert_eq!(&out[..5], b"\x00\x02\x01hi");
        let Plain::Frame {
            msg_type: 1,
            msg: b"hi",
            used: 5,
        } = parse_plain(&out)
        else {
            panic!("{:?}", parse_plain(&out));
        };
        let second = &out[5..];
        let Plain::Frame {
            msg_type: 300,
            msg,
            used,
        } = parse_plain(second)
        else {
            panic!();
        };
        assert_eq!((msg.len(), used), (200, second.len()));
        // Every truncation of a valid frame waits for more bytes.
        for n in 1..second.len() {
            assert_eq!(parse_plain(&second[..n]), Plain::Incomplete, "{n}");
        }
        assert_eq!(parse_plain(b"\x01\x00\x00"), Plain::BadPreamble);
        assert_eq!(parse_plain(b"\x07"), Plain::BadPreamble);
    }

    #[test]
    fn plain_header_limits() {
        // 1 MiB is accepted (waiting for the body), one byte more is not.
        let mut p = vec![0u8];
        put_varint(&mut p, MAX_FRAME as u64);
        put_varint(&mut p, 9);
        assert_eq!(parse_plain(&p), Plain::Incomplete);
        let mut p = vec![0u8];
        put_varint(&mut p, MAX_FRAME as u64 + 1);
        put_varint(&mut p, 9);
        assert_eq!(parse_plain(&p), Plain::TooLarge);
        let mut p = vec![0u8];
        put_varint(&mut p, u64::MAX);
        put_varint(&mut p, 9);
        assert_eq!(parse_plain(&p), Plain::TooLarge);
        // A header that never terminates: incomplete up to 21 bytes, malformed after.
        let mut p = vec![0u8];
        p.extend_from_slice(&[0x80; 20]);
        assert_eq!(parse_plain(&p), Plain::Incomplete);
        p.push(0x80);
        assert_eq!(parse_plain(&p), Plain::MalformedHeader);
        // An 11-byte varint is malformed even when it ends.
        let mut p = vec![0u8, 1];
        p.extend_from_slice(&[0xff; 10]);
        p.extend_from_slice(&[0x01; 10]);
        assert_eq!(parse_plain(&p), Plain::MalformedHeader);
    }

    #[test]
    fn noise_frame_limits() {
        assert_eq!(parse_noise(b"\x00", true), NoiseFrame::BadIndicator);
        assert_eq!(parse_noise(b"\x01\x00", true), NoiseFrame::Incomplete);
        assert_eq!(
            parse_noise(b"\x01\x00\x81", true),
            NoiseFrame::BadHandshakeLen
        );
        assert_eq!(parse_noise(b"\x01\x00\x81", false), NoiseFrame::Incomplete);
        assert_eq!(parse_noise(b"\x01\x00\x02a", true), NoiseFrame::Incomplete);
        assert_eq!(
            parse_noise(b"\x01\x00\x02abc", true),
            NoiseFrame::Frame {
                body: b"ab",
                used: 5
            }
        );
        let mut big = vec![0x01, 0xff, 0xff];
        big.resize(3 + 0xffff, 0);
        assert!(matches!(
            parse_noise(&big, false),
            NoiseFrame::Frame { used: 65538, .. }
        ));
    }

    #[test]
    fn noise_handshake_and_data_both_ways() {
        let psk: Psk = std::array::from_fn(|i| i as u8 * 7);
        let (mut client, hello) = Initiator::new(&psk);
        let (mut noise, out, r) = server_frames(&psk, &hello);
        r.unwrap();
        let bodies = noise_bodies(&out);
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], format!("\x01{NAME}\0{MAC}\0").as_bytes());
        assert_eq!(bodies[1].len(), 1 + 48);
        client.finish(&bodies[1]).unwrap();
        assert!(!noise.in_handshake());

        let frame = client.encrypt(7, b"");
        let NoiseFrame::Frame { body, .. } = parse_noise(&frame, false) else {
            panic!()
        };
        let mut buf = Vec::new();
        let (t, range) = noise.decrypt(body, &mut buf).unwrap();
        assert_eq!((t, &buf[range]), (7, &b""[..]));
        let frame = client.encrypt(1, b"\x0a\x03abc");
        let NoiseFrame::Frame { body, .. } = parse_noise(&frame, false) else {
            panic!()
        };
        let (t, range) = noise.decrypt(body, &mut buf).unwrap();
        assert_eq!((t, &buf[range]), (1, &b"\x0a\x03abc"[..]));

        let mut out = Vec::new();
        let mut scratch = Vec::new();
        noise
            .append_encrypted(&mut out, 90, b"\x08\x01", &mut scratch)
            .unwrap();
        let big = vec![5u8; NOISE_MAX_FRAME - 4 - NOISE_MAC];
        noise
            .append_encrypted(&mut out, 106, &big, &mut scratch)
            .unwrap();
        assert_eq!(
            noise.append_encrypted(
                &mut out,
                106,
                &[0; NOISE_MAX_FRAME - 4 - NOISE_MAC + 1],
                &mut scratch
            ),
            Err(EncryptError::TooLarge)
        );
        let bodies = noise_bodies(&out);
        assert_eq!(bodies.len(), 2);
        assert_eq!(client.decrypt(&bodies[0]), (90, b"\x08\x01".to_vec()));
        assert_eq!(client.decrypt(&bodies[1]), (106, big));

        // A tampered frame and a short one drop the client.
        let mut frame = client.encrypt(7, b"");
        let last = frame.len() - 1;
        frame[last] ^= 1;
        assert_eq!(
            noise.decrypt(&frame[3..], &mut buf),
            Err("noise decrypt failed")
        );
    }

    #[test]
    fn noise_data_length_checks() {
        let psk = [3u8; 32];
        let (mut client, hello) = Initiator::new(&psk);
        let (mut noise, out, r) = server_frames(&psk, &hello);
        r.unwrap();
        client.finish(&noise_bodies(&out)[1]).unwrap();
        let mut buf = Vec::new();
        // Three plaintext bytes: no room for type and length.
        let ts = client.ts.as_mut().unwrap();
        let mut ct = [0u8; 64];
        let n = ts.write_message(b"\x00\x07\x00", &mut ct).unwrap();
        assert_eq!(
            noise.decrypt(&ct[..n], &mut buf),
            Err("short noise message")
        );
        // A length past the decrypted bytes.
        let n = ts.write_message(b"\x00\x07\x00\x02a", &mut ct).unwrap();
        assert_eq!(
            noise.decrypt(&ct[..n], &mut buf),
            Err("bad noise message length")
        );
        // Trailing bytes after the declared length are ignored.
        let n = ts.write_message(b"\x00\x07\x00\x01ab", &mut ct).unwrap();
        let (t, range) = noise.decrypt(&ct[..n], &mut buf).unwrap();
        assert_eq!((t, &buf[range]), (7, &b"a"[..]));
        assert_eq!(
            noise.decrypt(&ct[..10], &mut buf),
            Err("noise decrypt failed")
        );
    }

    #[test]
    fn wrong_psk_is_a_mac_failure() {
        let (_, hello) = Initiator::new(&[1u8; 32]);
        let (noise, out, r) = server_frames(&[2u8; 32], &hello);
        assert_eq!(r, Err(HandshakeError::ReadFailed { mac: true }));
        assert!(noise.in_handshake());
        // Only the server hello went out.
        assert_eq!(noise_bodies(&out).len(), 1);
    }

    #[test]
    fn handshake_rejections() {
        let psk = [0u8; 32];
        let check = |body: &[u8]| {
            let mut noise = Noise::ClientHello;
            let mut out = Vec::new();
            noise.start(&[], &psk, NAME, MAC, &mut out).unwrap();
            noise.handshake(body, &mut out)
        };
        assert_eq!(
            check(b""),
            Err(HandshakeError::Reject("Empty handshake message"))
        );
        assert_eq!(
            check(b"\x01abc"),
            Err(HandshakeError::Reject("Bad handshake error byte"))
        );
        // Truncated messages and a null ephemeral key are protocol errors, as in noise-c.
        assert_eq!(
            check(&[0u8; 1 + 31]),
            Err(HandshakeError::ReadFailed { mac: false })
        );
        let mut msg = vec![0u8];
        msg.extend_from_slice(&[9u8; 32 + 15]);
        assert_eq!(check(&msg), Err(HandshakeError::ReadFailed { mac: false }));
        let mut msg = vec![0u8];
        msg.extend_from_slice(&[0u8; 32 + 16]);
        assert_eq!(check(&msg), Err(HandshakeError::ReadFailed { mac: false }));
        // Garbage of the right length fails the MAC.
        let mut msg = vec![0u8];
        msg.extend_from_slice(&[9u8; 32 + 16]);
        assert_eq!(check(&msg), Err(HandshakeError::ReadFailed { mac: true }));
        // Before the client hello.
        let mut out = Vec::new();
        assert_eq!(
            Noise::ClientHello.handshake(b"\x00", &mut out),
            Err(HandshakeError::Reject("Handshake error"))
        );
    }

    #[test]
    fn prologue_binds_the_client_hello() {
        // The initiator's prologue assumes an empty client hello; a server that saw another
        // one derives other keys.
        let psk = [4u8; 32];
        let (_, hello) = Initiator::new(&psk);
        let mut noise = Noise::ClientHello;
        let mut out = Vec::new();
        noise.start(b"x", &psk, NAME, MAC, &mut out).unwrap();
        let NoiseFrame::Frame { body, .. } = parse_noise(&hello[3..], true) else {
            panic!()
        };
        assert_eq!(
            noise.handshake(body, &mut out),
            Err(HandshakeError::ReadFailed { mac: true })
        );
    }
}
