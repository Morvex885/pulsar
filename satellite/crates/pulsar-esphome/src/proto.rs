//! Minimal hand-written protobuf (proto3) wire codec for the ESPHome native API.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireType {
    Varint = 0,
    Fixed64 = 1,
    Len = 2,
    Fixed32 = 5,
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Decodes a varint at `p[*pos..]`, advancing `pos`. None if truncated or longer than 10 bytes
/// (bits past the 64th are dropped, as protobuf does).
pub fn get_varint(p: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    while shift < 70 && *pos < p.len() {
        let b = p[*pos];
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
    }
    None
}

/// sint32 (zigzag) from a decoded varint; bits above 32 are ignored.
pub fn zigzag32(v: u64) -> i32 {
    ((v >> 1) as u32 ^ (v as u32 & 1).wrapping_neg()) as i32
}

/// Encoder. Proto3 semantics: scalar fields equal to their default are omitted unless forced.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn data(&self) -> &[u8] {
        &self.buf
    }

    pub fn tag(&mut self, field: u32, wt: WireType) {
        put_varint(&mut self.buf, (u64::from(field) << 3) | wt as u64);
    }

    pub fn u32(&mut self, field: u32, v: u32, force: bool) {
        if v == 0 && !force {
            return;
        }
        self.tag(field, WireType::Varint);
        put_varint(&mut self.buf, u64::from(v));
    }

    pub fn i32(&mut self, field: u32, v: i32, force: bool) {
        if v == 0 && !force {
            return;
        }
        self.tag(field, WireType::Varint);
        // A negative int32 is sign-extended to 10 bytes.
        put_varint(&mut self.buf, i64::from(v) as u64);
    }

    pub fn boolean(&mut self, field: u32, v: bool, force: bool) {
        if !v && !force {
            return;
        }
        self.tag(field, WireType::Varint);
        self.buf.push(u8::from(v));
    }

    pub fn fixed32(&mut self, field: u32, v: u32, force: bool) {
        if v == 0 && !force {
            return;
        }
        self.tag(field, WireType::Fixed32);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// -0.0 counts as the default (it compares equal to 0.0), NaN does not.
    pub fn flt(&mut self, field: u32, v: f32, force: bool) {
        if v == 0.0 && !force {
            return;
        }
        self.fixed32(field, v.to_bits(), true);
    }

    pub fn bytes(&mut self, field: u32, p: &[u8], force: bool) {
        if p.is_empty() && !force {
            return;
        }
        self.tag(field, WireType::Len);
        put_varint(&mut self.buf, p.len() as u64);
        self.buf.extend_from_slice(p);
    }

    pub fn str(&mut self, field: u32, s: &str, force: bool) {
        self.bytes(field, s.as_bytes(), force);
    }

    pub fn msg(&mut self, field: u32, sub: &Writer) {
        self.bytes(field, &sub.buf, true);
    }

    /// 16-bit samples as little-endian bytes (VoiceAssistantAudio data), without an
    /// intermediate byte buffer.
    pub fn pcm16(&mut self, field: u32, pcm: &[i16]) {
        if pcm.is_empty() {
            return;
        }
        self.tag(field, WireType::Len);
        put_varint(&mut self.buf, (pcm.len() * 2) as u64);
        self.buf.reserve(pcm.len() * 2);
        for s in pcm {
            self.buf.extend_from_slice(&s.to_le_bytes());
        }
    }

    /// Packed repeated varints (canonical proto3 encoding for repeated enums / uint32).
    pub fn packed_u32(&mut self, field: u32, vals: &[u32]) {
        if vals.is_empty() {
            return;
        }
        let mut tmp = Vec::new();
        for &v in vals {
            put_varint(&mut tmp, u64::from(v));
        }
        self.bytes(field, &tmp, true);
    }
}

/// One decoded field. For LEN fields `data` points into the reader's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field<'a> {
    pub num: u32,
    pub wt: WireType,
    /// VARINT / FIXED32 / FIXED64 value.
    pub v: u64,
    pub data: &'a [u8],
}

impl Field<'_> {
    pub fn u32(&self) -> u32 {
        self.v as u32
    }

    pub fn boolean(&self) -> bool {
        self.v != 0
    }

    pub fn flt(&self) -> f32 {
        f32::from_bits(self.v as u32)
    }

    /// The C++ server copied the bytes as they came; invalid UTF-8 is replaced here.
    pub fn str(&self) -> String {
        String::from_utf8_lossy(self.data).into_owned()
    }
}

pub struct Reader<'a> {
    p: &'a [u8],
    pos: usize,
    err: bool,
}

impl<'a> Reader<'a> {
    pub fn new(p: &'a [u8]) -> Self {
        Self {
            p,
            pos: 0,
            err: false,
        }
    }

    /// None at the end of the buffer or on malformed input (then `error()` is true and every
    /// later call returns None).
    pub fn next(&mut self) -> Option<Field<'a>> {
        if self.pos >= self.p.len() {
            return None;
        }
        let Some(key) = get_varint(self.p, &mut self.pos) else {
            return self.fail();
        };
        let num = (key >> 3) as u32;
        let rest = self.p.len() - self.pos;
        let mut f = Field {
            num,
            wt: WireType::Varint,
            v: 0,
            data: &[],
        };
        match key & 7 {
            0 => match get_varint(self.p, &mut self.pos) {
                Some(v) => f.v = v,
                None => return self.fail(),
            },
            1 => {
                if rest < 8 {
                    return self.fail();
                }
                f.wt = WireType::Fixed64;
                f.v = u64::from_le_bytes(self.take(8).try_into().unwrap_or_default());
            }
            5 => {
                if rest < 4 {
                    return self.fail();
                }
                f.wt = WireType::Fixed32;
                f.v = u64::from(u32::from_le_bytes(
                    self.take(4).try_into().unwrap_or_default(),
                ));
            }
            2 => {
                let Some(len) = get_varint(self.p, &mut self.pos) else {
                    return self.fail();
                };
                let Ok(len) = usize::try_from(len) else {
                    return self.fail();
                };
                if len > self.p.len() - self.pos {
                    return self.fail();
                }
                f.wt = WireType::Len;
                f.data = self.take(len);
            }
            _ => return self.fail(),
        }
        Some(f)
    }

    pub fn error(&self) -> bool {
        self.err
    }

    fn take(&mut self, n: usize) -> &'a [u8] {
        let s = &self.p[self.pos..self.pos + n];
        self.pos += n;
        s
    }

    fn fail(&mut self) -> Option<Field<'a>> {
        self.err = true;
        self.pos = self.p.len();
        None
    }
}

#[cfg(test)]
pub(crate) mod golden {
    //! Golden vectors of the C++ reference, `tests/golden/proto.txt` (see its header).

    pub const PROTO: &str = include_str!("../tests/golden/proto.txt");

    pub fn unhex(s: &str) -> Vec<u8> {
        if s == "-" {
            return Vec::new();
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    pub fn hex(b: &[u8]) -> String {
        if b.is_empty() {
            return "-".into();
        }
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Lines of one kind, split into words after the kind.
    pub fn cases(kind: &str) -> Vec<Vec<&'static str>> {
        let v: Vec<Vec<&str>> = PROTO
            .lines()
            .filter_map(|l| {
                let mut w = l.split(' ');
                (w.next() == Some(kind)).then(|| w.collect())
            })
            .collect();
        assert!(!v.is_empty(), "no golden cases for {kind}");
        v
    }
}

#[cfg(test)]
mod tests {
    use super::golden::{cases, hex, unhex};
    use super::*;

    fn flag(s: &str) -> bool {
        s == "1"
    }

    #[test]
    fn varint_golden() {
        for c in cases("varint") {
            let mut out = Vec::new();
            put_varint(&mut out, c[0].parse().unwrap());
            assert_eq!(hex(&out), c[1], "put_varint {}", c[0]);
        }
        for c in cases("get_varint") {
            let input = unhex(c[0]);
            let mut pos = 0;
            let got = match get_varint(&input, &mut pos) {
                Some(v) => format!("ok {v} {pos}"),
                None => format!("fail {pos}"),
            };
            assert_eq!(got, c[1..].join(" "), "get_varint {}", c[0]);
        }
    }

    #[test]
    fn writer_golden() {
        let check = |kind: &str, f: &dyn Fn(&mut Writer, &[&str])| {
            for c in cases(kind) {
                let mut w = Writer::new();
                f(&mut w, &c);
                assert_eq!(hex(w.data()), *c.last().unwrap(), "{kind} {c:?}");
            }
        };
        check("u32", &|w, c| {
            w.u32(c[0].parse().unwrap(), c[1].parse().unwrap(), flag(c[2]))
        });
        check("i32", &|w, c| {
            w.i32(c[0].parse().unwrap(), c[1].parse().unwrap(), flag(c[2]))
        });
        check("bool", &|w, c| {
            w.boolean(c[0].parse().unwrap(), flag(c[1]), flag(c[2]))
        });
        check("fixed32", &|w, c| {
            w.fixed32(c[0].parse().unwrap(), c[1].parse().unwrap(), flag(c[2]))
        });
        check("flt", &|w, c| {
            let bits = u32::from_str_radix(c[1], 16).unwrap();
            w.flt(c[0].parse().unwrap(), f32::from_bits(bits), flag(c[2]))
        });
        check("bytes", &|w, c| {
            w.bytes(c[0].parse().unwrap(), &unhex(c[1]), flag(c[2]))
        });
        check("msg", &|w, c| {
            let mut sub = Writer::new();
            sub.buf = unhex(c[1]);
            w.msg(c[0].parse().unwrap(), &sub)
        });
        check("packed", &|w, c| {
            let vals: Vec<u32> = if c[1] == "-" {
                Vec::new()
            } else {
                c[1].split(',').map(|v| v.parse().unwrap()).collect()
            };
            w.packed_u32(c[0].parse().unwrap(), &vals)
        });
    }

    #[test]
    fn str_is_bytes() {
        let mut a = Writer::new();
        let mut b = Writer::new();
        a.str(3, "Алиса", false);
        b.bytes(3, "Алиса".as_bytes(), false);
        assert_eq!(a.data(), b.data());
    }

    #[test]
    fn pcm16_is_le_bytes() {
        let pcm = [0i16, 1, -1, 0x1234, i16::MIN];
        let le: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut a = Writer::new();
        let mut b = Writer::new();
        a.pcm16(1, &pcm);
        b.bytes(1, &le, false);
        assert_eq!(a.data(), b.data());
        let mut e = Writer::new();
        e.pcm16(1, &[]);
        assert!(e.data().is_empty());
    }

    #[test]
    fn reader_golden_including_broken_input() {
        for c in cases("reader") {
            let input = unhex(c[0]);
            let mut r = Reader::new(&input);
            let mut out = String::new();
            while let Some(f) = r.next() {
                let data = if f.wt == WireType::Len {
                    hex(f.data)
                } else {
                    "-".into()
                };
                out += &format!("{}:{}:{}:{data} ", f.num, f.wt as u8, f.v);
            }
            let again = r.next().is_some();
            out += &format!("err={} again={}", u8::from(r.error()), u8::from(again));
            assert_eq!(out, c[1..].join(" "), "reader {}", c[0]);
        }
    }

    #[test]
    fn field_accessors() {
        let mut w = Writer::new();
        w.flt(1, -2.5, false);
        w.u32(2, 7, false);
        w.boolean(3, true, false);
        w.str(4, "tz", false);
        let mut r = Reader::new(w.data());
        assert_eq!(r.next().unwrap().flt(), -2.5);
        assert_eq!(r.next().unwrap().u32(), 7);
        assert!(r.next().unwrap().boolean());
        assert_eq!(r.next().unwrap().str(), "tz");
        assert!(r.next().is_none() && !r.error());
    }

    #[test]
    fn zigzag_golden() {
        for c in cases("zigzag") {
            assert_eq!(
                zigzag32(c[0].parse().unwrap()),
                c[1].parse::<i32>().unwrap(),
                "zigzag {}",
                c[0]
            );
        }
    }
}
