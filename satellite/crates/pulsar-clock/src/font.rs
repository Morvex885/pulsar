//! Glyphs of the 7-segment clock (bit i = segment 'a' + i), text layout and the static content
//! drawn from glyphs: numbers, the time, timer countdowns, pairing codes, faces.

pub const DIGITS: usize = 4;

/// One glyph per digit position, left to right.
pub type Glyphs = [u8; DIGITS];

const FONT: [u8; 10] = [
    0x3f, // 0 abcdef
    0x06, // 1 bc
    0x5b, // 2 abdeg
    0x4f, // 3 abcdg
    0x66, // 4 bcfg
    0x6d, // 5 acdfg
    0x7d, // 6 acdefg
    0x07, // 7 abc
    0x7f, // 8 abcdefg
    0x6f, // 9 abcdfg
];
const MINUS: u8 = 0x40; // g

/// Printable ASCII 0x20..0x7E, from github.com/dmadison/LED-Segment-ASCII
/// 7-Segment-ASCII_HEX.txt (MIT, Copyright (c) 2017 David Madison); decimal point bits dropped,
/// '.' drawn like ','.
#[rustfmt::skip]
const ASCII: [u8; 95] = [
    0x00, 0x06, 0x22, 0x7E, 0x6D, 0x52, 0x46, 0x20, 0x29, 0x0B, 0x21, 0x70, 0x10, 0x40, 0x10, 0x52, // ' '../
    0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F, 0x09, 0x0D, 0x61, 0x48, 0x43, 0x53, // 0..?
    0x5F, 0x77, 0x7C, 0x39, 0x5E, 0x79, 0x71, 0x3D, 0x76, 0x30, 0x1E, 0x75, 0x38, 0x15, 0x37, 0x3F, // @..O
    0x73, 0x6B, 0x33, 0x6D, 0x78, 0x3E, 0x3E, 0x2A, 0x76, 0x6E, 0x5B, 0x39, 0x64, 0x0F, 0x23, 0x08, // P.._
    0x02, 0x5F, 0x7C, 0x58, 0x5E, 0x7B, 0x71, 0x6F, 0x74, 0x10, 0x0C, 0x75, 0x30, 0x14, 0x54, 0x5C, // `..o
    0x73, 0x67, 0x50, 0x6D, 0x78, 0x1C, 0x1C, 0x14, 0x76, 0x6E, 0x5B, 0x46, 0x30, 0x70, 0x01,       // p..~
];

/// Cyrillic А..Я approximations; lower case uses the same glyphs.
#[rustfmt::skip]
const CYRILLIC: [u8; 32] = [
    0x77, 0x7D, 0x7F, 0x31, 0x5E, 0x79, 0x76, 0x4F, // А Б В Г Д Е Ж З
    0x3E, 0x3E, 0x75, 0x37, 0x15, 0x76, 0x3F, 0x37, // И Й К Л М Н О П
    0x73, 0x39, 0x78, 0x6E, 0x71, 0x76, 0x3E, 0x66, // Р С Т У Ф Х Ц Ч
    0x2A, 0x2A, 0x7C, 0x7C, 0x7C, 0x4F, 0x3F, 0x67, // Ш Щ Ъ Ы Ь Э Ю Я
];

fn glyph_for(cp: u32) -> u8 {
    match cp {
        0x20..=0x7E => ASCII[(cp - 0x20) as usize] & 0x7F,
        0x410..=0x44F => CYRILLIC[((cp - 0x410) % 32) as usize],
        0x401 | 0x451 => CYRILLIC[5], // Ё ё
        0xB0 => 0x63,                 // °
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub glyph: u8,
    pub colon_after: bool,
}

/// One cell per code point; ':' lights the colon after the previous cell. Malformed UTF-8
/// bytes become blank cells, one per byte (the decoder takes any well-framed sequence as is,
/// overlong forms and surrogates included).
pub fn layout_text(utf8: &[u8]) -> Vec<Cell> {
    let mut cells: Vec<Cell> = Vec::new();
    let n = utf8.len();
    let mut i = 0;
    while i < n {
        let b = utf8[i];
        let mut cp = 0xFFFD;
        let mut len = if b < 0x80 {
            1
        } else if b >> 5 == 0x6 {
            2
        } else if b >> 4 == 0xE {
            3
        } else if b >> 3 == 0x1E {
            4
        } else {
            0
        };
        if len == 1 {
            cp = u32::from(b);
        } else if len != 0 && i + len <= n {
            let mut v = u32::from(b & (0x7F >> len));
            let mut k = 1;
            while k < len && utf8[i + k] & 0xC0 == 0x80 {
                v = v << 6 | u32::from(utf8[i + k] & 0x3F);
                k += 1;
            }
            if k == len {
                cp = v;
            } else {
                len = 0;
            }
        } else {
            len = 0;
        }
        i += len.max(1);
        if cp == u32::from(b':') {
            if let Some(last) = cells.last_mut() {
                last.colon_after = true;
            }
            continue;
        }
        cells.push(Cell {
            glyph: glyph_for(cp),
            colon_after: false,
        });
    }
    cells
}

/// Static text is centred; scrolling text (more than 4 cells) enters from the right, one cell
/// per `scroll_ms`, and ends on a blank frame. `t` = ms since the text was set. The colon is
/// the one after the cell at position 1.
pub fn text_glyphs(cells: &[Cell], t: i64, scroll_ms: i64) -> (Glyphs, bool) {
    let mut g = [0; DIGITS];
    let mut colon = false;
    let n = cells.len() as i64;
    let digits = DIGITS as i64;
    let first = if n <= digits {
        -(digits - n) / 2
    } else {
        (t / scroll_ms) % (n + digits) - (digits - 1)
    };
    for (p, slot) in g.iter_mut().enumerate() {
        let ci = first + p as i64;
        if !(0..n).contains(&ci) {
            continue;
        }
        let cell = cells[ci as usize];
        *slot = cell.glyph;
        if p == 1 {
            colon = cell.colon_after;
        }
    }
    (g, colon)
}

/// Faces from the "LED Studio" page (Text.html), positions left to right.
const FACES: [(&str, Glyphs); 17] = [
    ("Smiley", [0x40, 0x18, 0x0C, 0x40]),
    ("Sad", [0x40, 0x21, 0x03, 0x40]),
    ("Neutral", [0x40, 0x40, 0x40, 0x40]),
    ("Surprised", [0x40, 0x09, 0x00, 0x40]),
    ("Angry", [0x41, 0x40, 0x40, 0x41]),
    ("Wink", [0x40, 0x18, 0x0C, 0x00]),
    ("Big Smile", [0x40, 0x38, 0x1C, 0x40]),
    ("Sleepy", [0x08, 0x40, 0x40, 0x08]),
    ("Dead", [0x74, 0x08, 0x08, 0x74]),
    ("Cool", [0x22, 0x18, 0x0C, 0x22]),
    ("Love", [0x30, 0x18, 0x0C, 0x30]),
    ("Robot", [0x3F, 0x09, 0x00, 0x3F]),
    ("UwU", [0x1C, 0x1C, 0x1C, 0x1C]),
    ("UwU thin", [0x1C, 0x14, 0x14, 0x1C]),
    ("UwU happy", [0x23, 0x1C, 0x1C, 0x23]),
    ("OwO", [0x23, 0x08, 0x08, 0x23]),
    (">w<", [0x46, 0x1C, 0x1C, 0x70]),
];
const STATIC_FACES: i32 = FACES.len() as i32;
pub const FACE_BLINK: i32 = STATIC_FACES;
pub const FACE_ALL: i32 = STATIC_FACES + 1;
pub const FACE_COUNT: i32 = STATIC_FACES + 2;
const BLINK_OPEN: Glyphs = [0x23, 0x08, 0x08, 0x23];
const BLINK_SHUT: Glyphs = [0x40, 0x08, 0x08, 0x40];
const BLINK_PERIOD_MS: i64 = 3000;
const BLINK_SHUT_MS: i64 = 200;
/// Frame step of the animated faces.
pub const BLINK_STEP_MS: i64 = 100;
pub const FACE_CYCLE_MS: i64 = 2000;

/// Names of the face indices: the static faces, then "Blink", "All faces".
pub const FACE_NAMES: [&str; FACE_COUNT as usize] = {
    let mut names = [""; FACE_COUNT as usize];
    let mut i = 0;
    while i < FACES.len() {
        names[i] = FACES[i].0;
        i += 1;
    }
    names[FACE_BLINK as usize] = "Blink";
    names[FACE_ALL as usize] = "All faces";
    names
};

/// Face `face` at `t` ms since it was set; blank for an unknown index.
pub fn face_glyphs(face: i32, t: i64) -> Glyphs {
    if face == FACE_BLINK {
        if t % BLINK_PERIOD_MS < BLINK_PERIOD_MS - BLINK_SHUT_MS {
            BLINK_OPEN
        } else {
            BLINK_SHUT
        }
    } else if face == FACE_ALL {
        FACES[((t / FACE_CYCLE_MS) % i64::from(STATIC_FACES)) as usize].1
    } else if (0..STATIC_FACES).contains(&face) {
        FACES[face as usize].1
    } else {
        [0; DIGITS]
    }
}

/// Two digits of `v` (0..99) into `g[0..2]`; a leading zero blank if asked.
fn two(g: &mut [u8], v: i64, blank_lead: bool) {
    g[0] = if blank_lead && v < 10 {
        0
    } else {
        FONT[((v / 10) % 10) as usize]
    };
    g[1] = FONT[(v % 10) as usize];
}

/// hh:mm of the local time (colon handled by the caller).
pub fn time_glyphs(hour: i32, min: i32) -> Glyphs {
    let mut g = [0; DIGITS];
    two(&mut g[..2], i64::from(hour), false);
    two(&mut g[2..], i64::from(min), false);
    g
}

/// Countdown of `left_s` whole seconds: m:ss / mm:ss under an hour, else h:mm (hours >= 10 as
/// hh:mm, capped at 99).
pub fn timer_glyphs(left_s: i64) -> Glyphs {
    let mut g = [0; DIGITS];
    if left_s >= 3600 {
        two(&mut g[..2], (left_s / 3600).min(99), true);
        two(&mut g[2..], left_s / 60 % 60, false);
    } else {
        two(&mut g[..2], left_s / 60, true);
        two(&mut g[2..], left_s % 60, false);
    }
    g
}

/// Right-aligned number, '-' for negatives (the caller keeps it within -999..9999).
pub fn number_glyphs(v: i32) -> Glyphs {
    let mut g = [0; DIGITS];
    let mut n = v.unsigned_abs();
    let mut pos = DIGITS;
    loop {
        pos -= 1;
        g[pos] = FONT[(n % 10) as usize];
        n /= 10;
        if n == 0 || pos == 0 {
            break;
        }
    }
    if v < 0 && pos > 0 {
        g[pos - 1] = MINUS;
    }
    g
}

/// Up to 4 characters shown as is (right-aligned); longer codes page through 3-character
/// groups every `page_ms` ("123-", "-456"), matching the 3-3 grouping the Sendspin spec asks
/// for. Digits as digits, anything else (byte-wise) as '-'. `t` = ms since the code was set.
pub fn code_glyphs(code: &[u8], t: i64, page_ms: i64) -> Glyphs {
    let mut g = [0; DIGITS];
    let glyph = |c: u8| {
        if c.is_ascii_digit() {
            FONT[usize::from(c - b'0')]
        } else {
            MINUS
        }
    };
    if code.len() <= DIGITS {
        for (slot, &c) in g[DIGITS - code.len()..].iter_mut().zip(code) {
            *slot = glyph(c);
        }
    } else {
        let groups = code.len().div_ceil(3);
        let page = (t / page_ms) as usize % groups;
        let grp = &code[page * 3..(page * 3 + 3).min(code.len())];
        if page == 0 {
            for (slot, &c) in g.iter_mut().zip(grp) {
                *slot = glyph(c);
            }
            g[3] = MINUS;
        } else {
            g[0] = MINUS;
            for (slot, &c) in g[1..].iter_mut().zip(grp) {
                *slot = glyph(c);
            }
        }
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn cells_str(cells: &[Cell]) -> String {
        if cells.is_empty() {
            return "-".into();
        }
        let parts: Vec<String> = cells
            .iter()
            .map(|c| format!("{:02x}{}", c.glyph, if c.colon_after { ":" } else { "" }))
            .collect();
        parts.join(".")
    }

    /// ASCII, Cyrillic (both cases, Ё), '°', colons and malformed UTF-8, compared with the C++
    /// `layout_text`.
    #[test]
    fn layout_matches_cxx_golden() {
        let golden = include_str!("../tests/golden/text_layout.txt");
        let mut n = 0;
        for line in golden.lines() {
            let (input, want) = line.split_once(' ').unwrap();
            let bytes = if input == "-" { Vec::new() } else { hex(input) };
            assert_eq!(cells_str(&layout_text(&bytes)), want, "{input}");
            n += 1;
        }
        assert!(n >= 20);
    }

    #[test]
    fn glyph_table_spot_checks() {
        assert_eq!(glyph_for(u32::from(b'0')), FONT[0]);
        assert_eq!(glyph_for(u32::from(b'9')), FONT[9]);
        assert_eq!(glyph_for(u32::from(b'-')), MINUS);
        assert_eq!(glyph_for('Ё' as u32), glyph_for('Е' as u32));
        assert_eq!(glyph_for('ё' as u32), glyph_for('е' as u32));
        assert_eq!(glyph_for('я' as u32), glyph_for('Я' as u32));
        assert_eq!(glyph_for('°' as u32), 0x63);
        assert_eq!(glyph_for(0x7F), 0);
        assert_eq!(glyph_for('€' as u32), 0);
    }

    fn cells(s: &str) -> Vec<Cell> {
        layout_text(s.as_bytes())
    }

    #[test]
    fn static_text_is_centred() {
        let g = |s: &str| text_glyphs(&cells(s), 12345, 400);
        assert_eq!(g("1"), ([0, FONT[1], 0, 0], false));
        assert_eq!(g("12"), ([0, FONT[1], FONT[2], 0], false));
        assert_eq!(g("123"), ([FONT[1], FONT[2], FONT[3], 0], false));
        assert_eq!(g("1234"), ([FONT[1], FONT[2], FONT[3], FONT[4]], false));
        // The colon belongs to the cell at position 1.
        assert_eq!(g("12:34"), ([FONT[1], FONT[2], FONT[3], FONT[4]], true));
        assert!(!g("1:234").1);
        assert!(!g("12:").1); // "12" sits at positions 1..2: the colon follows '2'
        assert!(g("1:2").1);
    }

    #[test]
    fn scrolling_text_positions() {
        let c = cells("12345");
        let at = |step: i64| text_glyphs(&c, step * 300 + 299, 300).0;
        // Enters from the right ...
        assert_eq!(at(0), [0, 0, 0, FONT[1]]);
        assert_eq!(at(1), [0, 0, FONT[1], FONT[2]]);
        assert_eq!(at(3), [FONT[1], FONT[2], FONT[3], FONT[4]]);
        assert_eq!(at(4), [FONT[2], FONT[3], FONT[4], FONT[5]]);
        assert_eq!(at(7), [FONT[5], 0, 0, 0]);
        // ... ends on a blank frame, then starts over (period n + 4 steps).
        assert_eq!(at(8), [0; 4]);
        assert_eq!(at(9), at(0));
        // Colon after the cell at position 1 while it scrolls by.
        let c = cells("ab:cde");
        assert!(!text_glyphs(&c, 2 * 100, 100).1); // " abc": a at position 1
        assert!(text_glyphs(&c, 3 * 100, 100).1); // "abcd": b at position 1
    }

    #[test]
    fn faces() {
        assert_eq!(FACE_NAMES.len(), 19);
        assert_eq!(FACE_NAMES[0], "Smiley");
        assert_eq!(FACE_NAMES[16], ">w<");
        assert_eq!(FACE_NAMES[17], "Blink");
        assert_eq!(FACE_NAMES[18], "All faces");
        assert_eq!(face_glyphs(0, 99_999), [0x40, 0x18, 0x0C, 0x40]);
        assert_eq!(face_glyphs(FACE_BLINK, 2799), BLINK_OPEN);
        assert_eq!(face_glyphs(FACE_BLINK, 2800), BLINK_SHUT);
        assert_eq!(face_glyphs(FACE_BLINK, 3000), BLINK_OPEN);
        assert_eq!(face_glyphs(FACE_ALL, 1999), FACES[0].1);
        assert_eq!(face_glyphs(FACE_ALL, 2000), FACES[1].1);
        assert_eq!(face_glyphs(FACE_ALL, 17 * 2000), FACES[0].1);
        assert_eq!(face_glyphs(-1, 0), [0; 4]);
    }

    #[test]
    fn timer_format() {
        let f = |d: usize| FONT[d];
        assert_eq!(timer_glyphs(0), [0, f(0), f(0), f(0)]);
        assert_eq!(timer_glyphs(5), [0, f(0), f(0), f(5)]);
        assert_eq!(timer_glyphs(125), [0, f(2), f(0), f(5)]);
        assert_eq!(timer_glyphs(59 * 60 + 59), [f(5), f(9), f(5), f(9)]);
        // From an hour on: h:mm.
        assert_eq!(timer_glyphs(3600), [0, f(1), f(0), f(0)]);
        assert_eq!(timer_glyphs(3600 + 61 * 60), [0, f(2), f(0), f(1)]);
        assert_eq!(timer_glyphs(10 * 3600 + 5 * 60), [f(1), f(0), f(0), f(5)]);
        assert_eq!(timer_glyphs(150 * 3600), [f(9), f(9), f(0), f(0)]);
    }

    #[test]
    fn numbers_and_time() {
        let f = |d: usize| FONT[d];
        assert_eq!(number_glyphs(0), [0, 0, 0, f(0)]);
        assert_eq!(number_glyphs(42), [0, 0, f(4), f(2)]);
        assert_eq!(number_glyphs(100), [0, f(1), f(0), f(0)]);
        assert_eq!(number_glyphs(9999), [f(9), f(9), f(9), f(9)]);
        assert_eq!(number_glyphs(-5), [0, 0, MINUS, f(5)]);
        assert_eq!(number_glyphs(-999), [MINUS, f(9), f(9), f(9)]);
        assert_eq!(time_glyphs(7, 5), [f(0), f(7), f(0), f(5)]);
        assert_eq!(time_glyphs(23, 59), [f(2), f(3), f(5), f(9)]);
    }

    #[test]
    fn code_paging() {
        let f = |d: usize| FONT[d];
        assert_eq!(code_glyphs(b"12", 0, 2000), [0, 0, f(1), f(2)]);
        assert_eq!(code_glyphs(b"----", 0, 2000), [MINUS; 4]);
        assert_eq!(code_glyphs(b"12a4", 0, 2000), [f(1), f(2), MINUS, f(4)]);
        let c = b"123456";
        assert_eq!(code_glyphs(c, 0, 2000), [f(1), f(2), f(3), MINUS]);
        assert_eq!(code_glyphs(c, 1999, 2000), [f(1), f(2), f(3), MINUS]);
        assert_eq!(code_glyphs(c, 2000, 2000), [MINUS, f(4), f(5), f(6)]);
        assert_eq!(code_glyphs(c, 4000, 2000), [f(1), f(2), f(3), MINUS]);
        // A short last group.
        let c = b"1234567";
        assert_eq!(code_glyphs(c, 4000, 2000), [MINUS, f(7), 0, 0]);
        assert_eq!(code_glyphs(c, 6000, 2000), [f(1), f(2), f(3), MINUS]);
        assert_eq!(code_glyphs(b"12345", 2000, 2000), [MINUS, f(4), f(5), 0]);
    }
}
