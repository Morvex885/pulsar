//! Timing metadata supplied with the TTS stream, in milliseconds from its first sample.

/// How long a number stays on the clock from the moment it appears. The next number waits,
/// so a one-second word can still be read.
pub(super) const HOLD_MS: i64 = 3_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SpeechMark {
    pub start: i64,
    pub end: i64,
    pub display: String,
}

/// What the clock should do at `elapsed`.
/// `latch` remembers the last mark that was shown and when the next decision is due, so a gap
/// does not play that mark again. `wake` is that decision, in the same elapsed clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Step {
    pub show: Option<usize>,
    pub latch: Option<(usize, i64)>,
    pub wake: Option<i64>,
}

pub(super) fn shown(
    marks: &[SpeechMark],
    elapsed: i64,
    latched: Option<(usize, i64)>,
    scroll_ms: i64,
) -> Step {
    if let Some((index, until)) = latched
        && elapsed < until
        && index < marks.len()
        && marks.get(index + 1).map(|mark| mark.start) != Some(until)
    {
        // `until == next.start` is a blank gap, not a hold. Showing here would put the
        // previous number back up after it had already left.
        return Step {
            show: Some(index),
            latch: latched,
            wake: Some(until),
        };
    }
    let next = latched.map(|(index, _)| index + 1).unwrap_or(0);
    let Some(mark) = marks.get(next) else {
        return Step {
            show: None,
            latch: None,
            wake: None,
        };
    };
    if elapsed < mark.start {
        return Step {
            show: None,
            latch: latched.map(|(index, _)| (index, mark.start)),
            wake: Some(mark.start),
        };
    }
    let cells = mark.display.bytes().filter(|&b| b != b':').count() as i64;
    let hold = HOLD_MS.max(if cells > 4 {
        (cells + 4) * scroll_ms
    } else {
        0
    });
    let until = (elapsed + hold).max(mark.end);
    Step {
        show: Some(next),
        latch: Some((next, until)),
        wake: Some(until),
    }
}

/// Lines: start,end,value. Reject the entire list on malformed or overlapping marks.
pub(super) fn parse(input: &str) -> Option<Vec<SpeechMark>> {
    let mut marks = Vec::new();
    let mut previous_end = 0;
    for line in input.lines() {
        if marks.len() == 128 {
            return None;
        }
        let mut fields = line.splitn(3, ',');
        let start: i64 = fields.next()?.parse().ok()?;
        let end: i64 = fields.next()?.parse().ok()?;
        let display = fields.next()?;
        if start < previous_end
            || end <= start
            || end > 300_000
            || display.is_empty()
            || display.len() > 32
            || !display
                .bytes()
                .all(|b| b.is_ascii_digit() || b"-:.".contains(&b))
        {
            return None;
        }
        marks.push(SpeechMark {
            start,
            end,
            display: display.into(),
        });
        previous_end = end;
    }
    Some(marks)
}

/// Put two digit values in alternating halves of the four digit display.
pub(super) fn positioned(index: usize, value: &str) -> String {
    if value.len() == 2 && value.bytes().all(|b| b.is_ascii_digit()) {
        if index.is_multiple_of(2) {
            format!("{value}  ")
        } else {
            format!("  {value}")
        }
    } else {
        value.into()
    }
}

fn word(token: &str) -> String {
    token
        .trim_matches(|c: char| !c.is_alphabetic())
        .to_lowercase()
        .replace('ё', "е")
}

fn unit(token: &str) -> Option<i32> {
    Some(match word(token).as_str() {
        "ноль" | "нуля" => 0,
        "один" | "одна" | "одно" | "одного" | "одной" => 1,
        "два" | "две" | "двух" => 2,
        "три" | "трех" => 3,
        "четыре" | "четырех" => 4,
        "пять" | "пяти" => 5,
        "шесть" | "шести" => 6,
        "семь" | "семи" => 7,
        "восемь" | "восьми" => 8,
        "девять" | "девяти" => 9,
        "десять" | "десяти" => 10,
        "одиннадцать" | "одиннадцати" => 11,
        "двенадцать" | "двенадцати" => 12,
        "тринадцать" | "тринадцати" => 13,
        "четырнадцать" | "четырнадцати" => 14,
        "пятнадцать" | "пятнадцати" => 15,
        "шестнадцать" | "шестнадцати" => 16,
        "семнадцать" | "семнадцати" => 17,
        "восемнадцать" | "восемнадцати" => 18,
        "девятнадцать" | "девятнадцати" => 19,
        _ => return None,
    })
}

fn tens(token: &str) -> Option<i32> {
    Some(match word(token).as_str() {
        "двадцать" | "двадцати" => 20,
        "тридцать" | "тридцати" => 30,
        "сорок" | "сорока" => 40,
        "пятьдесят" | "пятидесяти" => 50,
        "шестьдесят" | "шестидесяти" => 60,
        "семьдесят" | "семидесяти" => 70,
        "восемьдесят" | "восьмидесяти" => 80,
        "девяносто" | "девяноста" => 90,
        _ => return None,
    })
}

fn literal(token: &str) -> Option<String> {
    let value = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '-');
    if value.is_empty()
        || value.len() > 32
        || !value.bytes().any(|b| b.is_ascii_digit())
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || b"-:.,".contains(&b))
    {
        return None;
    }
    Some(value.replace(',', "."))
}

/// Approximate display schedule when HA sends response text but no timed marks.
/// Digits and common Russian cardinal words get a minimum three-second slot.
pub(super) fn from_text(input: &str) -> Vec<SpeechMark> {
    let mut marks = Vec::new();
    let tokens: Vec<_> = input.split_whitespace().collect();
    let mut i = 0;
    let mut negative = false;
    while i < tokens.len() && marks.len() < 100 {
        match word(tokens[i]).as_str() {
            "минус" => {
                negative = true;
                i += 1;
                continue;
            }
            "плюс" => {
                negative = false;
                i += 1;
                continue;
            }
            _ => {}
        }
        let (value, consumed) = if let Some(t) = tens(tokens[i]) {
            let next = tokens.get(i + 1).and_then(|s| unit(s));
            if let Some(u @ 1..=9) = next {
                ((t + u).to_string(), 2)
            } else {
                (t.to_string(), 1)
            }
        } else if let Some(u) = unit(tokens[i]) {
            (u.to_string(), 1)
        } else if let Some(value) = literal(tokens[i]) {
            (value, 1)
        } else {
            negative = false;
            i += 1;
            continue;
        };
        let display = if negative && !value.starts_with('-') {
            format!("-{value}")
        } else {
            value
        };
        let start = marks.len() as i64 * HOLD_MS;
        marks.push(SpeechMark {
            start,
            end: start + 1,
            display,
        });
        negative = false;
        i += consumed;
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_and_rejects_invalid_or_overlapping_cues() {
        let m = parse("766,1103,9\n1800,2200,-12").unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].display, "9");
        assert!(parse("766,1103,9\n1000,2200,2").is_none());
        assert!(parse("0,4,123;bad").is_none());
        assert!(parse("0,999999,1").is_none());
    }

    fn mark(start: i64, end: i64, display: &str) -> SpeechMark {
        SpeechMark {
            start,
            end,
            display: display.into(),
        }
    }

    #[test]
    fn each_number_stays_long_enough_to_read() {
        let marks = vec![mark(500, 900, "9"), mark(4_000, 4_400, "13")];
        let first = shown(&marks, 500, None, 400);
        assert_eq!(first.show, Some(0));
        assert_eq!(first.wake, Some(500 + HOLD_MS));
        let still = shown(&marks, 1_800, first.latch, 400);
        assert_eq!(still.show, Some(0));
        let gap = shown(&marks, 3_500, Some((0, 3_500)), 400);
        assert_eq!(gap.show, None);
        assert_eq!(gap.latch, Some((0, 4_000)));
        let next = shown(&marks, 4_000, gap.latch, 400);
        assert_eq!(next.show, Some(1));
        assert_eq!(next.wake, Some(4_000 + HOLD_MS));
        let parked = shown(&marks, 3_600, gap.latch, 400);
        assert_eq!(parked.show, None);
    }

    #[test]
    fn long_numbers_complete_one_scroll_pass() {
        let marks = vec![mark(0, 500, "-12.5")];
        assert_eq!(shown(&marks, 0, None, 400).wake, Some(3_600));
    }

    #[test]
    fn text_without_timing_extracts_written_numbers() {
        assert_eq!(positioned(0, "12"), "12  ");
        assert_eq!(positioned(1, "34"), "  34");
        assert_eq!(positioned(2, "56"), "56  ");
        assert_eq!(positioned(1, "7.5"), "7.5");
        let marks = from_text("Температура -12,5, время 21:58. Повтор 42.");
        assert_eq!(
            marks.iter().map(|m| m.display.as_str()).collect::<Vec<_>>(),
            vec!["-12.5", "21:58", "42"]
        );
        assert_eq!(marks[1].start, HOLD_MS);
        let weather = from_text(
            "Сейчас плюс два градуса, ночью минус двадцать три. Ветер шесть метров в секунду.",
        );
        assert_eq!(
            weather
                .iter()
                .map(|m| m.display.as_str())
                .collect::<Vec<_>>(),
            vec!["2", "-23", "6"]
        );
        assert!(from_text("Сегодня облачно, температура низкая.").is_empty());
    }

    #[test]
    fn a_long_phrase_stays_until_it_finishes() {
        let marks = vec![mark(0, 4_500, "21:58")];
        assert_eq!(shown(&marks, 0, None, 400).wake, Some(4_500));
    }
}
