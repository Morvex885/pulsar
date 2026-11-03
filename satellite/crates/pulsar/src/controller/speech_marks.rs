//! Последовательный показ чисел ответа без синхронизации с отдельными словами.

const HOLD_MS: i64 = 3_000;

#[derive(Default)]
pub(super) struct SpeechDisplay {
    numbers: Vec<String>,
    started: bool,
    next: usize,
    until: Option<i64>,
    displayed: String,
}

impl SpeechDisplay {
    pub fn text(&mut self, text: &str) {
        if self.numbers.is_empty() {
            self.numbers = from_text(text);
        }
    }

    pub fn start(&mut self) {
        self.started = true;
    }

    pub fn clear(&mut self) -> Option<String> {
        let changed = !self.displayed.is_empty();
        *self = Self::default();
        changed.then(String::new)
    }

    pub fn wake(&self) -> Option<i64> {
        self.until
    }

    pub fn tick(&mut self, now: i64, scroll_ms: i64) -> Option<String> {
        if !self.started || self.until.is_some_and(|until| now < until) {
            return None;
        }
        let display = if let Some(number) = self.numbers.get(self.next) {
            let cells = number.bytes().filter(|&b| b != b':').count() as i64;
            let hold = HOLD_MS.max(if cells > 4 {
                (cells + 4) * scroll_ms
            } else {
                0
            });
            self.until = Some(now + hold);
            let display = positioned(&self.numbers, self.next);
            self.next += 1;
            display
        } else {
            self.until = None;
            String::new()
        };
        if display == self.displayed {
            return None;
        }
        self.displayed.clone_from(&display);
        Some(display)
    }
}

fn positioned(numbers: &[String], index: usize) -> String {
    let value = &numbers[index];
    let two_digits = |value: &str| value.len() == 2 && value.bytes().all(|b| b.is_ascii_digit());
    if two_digits(value) {
        let preceding = numbers[..index]
            .iter()
            .filter(|value| two_digits(value))
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
        "один" | "одна" | "одно" | "одного" | "одной" | "одну" => 1,
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
    let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
    // Буквенный префикс — часть имени (COVID-19), а не отрицательное число.
    if !token.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
        return None;
    }
    let value = if let Some((digits, suffix)) = token.split_once('-') {
        if !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && matches!(
                suffix,
                "го" | "му"
                    | "й"
                    | "я"
                    | "е"
                    | "ый"
                    | "ой"
                    | "ая"
                    | "ое"
                    | "ые"
                    | "х"
                    | "м"
            )
        {
            digits
        } else {
            token
        }
    } else {
        token
    };
    if value.is_empty() || value.len() > 32 {
        return None;
    }
    if let Some((h, m)) = value.split_once(':') {
        let hour = h.parse::<u8>().ok()?;
        let minute = m.parse::<u8>().ok()?;
        return ((1..=2).contains(&h.len()) && m.len() == 2 && hour < 24 && minute < 60)
            .then(|| format!("{hour:02}:{minute:02}"));
    }
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let mut parts = unsigned.split(['.', ',']);
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(parts.next()?)
        || parts.next().is_some_and(|part| !digits(part))
        || parts.next().is_some()
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

/// Текст HA не содержит выравнивания слов со звуком; порядок чисел сохраняется.
pub(super) fn from_text(input: &str) -> Vec<String> {
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
        // «Один момент» и «ни одного» не сообщают измеренного значения.
        if consumed == 1
            && unit(tokens[i]) == Some(1)
            && (tokens
                .get(i + 1)
                .is_some_and(|s| matches!(word(s).as_str(), "момент" | "мгновение"))
                || (i > 0 && word(tokens[i - 1]) == "ни" && !boundary(tokens[i - 1])))
        {
            negative = false;
            i += consumed;
            continue;
        }
        // Часы и минуты показываются вместе, включая произнесённый ведущий ноль.
        if !negative && let Ok(hour @ 0..=23) = value.parse::<u8>() {
            let unit_index = i + consumed;
            if !boundary(tokens[unit_index - 1])
                && tokens
                    .get(unit_index)
                    .is_some_and(|s| matches!(word(s).as_str(), "час" | "часа" | "часов"))
                && !boundary(tokens[unit_index])
                && let Some((minutes, count)) = minutes(&tokens, unit_index + 1)
                && !boundary(tokens[unit_index + count])
                && let Ok(minute @ 0..=59) = minutes.parse::<u8>()
                && tokens.get(unit_index + 1 + count).is_some_and(|s| {
                    matches!(word(s).as_str(), "минута" | "минуты" | "минут" | "минуту")
                })
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
        marks.push(display);
        negative = false;
        i += consumed;
    }
    marks
}

fn minutes(tokens: &[&str], i: usize) -> Option<(String, usize)> {
    let (value, count) = number(tokens, i)?;
    if value == "0"
        && !boundary(tokens[i + count - 1])
        && let Some((digit, extra)) = number(tokens, i + count)
        && digit.parse::<u8>().is_ok_and(|n| n < 10)
    {
        return Some((digit, count + extra));
    }
    Some((value, count))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognition_regressions() {
        for (text, expected) in [
            ("21-го ноября", vec!["21"]),
            ("COVID-19", vec![]),
            ("один момент, ни одного", vec![]),
            ("двадцать одну минуту", vec!["21"]),
            ("двадцать один час ноль пять минут", vec!["21:05"]),
            ("Сейчас 21 час 5 минут.", vec!["21:05"]),
            ("Время 9:05.", vec!["09:05"]),
            ("Температура -12,5, потом 7.5.", vec!["-12.5", "7.5"]),
            ("плюс два градуса, минус двадцать три", vec!["2", "-23"]),
            ("5, завтра 6.", vec!["5", "6"]),
            ("123 12-34 1.2.3 -", vec!["123"]),
        ] {
            assert_eq!(from_text(text), expected, "{text}");
        }
        for separator in [".", "!", "?", ",", ";", "…", ".»"] {
            assert_eq!(
                from_text(&format!("двадцать{separator} Пять минут")),
                ["20", "5"]
            );
        }
    }

    #[test]
    fn late_text_and_short_answer_keep_every_number() {
        let mut display = SpeechDisplay::default();
        display.start();
        assert_eq!(display.tick(0, 400), None);
        display.text("12 34 56");
        assert_eq!(display.tick(10_000, 400).as_deref(), Some("12  "));
        assert_eq!(display.tick(10_100, 400), None);
        assert_eq!(display.tick(13_000, 400).as_deref(), Some("  34"));
        // Задержанный таймер не пропускает числа и даёт полное время чтения.
        assert_eq!(display.tick(20_000, 400).as_deref(), Some("56  "));
        assert_eq!(display.wake(), Some(23_000));
        assert_eq!(display.tick(23_000, 400).as_deref(), Some(""));
        display.text("12 34 56");
        assert_eq!(display.tick(30_000, 400), None);
        assert_eq!(display.wake(), None);
    }

    #[test]
    fn playback_gates_text_and_cancellation_clears_once() {
        let mut display = SpeechDisplay::default();
        display.text("5");
        assert_eq!(display.tick(0, 400), None);
        display.start();
        assert_eq!(display.tick(1, 400).as_deref(), Some("5"));
        assert_eq!(display.clear().as_deref(), Some(""));
        assert_eq!(display.clear(), None);
        assert_eq!(display.tick(5000, 400), None);
    }

    #[test]
    fn long_numbers_finish_scrolling_and_sides_ignore_other_numbers() {
        let numbers = from_text("5 12 123 34 7,5 56");
        assert_eq!(
            (0..numbers.len())
                .map(|i| positioned(&numbers, i))
                .collect::<Vec<_>>(),
            ["5", "12  ", "123", "  34", "7.5", "56  "]
        );
        let mut display = SpeechDisplay::default();
        display.text("-12.5 7");
        display.start();
        assert_eq!(display.tick(0, 400).as_deref(), Some("-12.5"));
        assert_eq!(display.wake(), Some(3600));
        assert_eq!(display.tick(3600, 400).as_deref(), Some("7"));
    }
}
