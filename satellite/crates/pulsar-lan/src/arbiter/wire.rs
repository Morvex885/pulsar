//! The claim datagram, one ASCII line: `pulsar-wake/1 <station> <wake> <busy 0|1> <level %.1f>`.
//! Parsing follows the C++ `sscanf("%15s %31s %15s %d %f")` field limits, so both sides accept
//! and reject the same datagrams (tests/golden/arbiter.txt).

use super::Claim;

const MAGIC: &[u8] = b"pulsar-wake/1";
/// The C++ arbiter formatted into char[128] and did not send longer claims.
const MAX_DATAGRAM: usize = 127;
const MAGIC_FIELD: usize = 15;
const STATION_FIELD: usize = 31;
const WAKE_FIELD: usize = 15;

/// printf("%.1f"): round half to even on the exact binary value, as glibc.
pub fn level_text(level: f32) -> String {
    if level.is_nan() {
        let sign = if level.is_sign_negative() { "-" } else { "" };
        return format!("{sign}nan");
    }
    format!("{level:.1}")
}

/// The datagram of a claim; None when it would not fit the C++ buffer.
pub fn format(c: &Claim) -> Option<String> {
    let d = format!(
        "pulsar-wake/1 {} {} {} {}",
        c.station,
        c.wake,
        u8::from(c.busy),
        level_text(c.level_db)
    );
    (d.len() <= MAX_DATAGRAM).then_some(d)
}

/// A claim from a datagram; None unless all five fields parse, the magic matches and the
/// level is within -100..0 dB (anything else would break the ordering). Text after the level
/// and after a NUL byte is ignored.
pub fn parse(datagram: &[u8]) -> Option<Claim> {
    let end = datagram
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(datagram.len());
    let mut s = Scan {
        s: &datagram[..end],
        pos: 0,
    };
    let magic = s.word(MAGIC_FIELD)?;
    let station = s.word(STATION_FIELD)?;
    let wake = s.word(WAKE_FIELD)?;
    let busy = s.int_nonzero()?;
    let level = s.float()?;
    if !level.is_finite() || !(-100.0..=0.0).contains(&level) || magic != MAGIC {
        return None;
    }
    Some(Claim {
        station: String::from_utf8_lossy(station).into_owned(),
        wake: String::from_utf8_lossy(wake).into_owned(),
        busy,
        level_db: level,
    })
}

/// isspace() in the C locale.
fn is_space(b: u8) -> bool {
    b == b' ' || (b'\t'..=b'\r').contains(&b)
}

/// scanf conversions over a NUL-free byte string: each one skips leading white space; a field
/// width cuts a %s word, and the next conversion continues right after it.
struct Scan<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Scan<'a> {
    fn skip_space(&mut self) {
        while self.s.get(self.pos).is_some_and(|&b| is_space(b)) {
            self.pos += 1;
        }
    }

    fn take_while(&mut self, f: impl Fn(u8) -> bool) -> usize {
        let start = self.pos;
        while self.s.get(self.pos).is_some_and(|&b| f(b)) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    /// %<width>s
    fn word(&mut self, width: usize) -> Option<&'a [u8]> {
        self.skip_space();
        let start = self.pos;
        while self.pos - start < width && self.peek().is_some_and(|b| !is_space(b)) {
            self.pos += 1;
        }
        (self.pos > start).then(|| &self.s[start..self.pos])
    }

    /// %d, reduced to whether the value is non-zero.
    fn int_nonzero(&mut self) -> Option<bool> {
        self.skip_space();
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let start = self.pos;
        let digits = self.take_while(|b| b.is_ascii_digit());
        (digits > 0).then(|| self.s[start..self.pos].iter().any(|&b| b != b'0'))
    }

    /// %f in decimal notation: the longest prefix that is a number, as strtof. Hexadecimal
    /// floats, which glibc would also take, count as no number (no station sends them);
    /// infinity and NaN are dropped by the caller's range check anyway.
    fn float(&mut self) -> Option<f32> {
        self.skip_space();
        let start = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let int_digits = self.take_while(|b| b.is_ascii_digit());
        if int_digits == 1
            && self.s[self.pos - 1] == b'0'
            && matches!(self.peek(), Some(b'x' | b'X'))
        {
            return None;
        }
        let mut frac_digits = 0;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            frac_digits = self.take_while(|b| b.is_ascii_digit());
        }
        if int_digits + frac_digits == 0 {
            return None;
        }
        let mantissa_end = self.pos;
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.take_while(|b| b.is_ascii_digit()) == 0 {
                // "1e", "1e+": the exponent is not part of the number.
                self.pos = mantissa_end;
            }
        }
        std::str::from_utf8(&self.s[start..self.pos])
            .ok()?
            .parse()
            .ok()
    }
}
