//! Метки TTS в миллисекундах аудио ответа, без добавленной при голодании тишины.

/// Минимальное удержание числа: даже коротко произнесённое значение нужно успеть прочитать.
pub(super) const HOLD_MS: i64 = 3_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SpeechMark {
    pub start: i64,
    pub end: i64,
    pub display: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Latch {
    pub index: usize,
    pub until: i64,
    pub holding: bool,
}

/// `latch` хранит последнюю показанную метку, чтобы пауза не запускала её повторно.
/// `wake` задаёт следующую проверку на той же шкале времени аудио, что и `elapsed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Step {
    pub show: Option<usize>,
    pub latch: Option<Latch>,
    pub wake: Option<i64>,
}

// Задержка ALSA может колебаться; шкала не идёт назад и не опережает переданные сэмплы.
pub(super) fn playback_elapsed(now: i64, start: i64, end: Option<i64>, previous: i64) -> i64 {
    (now - start)
        .max(0)
        .min(end.unwrap_or(i64::MAX))
        .max(previous)
}

pub(super) fn shown(
    marks: &[SpeechMark],
    elapsed: i64,
    latched: Option<Latch>,
    scroll_ms: i64,
) -> Step {
    if let Some(Latch {
        index,
        until,
        holding: true,
    }) = latched
        && elapsed < until
        && index < marks.len()
    {
        return Step {
            show: Some(index),
            latch: latched,
            wake: Some(until),
        };
    }
    let next = latched.map(|latch| latch.index + 1).unwrap_or_else(|| {
        marks
            .iter()
            .position(|mark| mark.end > elapsed)
            .unwrap_or(marks.len())
    });
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
            latch: latched.map(|latch| Latch {
                index: latch.index,
                until: mark.start,
                holding: false,
            }),
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
        latch: Some(Latch {
            index: next,
            until,
            holding: true,
        }),
        wake: Some(until),
    }
}

/// Строки start,end,value; повреждённый или пересекающийся список отклоняется целиком.
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
            || !display.bytes().any(|b| b.is_ascii_digit())
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

/// Двузначные числа занимают попеременно левую и правую половины дисплея.
pub(super) fn positioned(marks: &[SpeechMark], index: usize) -> String {
    let value = &marks[index].display;
    let two_digits = |value: &str| value.len() == 2 && value.bytes().all(|b| b.is_ascii_digit());
    if two_digits(value) {
        let preceding = marks[..index]
            .iter()
            .filter(|mark| two_digits(&mark.display))
            .count();
        if preceding.is_multiple_of(2) {
            format!("{value}  ")
        } else {
            format!("  {value}")
        }
    } else {
        value.clone()
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

fn boundary(token: &str) -> bool {
    token
        .chars()
        .rev()
        .take_while(|c| !c.is_alphanumeric())
        .any(|c| ".,;:!?…".contains(c))
}

fn number(tokens: &[&str], i: usize) -> Option<(String, usize)> {
    let token = *tokens.get(i)?;
    if let Some(t) = tens(token) {
        if !boundary(token)
            && let Some(u @ 1..=9) = tokens.get(i + 1).and_then(|s| unit(s))
        {
            return Some(((t + u).to_string(), 2));
        }
        Some((t.to_string(), 1))
    } else if let Some(u) = unit(token) {
        Some((u.to_string(), 1))
    } else {
        literal(token).map(|s| (s, 1))
    }
}

/// Без меток от HA известен лишь текст: цифрам и русским числительным отводится по три секунды.
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
        let Some((mut value, mut consumed)) = number(&tokens, i) else {
            negative = false;
            i += 1;
            continue;
        };
        // Часы и минуты — одно значение: короткий ответ может закончиться до второй метки.
        if !negative && let Ok(hour @ 0..=23) = value.parse::<u8>() {
            let unit_index = i + consumed;
            if !boundary(tokens[unit_index - 1])
                && tokens
                    .get(unit_index)
                    .is_some_and(|s| matches!(word(s).as_str(), "час" | "часа" | "часов"))
                && !boundary(tokens[unit_index])
                && let Some((minutes, count)) = number(&tokens, unit_index + 1)
                && !boundary(tokens[unit_index + count])
                && let Ok(minute @ 0..=59) = minutes.parse::<u8>()
                && tokens
                    .get(unit_index + 1 + count)
                    .is_some_and(|s| matches!(word(s).as_str(), "минута" | "минуты" | "минут"))
            {
                value = format!("{hour:02}:{minute:02}");
                consumed += count + 2;
            }
        }
        let display = if negative && !value.starts_with('-') {
            format!("-{value}")
        } else {
            value
        };
        let start = marks.len() as i64 * HOLD_MS;
        marks.push(SpeechMark {
            start,
            end: start + HOLD_MS,
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
    fn adjacent_marks_keep_the_first_visible_during_progress() {
        let marks = from_text("12 34");
        let first = shown(&marks, 0, None, 400);
        assert_eq!(shown(&marks, 10, first.latch, 400).show, Some(0));
        assert_eq!(shown(&marks, HOLD_MS, first.latch, 400).show, Some(1));
    }

    #[test]
    fn playback_clock_freezes_in_silence_and_resumes_without_rewinding() {
        assert_eq!(playback_elapsed(1500, 1000, Some(1000), 0), 500);
        assert_eq!(playback_elapsed(5000, 1000, Some(1000), 500), 1000);
        assert_eq!(playback_elapsed(5100, 4200, Some(2000), 1000), 1000);
        assert_eq!(playback_elapsed(5600, 4200, Some(2000), 1000), 1400);
        assert_eq!(playback_elapsed(7000, 4200, None, 1400), 2800);
    }

    #[test]
    fn expired_marks_are_not_replayed_after_replacement() {
        let marks = vec![mark(500, 900, "21"), mark(4_000, 5_000, "22")];
        assert_eq!(shown(&marks, 4_500, None, 400).show, Some(1));
        assert_eq!(shown(&marks, 5_000, None, 400).show, None);
        assert_eq!(shown(&marks, 900, None, 400).show, None);
    }

    #[test]
    fn sentence_boundaries_and_full_time() {
        for separator in [".", "!", "?", ",", ";", "…", ".»"] {
            let marks = from_text(&format!("двадцать{separator} Пять минут"));
            assert_eq!(
                marks.iter().map(|m| m.display.as_str()).collect::<Vec<_>>(),
                ["20", "5"]
            );
        }
        for text in [
            "Сейчас двадцать один час пять минут.",
            "Сейчас 21 час 5 минут.",
            "Время 21:05.",
        ] {
            let marks = from_text(text);
            assert_eq!(marks.len(), 1, "{text}");
            assert_eq!(marks[0].display, "21:05");
            assert_eq!(shown(&marks, 20, None, 400).show, Some(0));
        }
        for text in [
            "25 часов 60 минут",
            "Двадцать один. Час пять минут",
            "21 час 5. Минут",
        ] {
            assert_eq!(from_text(text).len(), 2, "{text}");
        }
    }

    #[test]
    fn parses_and_rejects_invalid_or_overlapping_cues() {
        let m = parse("766,1103,9\n1800,2200,-12").unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].display, "9");
        assert!(parse("766,1103,9\n1000,2200,2").is_none());
        assert!(parse("0,4,123;bad").is_none());
        assert!(parse("0,999999,1").is_none());
        for value in [":", "-", ".", "-:."] {
            assert!(parse(&format!("0,500,{value}")).is_none());
        }
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
        let gap = shown(
            &marks,
            3_500,
            Some(Latch {
                index: 0,
                until: 3_500,
                holding: true,
            }),
            400,
        );
        assert_eq!(gap.show, None);
        assert_eq!(
            gap.latch,
            Some(Latch {
                index: 0,
                until: 4_000,
                holding: false
            })
        );
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
        let mixed = from_text("5 12 123 34 7,5 56");
        assert_eq!(
            (0..mixed.len())
                .map(|index| positioned(&mixed, index))
                .collect::<Vec<_>>(),
            ["5", "12  ", "123", "  34", "7.5", "56  "]
        );
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
    fn trailing_punctuation_is_not_a_decimal_separator() {
        let marks = from_text("5, завтра 6. Затем -5,5, и 7.5.");
        assert_eq!(
            marks
                .iter()
                .map(|mark| mark.display.as_str())
                .collect::<Vec<_>>(),
            ["5", "6", "-5.5", "7.5"]
        );
    }

    #[test]
    fn a_long_phrase_stays_until_it_finishes() {
        let marks = vec![mark(0, 4_500, "21:58")];
        assert_eq!(shown(&marks, 0, None, 400).wake, Some(4_500));
    }
}
