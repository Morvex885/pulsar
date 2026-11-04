//! Обучение голосу: приём образцов, конец фразы без конвейера HA, подсказки для сущности.

use super::gallery::{EMBEDDING_DIM, cosine, normalized};

pub const ENROLL_UTTERANCES: u32 = 5;
/// Пока идёт запись, станция не отвечает на активационное слово, поэтому запись не висит вечно.
pub const ENROLL_EXPIRE_MS: u64 = 5 * 60 * 1000;
/// Таймер нужен на случай, если звук с микрофона перестанет приходить.
pub const PHRASE_MAX_MS: u64 = 8000;

// Посередине между замерами на Миди: один говорящий от 0,70, разные до 0,47.
const MIN_SAMPLE_COS: f32 = 0.55;
// После стольких отказов подряд, видимо, плох первый образец: начинаем заново.
const MAX_REJECTS: u32 = 3;

const SAMPLE_RATE_MS: usize = 16;
// Чтобы пауза после активационного слова не обрывала запись.
const PHRASE_MIN_MS: usize = 2000;
const PHRASE_SILENCE_MS: usize = 800;
// Тише пика фразы на столько дБ — уже не речь.
const PHRASE_RANGE_DB: f32 = 20.0;

const PHRASES: [(&str, &str); ENROLL_UTTERANCES as usize] = [
    (
        "какая сегодня погода и нужен ли зонт?",
        "В метре от станции, обычным голосом.",
    ),
    (
        "сколько будет двенадцать умножить на восемь?",
        "В метре от станции, обычным голосом.",
    ),
    (
        "как сказать по-английски доброе утро?",
        "С места, откуда обычно говорите со станцией.",
    ),
    (
        "назови столицы Австралии и Канады.",
        "С места, откуда обычно говорите со станцией.",
    ),
    (
        "расскажи коротко, почему небо голубое.",
        "С самого дальнего места, откуда станция вас слышит.",
    ),
];

#[derive(Debug)]
pub(super) struct Enrollment {
    pub(super) name: String,
    pub(super) samples: Vec<Vec<f32>>,
    rejects: u32,
}

#[derive(Debug, PartialEq)]
pub(super) enum Offer {
    Accepted,
    Rejected(f32),
    Restarted,
    Done,
}

impl Enrollment {
    pub(super) fn new(name: String) -> Enrollment {
        Enrollment {
            name,
            samples: Vec::new(),
            rejects: 0,
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn done(&self) -> u32 {
        self.samples.len() as u32
    }

    pub(super) fn offer(&mut self, embedding: Vec<f32>) -> Offer {
        if let Some(center) = self.center() {
            let score = cosine(&center, &embedding);
            if score < MIN_SAMPLE_COS {
                self.rejects += 1;
                if self.rejects >= MAX_REJECTS {
                    self.samples.clear();
                    self.rejects = 0;
                    return Offer::Restarted;
                }
                return Offer::Rejected(score);
            }
        }
        self.rejects = 0;
        self.samples.push(embedding);
        if self.done() >= ENROLL_UTTERANCES {
            Offer::Done
        } else {
            Offer::Accepted
        }
    }

    fn center(&self) -> Option<Vec<f32>> {
        let mut sum = vec![0.0f32; EMBEDDING_DIM];
        for s in &self.samples {
            for (a, b) in sum.iter_mut().zip(s) {
                *a += b;
            }
        }
        normalized(sum)
    }
}

/// Пометка к подсказке: чем кончилось последнее действие.
#[derive(Clone, Debug, PartialEq)]
pub enum EnrollNote {
    None,
    TooShort,
    Rejected,
    Restarted,
    SoundsLike(String),
    Saved(String),
    SaveFailed,
    ForgetFailed,
    Cancelled,
    Expired,
    BadName,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EnrollSound {
    Accepted,
    Saved,
    Rejected,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnrollView {
    /// Имя и число принятых образцов; None — запись не идёт.
    pub active: Option<(String, u32)>,
    pub note: EnrollNote,
    pub sound: Option<EnrollSound>,
}

/// Текст сущности «Voice recording prompt». `wake` — активационное слово для фразы.
#[must_use]
pub fn prompt(view: &EnrollView, wake: &str) -> String {
    let Some((_, done)) = &view.active else {
        return match &view.note {
            EnrollNote::Saved(name) => {
                format!("Голос сохранён: {name}. Проверьте: «{wake}, как меня зовут?»")
            }
            EnrollNote::SaveFailed => {
                "Не удалось сохранить голос: запись отменена, прежний голос остался.".to_owned()
            }
            EnrollNote::ForgetFailed => "Не удалось удалить голос: он остался.".to_owned(),
            EnrollNote::Cancelled => "Запись отменена.".to_owned(),
            EnrollNote::Expired => "Запись отменена: 5 минут не было фраз.".to_owned(),
            EnrollNote::BadName => {
                "Имя не подходит: нужно от 1 до 32 символов без пробелов по краям.".to_owned()
            }
            _ => "Введите имя в Voice name и нажмите Start voice recording.".to_owned(),
        };
    };
    let note = match &view.note {
        EnrollNote::TooShort => "Мало речи, повторите. ".to_owned(),
        EnrollNote::Rejected => {
            "Не похоже на прошлые фразы: говорить должен один человек, без музыки. ".to_owned()
        }
        EnrollNote::Restarted => "Фразы слишком разные, запись начата заново. ".to_owned(),
        EnrollNote::SoundsLike(name) => format!("Похоже на голос: {name}. "),
        EnrollNote::ForgetFailed => "Не удалось удалить голос: он остался. ".to_owned(),
        _ => String::new(),
    };
    let index = (*done as usize).min(PHRASES.len() - 1);
    let (phrase, place) = PHRASES[index];
    format!(
        "{note}Фраза {} из {ENROLL_UTTERANCES}. Скажите: «{wake}, {phrase}» {place}",
        index + 1
    )
}

/// Конец фразы по громкости: во время записи VAD из HA нет.
#[derive(Debug, Default)]
pub struct Endpointer {
    samples: usize,
    peak_db: f32,
    silent_samples: usize,
}

impl Endpointer {
    #[must_use]
    pub fn new() -> Endpointer {
        Endpointer {
            samples: 0,
            peak_db: f32::NEG_INFINITY,
            silent_samples: 0,
        }
    }

    /// Следующий кусок звука, начиная с пре-ролла; true — фраза кончилась.
    pub fn feed(&mut self, pcm: &[i16]) -> bool {
        self.samples += pcm.len();
        let db = level_db(pcm);
        if db > self.peak_db {
            self.peak_db = db;
        }
        if db < self.peak_db - PHRASE_RANGE_DB {
            self.silent_samples += pcm.len();
        } else {
            self.silent_samples = 0;
        }
        self.samples >= PHRASE_MAX_MS as usize * SAMPLE_RATE_MS
            || (self.samples >= PHRASE_MIN_MS * SAMPLE_RATE_MS
                && self.silent_samples >= PHRASE_SILENCE_MS * SAMPLE_RATE_MS)
    }
}

fn level_db(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum: f32 = pcm.iter().map(|&s| f32::from(s) * f32::from(s)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum / pcm.len() as f32;
    10.0 * mean.max(1.0).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(cos: f32) -> Vec<f32> {
        let mut v = vec![0.0; EMBEDDING_DIM];
        v[0] = cos;
        v[1] = (1.0 - cos * cos).sqrt();
        v
    }

    fn view(active: Option<u32>, note: EnrollNote) -> EnrollView {
        EnrollView {
            active: active.map(|d| ("Мама".to_owned(), d)),
            note,
            sound: None,
        }
    }

    #[test]
    fn five_like_samples_finish() {
        let mut e = Enrollment::new("Мама".into());
        for _ in 0..4 {
            assert_eq!(e.offer(voice(1.0)), Offer::Accepted);
        }
        assert_eq!(e.offer(voice(0.8)), Offer::Done);
        assert_eq!(e.done(), 5);
    }

    #[test]
    fn unlike_sample_is_rejected_and_not_counted() {
        let mut e = Enrollment::new("Мама".into());
        assert_eq!(e.offer(voice(1.0)), Offer::Accepted);
        assert!(matches!(e.offer(voice(0.4)), Offer::Rejected(s) if s < MIN_SAMPLE_COS));
        assert_eq!(e.done(), 1);
        assert_eq!(e.offer(voice(0.9)), Offer::Accepted);
        assert_eq!(e.done(), 2);
    }

    #[test]
    fn rejects_in_a_row_restart_and_a_good_sample_resets_the_count() {
        let mut e = Enrollment::new("Мама".into());
        e.offer(voice(1.0));
        assert!(matches!(e.offer(voice(0.1)), Offer::Rejected(_)));
        assert!(matches!(e.offer(voice(0.1)), Offer::Rejected(_)));
        assert_eq!(e.offer(voice(1.0)), Offer::Accepted);
        assert!(matches!(e.offer(voice(0.1)), Offer::Rejected(_)));
        assert!(matches!(e.offer(voice(0.1)), Offer::Rejected(_)));
        assert_eq!(e.offer(voice(0.1)), Offer::Restarted);
        assert_eq!(e.done(), 0);
        // Первый образец новой записи сравнить не с чем.
        assert_eq!(e.offer(voice(0.1)), Offer::Accepted);
    }

    #[test]
    fn prompts_follow_progress_and_fit_a_state() {
        let p = prompt(&view(Some(2), EnrollNote::None), "Алиса");
        assert!(
            p.starts_with("Фраза 3 из 5. Скажите: «Алиса, как сказать"),
            "{p}"
        );
        let p = prompt(&view(Some(0), EnrollNote::Restarted), "Яндекс");
        assert!(p.starts_with("Фразы слишком разные"), "{p}");
        assert!(p.contains("«Яндекс, какая"), "{p}");
        let p = prompt(&view(None, EnrollNote::Saved("Мама".into())), "Алиса");
        assert_eq!(
            p,
            "Голос сохранён: Мама. Проверьте: «Алиса, как меня зовут?»"
        );
        // Предел длины состояния в HA — 255 символов, даже с самым длинным именем.
        let long = "я".repeat(32);
        for done in 0..ENROLL_UTTERANCES {
            for note in [
                EnrollNote::Rejected,
                EnrollNote::Restarted,
                EnrollNote::SoundsLike(long.clone()),
            ] {
                let p = prompt(&view(Some(done), note), "Алиса");
                assert!(p.chars().count() <= 255, "{p}");
            }
        }
    }

    fn chunk(amp: i16) -> Vec<i16> {
        vec![amp; 512]
    }

    fn chunks_until_end(e: &mut Endpointer, amp: i16, max: usize) -> Option<usize> {
        (0..max).find(|_| e.feed(&chunk(amp)))
    }

    #[test]
    fn phrase_ends_after_silence_but_not_before_the_minimum() {
        let mut e = Endpointer::new();
        // Короткое «Алиса» и пауза: до 2 с запись идёт.
        assert_eq!(chunks_until_end(&mut e, 5000, 10), None);
        let quiet = chunks_until_end(&mut e, 5, 200).expect("end");
        assert_eq!(
            quiet + 1 + 10,
            (PHRASE_MIN_MS * SAMPLE_RATE_MS).div_ceil(512)
        );
    }

    #[test]
    fn speech_resets_the_silence_and_the_limit_ends_endless_noise() {
        let mut e = Endpointer::new();
        assert_eq!(chunks_until_end(&mut e, 5000, 70), None);
        assert_eq!(chunks_until_end(&mut e, 5, 20), None);
        assert_eq!(chunks_until_end(&mut e, 5000, 10), None);
        let silence = chunks_until_end(&mut e, 5, 100).expect("end");
        assert_eq!(
            silence + 1,
            (PHRASE_SILENCE_MS * SAMPLE_RATE_MS).div_ceil(512)
        );
        let mut noise = Endpointer::new();
        let end = chunks_until_end(&mut noise, 5000, 1000).expect("limit");
        assert_eq!(
            end + 1,
            (PHRASE_MAX_MS as usize * SAMPLE_RATE_MS).div_ceil(512)
        );
    }
}
