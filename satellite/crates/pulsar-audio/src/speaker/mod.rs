//! Узнавание говорящего по голосу: ECAPA-TDNN из прошивки (`bio_lingware`, грузится со станции,
//! в репозиторий не копируется). Контракт модели — docs/voice.md, «Голосовая биометрия».
//!
//! Звук держим только в памяти, на диск идут только отпечатки.

mod enroll;
#[allow(unsafe_code)]
mod ffi;
mod gallery;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};

use log::{error, info, warn};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub use enroll::{
    ENROLL_EXPIRE_MS, ENROLL_UTTERANCES, Endpointer, EnrollNote, EnrollSound, EnrollView,
    PHRASE_MAX_MS, prompt,
};
use enroll::{Enrollment, Offer};
pub use gallery::{EMBEDDING_DIM, Gallery, MAX_NAME_CHARS, Match, valid_name};

const BODY: &str =
    "/system/vendor/quasar/bio_lingware/tflite-online-ecapa-tdnn-frontend/body.tflite";
const HEAD: &str =
    "/system/vendor/quasar/bio_lingware/tflite-online-ecapa-tdnn-frontend/head.tflite";

const SAMPLE_RATE: f32 = 16000.0;
// bio_lingware/features_config.json: окно 40 мс, шаг 24 мс, БПФ до степени двойки.
const FRAME_LEN: usize = 640;
const FRAME_SHIFT: usize = 384;
const FFT_LEN: usize = 1024;
const MEL_BINS: usize = 80;
const PREEMPHASIS: f32 = 0.97;
// Минимальное положительное float: как energy floor в Kaldi. Им же ограничена снизу энергия кадра:
// при пороге в 1 отсчёт² цифровая тишина в паузах тихой речи (VQE_OMNI без «Microphone gain»)
// оказывалась ближе 30 дБ к пику и шла в отпечаток как речь.
pub(crate) const LOG_FLOOR: f32 = 1.192_092_9e-7;
// Вход body.tflite: 20 кадров fbank; выход — 20 кадров по 128 признаков.
const BODY_FRAMES: usize = 20;
const BODY_DIM: usize = 128;
// "head-window-frames-size": 300 — 7,2 с речи.
const MAX_SPEECH_FRAMES: usize = 300;
// Короче секунды речи отпечаток ненадёжен: говорящий остаётся неизвестным.
const MIN_SPEECH_FRAMES: usize = 40;
// Кадры тише пика на столько дБ не речь: пауза и фон иначе смещают нормализацию.
const SPEECH_RANGE_DB: f32 = 30.0;
// Итог по всей реплике не всегда успевает к агенту, поэтому оцениваем заранее. Раньше 2 с в окне
// почти нет речи: конец активационного слова и сигнал станции.
const EARLY_FIRST_SAMPLES: usize = 32000;
const EARLY_STEP_SAMPLES: usize = 8000;
// Речи пока мало, поэтому порог выше обычного.
const EARLY_MIN_SCORE: f32 = 0.75;
const EARLY_MIN_LEAD: f32 = 0.10;
// Предел звука одной реплики: 30 с, дальше звук не копится.
const MAX_SAMPLES: usize = 30 * 16000;

/// Логарифмы мел-энергий по кадрам и энергия каждого кадра в дБ.
#[derive(Debug, PartialEq)]
pub struct Features {
    pub frames: Vec<[f32; MEL_BINS]>,
    pub energy_db: Vec<f32>,
}

struct Mel {
    /// (первый бин БПФ, веса) для каждого мел-фильтра.
    filters: Vec<(usize, Vec<f32>)>,
}

fn mel_scale(hz: f32) -> f32 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

impl Mel {
    fn new() -> Mel {
        let low = mel_scale(0.0);
        let delta = (mel_scale(SAMPLE_RATE / 2.0) - low) / (MEL_BINS + 1) as f32;
        let filters = (0..MEL_BINS)
            .map(|b| {
                let (left, center, right) = (
                    low + b as f32 * delta,
                    low + (b + 1) as f32 * delta,
                    low + (b + 2) as f32 * delta,
                );
                let mut first = 0;
                let mut weights = Vec::new();
                for k in 0..=FFT_LEN / 2 {
                    let m = mel_scale(k as f32 * SAMPLE_RATE / FFT_LEN as f32);
                    if m > left && m < right {
                        if weights.is_empty() {
                            first = k;
                        }
                        weights.push(if m <= center {
                            (m - left) / (center - left)
                        } else {
                            (right - m) / (right - center)
                        });
                    }
                }
                (first, weights)
            })
            .collect();
        Mel { filters }
    }

    fn apply(&self, power: &[f32], out: &mut [f32; MEL_BINS]) {
        for (o, (first, weights)) in out.iter_mut().zip(&self.filters) {
            let sum: f32 = weights
                .iter()
                .zip(&power[*first..])
                .map(|(w, p)| w * p)
                .sum();
            *o = sum.max(LOG_FLOOR).ln();
        }
    }
}

/// БПФ по основанию 2 на месте; длина — степень двойки.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = -2.0 * std::f32::consts::PI / len as f32;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (angle * k as f32).sin_cos();
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// Признаки как у Kaldi с параметрами bio_lingware/features_config.json, без dither (нули
/// отсекает [`select_speech`]). Шкала — отсчёты s16.
#[must_use]
pub fn fbank(pcm: &[i16]) -> Features {
    let mel = Mel::new();
    let window: Vec<f32> = (0..FRAME_LEN)
        .map(|i| {
            let x = 2.0 * std::f32::consts::PI * i as f32 / (FRAME_LEN - 1) as f32;
            0.54 - 0.46 * x.cos()
        })
        .collect();
    let count = pcm
        .len()
        .checked_sub(FRAME_LEN)
        .map_or(0, |rest| rest / FRAME_SHIFT + 1);
    let mut out = Features {
        frames: Vec::with_capacity(count),
        energy_db: Vec::with_capacity(count),
    };
    let mut re = vec![0.0f32; FFT_LEN];
    let mut im = vec![0.0f32; FFT_LEN];
    let mut power = vec![0.0f32; FFT_LEN / 2 + 1];
    for frame in pcm.windows(FRAME_LEN).step_by(FRAME_SHIFT) {
        let samples: Vec<f32> = frame.iter().map(|&s| f32::from(s)).collect();
        let mean = samples.iter().sum::<f32>() / FRAME_LEN as f32;
        let energy = samples.iter().map(|s| s * s).sum::<f32>() / FRAME_LEN as f32;
        re.fill(0.0);
        im.fill(0.0);
        let mut prev = samples[0] - mean;
        for i in 0..FRAME_LEN {
            let x = samples[i] - mean;
            re[i] = (x - PREEMPHASIS * prev) * window[i];
            prev = x;
        }
        fft(&mut re, &mut im);
        for (k, p) in power.iter_mut().enumerate() {
            *p = re[k] * re[k] + im[k] * im[k];
        }
        let mut bins = [0.0f32; MEL_BINS];
        mel.apply(&power, &mut bins);
        out.frames.push(bins);
        out.energy_db.push(10.0 * energy.max(LOG_FLOOR).log10());
    }
    out
}

/// Кадры речи: не тише [`SPEECH_RANGE_DB`] от 95-го процентиля энергии, не больше
/// [`MAX_SPEECH_FRAMES`]; из них вычтено среднее (CMN). Без CMN отпечаток зависит от громкости и
/// канала: замер — docs/voice.md, «Голосовая биометрия». None — речи слишком мало.
#[must_use]
pub fn select_speech(f: &Features) -> Option<Vec<[f32; MEL_BINS]>> {
    if f.frames.is_empty() {
        return None;
    }
    let mut sorted = f.energy_db.clone();
    sorted.sort_by(f32::total_cmp);
    let peak = sorted[(sorted.len() - 1) * 95 / 100];
    let mut speech: Vec<[f32; MEL_BINS]> = f
        .frames
        .iter()
        .zip(&f.energy_db)
        .filter(|(_, e)| **e > peak - SPEECH_RANGE_DB)
        .map(|(frame, _)| *frame)
        .take(MAX_SPEECH_FRAMES)
        .collect();
    if speech.len() < MIN_SPEECH_FRAMES {
        return None;
    }
    let mut mean = [0.0f32; MEL_BINS];
    for frame in &speech {
        for (m, v) in mean.iter_mut().zip(frame) {
            *m += v;
        }
    }
    for m in &mut mean {
        *m /= speech.len() as f32;
    }
    for frame in &mut speech {
        for (v, m) in frame.iter_mut().zip(&mean) {
            *v -= m;
        }
    }
    Some(speech)
}

/// Модели прошивки: звук -> голосовой отпечаток единичной длины.
pub struct Embedder {
    body: ffi::Interp,
    head: ffi::Interp,
}

impl Embedder {
    pub fn open() -> Result<Embedder, String> {
        Ok(Embedder {
            body: ffi::Interp::open(Path::new(BODY))?,
            head: ffi::Interp::open(Path::new(HEAD))?,
        })
    }

    /// Ok(None) — в звуке слишком мало речи.
    pub fn embed(&mut self, pcm: &[i16]) -> Result<Option<Vec<f32>>, String> {
        let Some(speech) = select_speech(&fbank(pcm)) else {
            return Ok(None);
        };
        let batches = speech.len() / BODY_FRAMES;
        let mut hidden = vec![0.0f32; batches * BODY_FRAMES * BODY_DIM];
        let mut input = vec![0.0f32; BODY_FRAMES * MEL_BINS];
        let (chunks, _) = speech.as_chunks::<BODY_FRAMES>();
        let (outs, _) = hidden.as_chunks_mut::<{ BODY_FRAMES * BODY_DIM }>();
        for (chunk, out) in chunks.iter().zip(outs) {
            let (rows, _) = input.as_chunks_mut::<MEL_BINS>();
            for (dst, frame) in rows.iter_mut().zip(chunk) {
                *dst = *frame;
            }
            self.body
                .run(&[1, BODY_FRAMES as i32, MEL_BINS as i32], &input, out)?;
        }
        let frames = i32::try_from(batches * BODY_FRAMES).map_err(|e| e.to_string())?;
        let mut embedding = vec![0.0f32; EMBEDDING_DIM];
        self.head
            .run(&[1, frames, BODY_DIM as i32], &hidden, &mut embedding)?;
        Ok(gallery::normalized(embedding))
    }
}

/// Файлы моделей на месте; на других прошивках узнавания нет, как и его сущностей.
#[must_use]
pub fn available() -> bool {
    Path::new(BODY).is_file() && Path::new(HEAD).is_file()
}

/// События рабочего потока. `seq` — из [`SpeakerHandle::begin`]: устаревшие контроллер игнорирует.
#[derive(Debug)]
pub enum SpeakerEvent {
    Identified {
        seq: u64,
        found: Option<Match>,
        detail: String,
    },
    /// Оценка по началу реплики изменилась; `None` снимает имя. Итог по всей реплике придёт позже
    /// и может заменить имя.
    Early {
        seq: u64,
        found: Option<Match>,
    },
    Status(String),
    Enroll(EnrollView),
}

/// Когда пробовать раннее узнавание и какое имя уже опубликовано.
struct EarlyCheck {
    next: usize,
    shown: Option<String>,
}

impl EarlyCheck {
    fn new() -> EarlyCheck {
        EarlyCheck {
            next: EARLY_FIRST_SAMPLES,
            shown: None,
        }
    }

    fn due(&mut self, samples: usize) -> bool {
        if samples < self.next {
            return false;
        }
        self.next = samples + EARLY_STEP_SAMPLES;
        true
    }

    /// Что опубликовать; `Some(None)` снимает имя. Новое имя — только при уверенной оценке. Тот же
    /// голос, но с меньшей уверенностью, имя не снимает.
    fn update(&mut self, found: Option<Match>) -> Option<Option<Match>> {
        let found = found.filter(|m| {
            let same = self.shown.as_deref() == Some(m.name.as_str());
            same || (m.score >= EARLY_MIN_SCORE && m.lead >= EARLY_MIN_LEAD)
        });
        let name = found.as_ref().map(|m| m.name.clone());
        if name == self.shown {
            return None;
        }
        self.shown = name;
        Some(found)
    }
}

enum Cmd {
    /// Номер реплики и образец ли она для записи голоса.
    Begin(u64, bool),
    Audio(Vec<i16>),
    End(u64),
    Enroll(String),
    /// Отмена записи; true — по истечении срока.
    Cancel(bool),
    Forget(String),
}

#[derive(Clone)]
pub struct SpeakerHandle {
    tx: Option<Sender<Cmd>>,
    seq: Rc<Cell<u64>>,
}

impl SpeakerHandle {
    /// Начало реплики. Возвращает номер, с которым придёт её результат.
    pub fn begin(&self) -> u64 {
        self.start(false)
    }

    /// Начало фразы для записи голоса: её результат — образец, а не узнавание.
    pub fn begin_sample(&self) -> u64 {
        self.start(true)
    }

    fn start(&self, sample: bool) -> u64 {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        self.send(Cmd::Begin(seq, sample));
        seq
    }

    pub fn audio(&self, pcm: Vec<i16>) {
        self.send(Cmd::Audio(pcm));
    }

    /// Повторный вызов ничего не делает: приходят и VAD_END, и STT_END.
    pub fn end(&self) {
        self.send(Cmd::End(self.seq.get()));
    }

    /// Следующие [`ENROLL_UTTERANCES`] принятых фраз становятся голосом `name`; прежний голос
    /// с этим именем заменяется только после последней.
    pub fn enroll(&self, name: String) {
        self.send(Cmd::Enroll(name));
    }

    pub fn cancel(&self, expired: bool) {
        self.send(Cmd::Cancel(expired));
    }

    pub fn forget(&self, name: String) {
        self.send(Cmd::Forget(name));
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }
}

/// Модели грузятся в потоке; если не загрузились, команды игнорируются. Поток не ждём: он
/// завершится сам, когда пропадут все handle.
pub fn spawn(store: PathBuf) -> (SpeakerHandle, UnboundedReceiver<SpeakerEvent>) {
    let (tx, rx) = channel();
    let (ev_tx, ev_rx) = unbounded_channel();
    let spawned = std::thread::Builder::new()
        .name("speaker".into())
        .spawn(move || worker(&store, &rx, &ev_tx));
    let tx = match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            error!("speaker thread: {e}");
            None
        }
    };
    (
        SpeakerHandle {
            tx,
            seq: Rc::default(),
        },
        ev_rx,
    )
}

/// Текст сущности «Speaker status»: ход обучения, записанные голоса, оценки последней реплики
/// (по ним настраивают порог). Предел длины строки состояния в HA — 255 символов.
fn status_text(gallery: &Gallery, enrolling: Option<&Enrollment>, last: &str) -> String {
    let names = gallery.names().join(", ");
    let text = match enrolling {
        Some(e) => format!(
            "enrolling {}: {}/{ENROLL_UTTERANCES}; known: {names}; last: {last}",
            e.name,
            e.done()
        ),
        None => format!("known: {names}; last: {last}"),
    };
    text.chars().take(255).collect()
}

fn view(
    enrolling: Option<&Enrollment>,
    note: EnrollNote,
    sound: Option<EnrollSound>,
) -> SpeakerEvent {
    SpeakerEvent::Enroll(EnrollView {
        active: enrolling.map(|e| (e.name.clone(), e.done())),
        note,
        sound,
    })
}

/// Образец записи. Возвращает пометку и звук для подсказки и текст «last» для статуса.
fn enroll_sample(
    store: &Path,
    gallery: &mut Gallery,
    enrolling: &mut Option<Enrollment>,
    embedding: Option<Vec<f32>>,
) -> (EnrollNote, EnrollSound, String) {
    let Some(emb) = embedding else {
        return (
            EnrollNote::TooShort,
            EnrollSound::Rejected,
            "too little speech".to_owned(),
        );
    };
    let Some(e) = enrolling.as_mut() else {
        return (EnrollNote::None, EnrollSound::Rejected, String::new());
    };
    // До сохранения: с образцом в записанном голосе сходство было бы завышено.
    let (found, detail) = gallery.identify(&emb);
    match e.offer(emb) {
        Offer::Accepted => {
            // Только предупреждаем: голоса родственников бывают похожи, и отказ не дал бы их
            // записать.
            let note = match found {
                Some(m) if m.name != e.name => EnrollNote::SoundsLike(m.name),
                _ => EnrollNote::None,
            };
            (note, EnrollSound::Accepted, detail)
        }
        Offer::Rejected(score) => (
            EnrollNote::Rejected,
            EnrollSound::Rejected,
            format!("rejected {score:.2}"),
        ),
        Offer::Restarted => (
            EnrollNote::Restarted,
            EnrollSound::Rejected,
            "restarted".to_owned(),
        ),
        Offer::Done => {
            let name = e.name.clone();
            let before = gallery.clone();
            gallery.add(&name, &e.samples);
            let saved = gallery.save(store);
            *enrolling = None;
            if let Err(err) = saved {
                // Иначе голос пропал бы при перезапуске, хотя запись объявлена удачной.
                error!("speaker: {err}");
                *gallery = before;
                return (
                    EnrollNote::SaveFailed,
                    EnrollSound::Rejected,
                    "save failed".to_owned(),
                );
            }
            (
                EnrollNote::Saved(name),
                EnrollSound::Saved,
                "saved".to_owned(),
            )
        }
    }
}

/// false — файл не перезаписан: голос остаётся и в памяти, иначе он выглядел бы удалённым, но
/// вернулся бы при перезапуске.
fn forget_voice(store: &Path, gallery: &mut Gallery, name: &str) -> bool {
    let before = gallery.clone();
    if gallery.remove(name)
        && let Err(err) = gallery.save(store)
    {
        error!("speaker: {err}");
        *gallery = before;
        return false;
    }
    true
}

fn worker(store: &Path, rx: &Receiver<Cmd>, tx: &UnboundedSender<SpeakerEvent>) {
    let mut gallery = Gallery::load(store);
    let mut embedder = match Embedder::open() {
        Ok(e) => {
            info!("speaker: {} known voices", gallery.names().len());
            e
        }
        Err(e) => {
            error!("speaker: {e}");
            return;
        }
    };
    // Только в памяти: перезапуск демона отменяет запись, прежний голос остаётся.
    let mut enrolling: Option<Enrollment> = None;
    // Номер идущей реплики и образец ли она.
    let mut active: Option<(u64, bool)> = None;
    let mut pcm: Vec<i16> = Vec::new();
    let mut early = EarlyCheck::new();
    let mut last = String::new();
    let _ = tx.send(SpeakerEvent::Status(status_text(&gallery, None, &last)));
    let _ = tx.send(view(None, EnrollNote::None, None));
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Begin(seq, sample) => {
                active = Some((seq, sample));
                pcm.clear();
                early = EarlyCheck::new();
            }
            Cmd::Audio(chunk) => {
                if active.is_some() && pcm.len() < MAX_SAMPLES {
                    pcm.extend_from_slice(&chunk);
                }
                if let Some((seq, false)) = active
                    && early.due(pcm.len())
                    && let Ok(Some(emb)) = embedder.embed(&pcm)
                    && let Some(found) = early.update(gallery.identify(&emb).0)
                {
                    match &found {
                        Some(m) => info!("speaker: early {} {:.2}", m.name, m.score),
                        None => info!("speaker: early guess withdrawn"),
                    }
                    let _ = tx.send(SpeakerEvent::Early { seq, found });
                }
            }
            Cmd::End(seq) => {
                let Some((current, sample)) = active else {
                    continue;
                };
                if current != seq {
                    continue;
                }
                active = None;
                let embedding = embedder.embed(&pcm);
                pcm.clear();
                let embedding = match embedding {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("speaker: {e}");
                        None
                    }
                };
                if sample {
                    // Запись могли отменить, пока шла фраза.
                    if enrolling.is_none() {
                        continue;
                    }
                    let (note, sound, detail) =
                        enroll_sample(store, &mut gallery, &mut enrolling, embedding);
                    info!("speaker: sample: {detail}");
                    last = detail;
                    let _ = tx.send(view(enrolling.as_ref(), note, Some(sound)));
                } else {
                    let (found, detail) = match &embedding {
                        Some(emb) => gallery.identify(emb),
                        None => (None, "too little speech".to_owned()),
                    };
                    last.clone_from(&detail);
                    let _ = tx.send(SpeakerEvent::Identified { seq, found, detail });
                }
                let _ = tx.send(SpeakerEvent::Status(status_text(
                    &gallery,
                    enrolling.as_ref(),
                    &last,
                )));
            }
            Cmd::Enroll(name) => {
                enrolling = Some(Enrollment::new(name));
                let _ = tx.send(view(enrolling.as_ref(), EnrollNote::None, None));
                let _ = tx.send(SpeakerEvent::Status(status_text(
                    &gallery,
                    enrolling.as_ref(),
                    &last,
                )));
            }
            Cmd::Cancel(expired) => {
                if enrolling.take().is_none() {
                    continue;
                }
                let note = if expired {
                    EnrollNote::Expired
                } else {
                    EnrollNote::Cancelled
                };
                let _ = tx.send(view(None, note, None));
                let _ = tx.send(SpeakerEvent::Status(status_text(&gallery, None, &last)));
            }
            Cmd::Forget(name) => {
                if enrolling.as_ref().is_some_and(|e| e.name == name) {
                    enrolling = None;
                    let _ = tx.send(view(None, EnrollNote::Cancelled, None));
                }
                if !forget_voice(store, &mut gallery, &name) {
                    let _ = tx.send(view(enrolling.as_ref(), EnrollNote::ForgetFailed, None));
                }
                let _ = tx.send(SpeakerEvent::Status(status_text(
                    &gallery,
                    enrolling.as_ref(),
                    &last,
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guess(name: &str, score: f32, lead: f32) -> Option<Match> {
        Some(Match {
            name: name.to_owned(),
            score,
            lead,
        })
    }

    fn name(update: Option<Option<Match>>) -> Option<Option<String>> {
        update.map(|found| found.map(|m| m.name))
    }

    const NO_STORE: &str = "/nonexistent/speakers.conf";

    fn unit_voice() -> Vec<f32> {
        let mut v = vec![0.0; EMBEDDING_DIM];
        v[0] = 1.0;
        v
    }

    #[test]
    fn failed_save_ends_the_recording_and_keeps_the_old_gallery() {
        let mut gallery = Gallery::default();
        gallery.add("old", &[unit_voice()]);
        let mut e = Enrollment::new("new".to_owned());
        for _ in 1..ENROLL_UTTERANCES {
            e.offer(unit_voice());
        }
        let mut enrolling = Some(e);
        let (note, sound, _) = enroll_sample(
            Path::new(NO_STORE),
            &mut gallery,
            &mut enrolling,
            Some(unit_voice()),
        );
        assert_eq!(note, EnrollNote::SaveFailed);
        assert_eq!(sound, EnrollSound::Rejected);
        assert!(enrolling.is_none());
        assert_eq!(gallery.names(), ["old"]);
    }

    #[test]
    fn failed_forget_keeps_the_voice() {
        let mut gallery = Gallery::default();
        gallery.add("a", &[unit_voice()]);
        assert!(!forget_voice(Path::new(NO_STORE), &mut gallery, "a"));
        assert_eq!(gallery.names(), ["a"]);
        // Такого голоса нет: файл не трогаем, и это не сбой.
        assert!(forget_voice(Path::new(NO_STORE), &mut gallery, "b"));
    }

    #[test]
    fn handle_clones_share_the_sequence() {
        let a = SpeakerHandle {
            tx: None,
            seq: Rc::default(),
        };
        let b = a.clone();
        assert_eq!(a.begin(), 1);
        assert_eq!(b.begin_sample(), 2);
        assert_eq!(a.begin(), 3);
    }

    #[test]
    fn early_check_waits_and_keeps_stepping() {
        let mut e = EarlyCheck::new();
        assert!(!e.due(EARLY_FIRST_SAMPLES - 1));
        assert!(e.due(EARLY_FIRST_SAMPLES));
        assert!(!e.due(EARLY_FIRST_SAMPLES + 1));
        assert!(e.due(EARLY_FIRST_SAMPLES + EARLY_STEP_SAMPLES));
        assert!(e.due(MAX_SAMPLES));
    }

    #[test]
    fn early_guess_needs_score_and_lead() {
        let mut e = EarlyCheck::new();
        assert_eq!(name(e.update(guess("a", 0.74, 0.5))), None);
        assert_eq!(name(e.update(guess("a", 0.9, 0.05))), None);
        assert_eq!(
            name(e.update(guess("a", 0.75, 0.1))),
            Some(Some("a".into()))
        );
    }

    #[test]
    fn early_guess_is_corrected_and_published_once() {
        let mut e = EarlyCheck::new();
        assert_eq!(
            name(e.update(guess("a", 0.81, 0.2))),
            Some(Some("a".into()))
        );
        assert_eq!(name(e.update(guess("a", 0.85, 0.3))), None);
        // Менее уверенная оценка того же голоса имя не снимает.
        assert_eq!(name(e.update(guess("a", 0.7, 0.05))), None);
        assert_eq!(
            name(e.update(guess("b", 0.81, 0.4))),
            Some(Some("b".into()))
        );
    }

    #[test]
    fn early_guess_is_withdrawn_when_another_voice_is_unsure() {
        let mut e = EarlyCheck::new();
        assert_eq!(
            name(e.update(guess("a", 0.81, 0.2))),
            Some(Some("a".into()))
        );
        assert_eq!(name(e.update(guess("b", 0.7, 0.05))), Some(None));
        assert_eq!(name(e.update(None)), None);
        let mut e = EarlyCheck::new();
        assert_eq!(
            name(e.update(guess("a", 0.81, 0.2))),
            Some(Some("a".into()))
        );
        assert_eq!(name(e.update(None)), Some(None));
    }

    fn tone(hz: f32, secs: f32, amp: f32) -> Vec<i16> {
        (0..(secs * SAMPLE_RATE) as usize)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE;
                ((2.0 * std::f32::consts::PI * hz * t).sin() * amp) as i16
            })
            .collect()
    }

    #[test]
    fn frame_count_follows_snip_edges() {
        assert!(fbank(&[0; FRAME_LEN - 1]).frames.is_empty());
        assert_eq!(fbank(&[0; FRAME_LEN]).frames.len(), 1);
        assert_eq!(fbank(&[0; FRAME_LEN + FRAME_SHIFT]).frames.len(), 2);
    }

    #[test]
    fn tone_peaks_in_the_mel_bin_of_its_frequency() {
        let frames = fbank(&tone(1000.0, 0.1, 8000.0)).frames;
        let frame = &frames[0];
        let top = (0..MEL_BINS)
            .max_by(|&a, &b| frame[a].total_cmp(&frame[b]))
            .unwrap();
        let (first, weights) = &Mel::new().filters[top];
        let bin_1k = (1000.0 * FFT_LEN as f32 / SAMPLE_RATE) as usize;
        assert!((*first..first + weights.len()).contains(&bin_1k));
    }

    #[test]
    fn fft_matches_the_dft_of_an_impulse_and_a_cosine() {
        let mut re = vec![0.0; 8];
        let mut im = vec![0.0; 8];
        re[0] = 1.0;
        fft(&mut re, &mut im);
        assert!(re.iter().all(|v| (v - 1.0).abs() < 1e-6));
        let mut re: Vec<f32> = (0..8)
            .map(|i| (std::f32::consts::FRAC_PI_4 * i as f32).cos())
            .collect();
        let mut im = vec![0.0; 8];
        fft(&mut re, &mut im);
        assert!((re[1] - 4.0).abs() < 1e-5 && (re[7] - 4.0).abs() < 1e-5);
        assert!(re[3].abs() < 1e-5);
    }

    #[test]
    fn short_or_empty_speech_gives_nothing() {
        assert!(select_speech(&fbank(&[])).is_none());
        assert!(select_speech(&fbank(&tone(300.0, 0.5, 8000.0))).is_none());
    }

    #[test]
    fn speech_selection_drops_silence_caps_length_and_centers() {
        let mut pcm = vec![0i16; 16000];
        pcm.extend(tone(300.0, 2.0, 8000.0));
        pcm.extend(vec![0i16; 16000]);
        let speech = select_speech(&fbank(&pcm)).unwrap();
        // 2 с речи при шаге 24 мс — около 83 кадров, краевые кадры могут попасть.
        assert!((70..=90).contains(&speech.len()), "{}", speech.len());
        let mean: f32 = speech.iter().map(|f| f[5]).sum::<f32>() / speech.len() as f32;
        assert!(mean.abs() < 1e-3);
        let long = select_speech(&fbank(&tone(300.0, 20.0, 8000.0))).unwrap();
        assert_eq!(long.len(), MAX_SPEECH_FRAMES);
    }
    /// Слоги по 0,3 с через 0,2 с пауз, с шумом размаха `noise_amp`.
    fn syllables(voice_amp: f32, noise_amp: f32) -> Vec<i16> {
        let mut seed = 1u32;
        (0..4 * 16000)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let t = i as f32 / SAMPLE_RATE;
                let voice = if (t % 0.5) < 0.3 {
                    (2.0 * std::f32::consts::PI * 220.0 * t).sin()
                        + (2.0 * std::f32::consts::PI * 1800.0 * t).sin() * 0.3
                } else {
                    0.0
                };
                (voice * voice_amp + noise * noise_amp).round() as i16
            })
            .collect()
    }

    /// После #81 в узнавание идёт VQE_OMNI с «Microphone gain»: на Миди при 0 дБ речь около
    /// -62 dBFS (rms ~26), фон около -85 dBFS, а голоса записаны на прежнем выходе, в 50 раз
    /// громче. Кадры речи те же, признаки после CMN почти совпадают.
    #[test]
    fn quiet_speech_gives_the_features_of_loud_speech() {
        let quiet = select_speech(&fbank(&syllables(37.0, 6.2))).unwrap();
        let loud = select_speech(&fbank(&syllables(37.0 * 50.0, 6.2 * 50.0))).unwrap();
        assert_eq!(quiet.len(), loud.len());
        let diffs: Vec<f32> = quiet
            .iter()
            .zip(&loud)
            .flat_map(|(q, l)| q.iter().zip(l).map(|(a, b)| (a - b).abs()))
            .collect();
        let mean = diffs.iter().sum::<f32>() / diffs.len() as f32;
        assert!(mean < 0.15, "{mean}");
    }

    /// Цифровая тишина в паузах тихой речи — не речь, как и в паузах громкой.
    #[test]
    fn digital_silence_after_quiet_speech_is_not_speech() {
        let quiet = select_speech(&fbank(&syllables(37.0, 0.0))).unwrap();
        let loud = select_speech(&fbank(&syllables(37.0 * 50.0, 0.0))).unwrap();
        assert_eq!(quiet.len(), loud.len());
    }
}
