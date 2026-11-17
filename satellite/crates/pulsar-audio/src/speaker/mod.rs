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
// Короче 0,6 с речи отпечаток ненадёжен: говорящий остаётся неизвестным. Секунды мало: в короткой
// команде после начала речи по VAD её 0,5-0,9 с (поле 2026-11-16).
const MIN_SPEECH_FRAMES: usize = 25;
// Кадры тише пика на столько дБ не речь: пауза и фон иначе смещают нормализацию.
const SPEECH_RANGE_DB: f32 = 30.0;
// И громче фона хотя бы на столько дБ: так из звука после начала речи уходят фон до неё и пауза до
// VAD_END. Больше нельзя: на Миди речь всего на 5-16 дБ громче фона, и при 10 дБ от команды
// в 1,2 с оставалось 0,4 с (поле 2026-11-15). Фон — 10-й процентиль энергии кадров.
const SPEECH_OVER_NOISE_DB: f32 = 4.0;
// Энергия кадра цифровой тишины: 10·lg(LOG_FLOOR).
const SILENCE_DB: f32 = -69.24;
// Шаг кадра в секундах, для журнала.
const FRAME_SECS: f32 = FRAME_SHIFT as f32 / SAMPLE_RATE;
// Итог по всей реплике не всегда успевает к агенту, поэтому оцениваем заранее, но не раньше,
// чем наберётся столько речи (кусков громче фона, см. [`SpeechTrack`]), и затем с шагом.
const EARLY_MIN_SPEECH_SAMPLES: usize = 1000 * 16;
const EARLY_STEP_SAMPLES: usize = 8000;
// Речи пока мало, поэтому порог выше обычного.
const EARLY_MIN_SCORE: f32 = 0.75;
const EARLY_MIN_LEAD: f32 = 0.10;
// Предел звука одной реплики: 30 с, дальше звук не копится.
const MAX_SAMPLES: usize = 30 * 16000;
// VAD из HA объявляет конец речи после своей паузы, и текст уходит агенту через 150-250 мс; итог,
// посчитанный только тогда, опаздывал к агенту на 40-100 мс. Поэтому итог считаем уже на паузе
// в речи дольше стольких мс; пауза короче паузы VAD из HA.
const PAUSE_SAMPLES: usize = 300 * 16;
// Итог на паузе — только после стольких мс речи. Прежде порогом были 2 с от начала реплики, но в
// них входят пре-ролл и сигнал станции: итог считался по секунде речи на первой запинке
// («Проверь, …») и выходил «unknown» при 0,51 (поле 2026-11-15).
const PAUSE_MIN_SPEECH_SAMPLES: usize = 1500 * 16;
// Кусок громче фона на столько дБ — речь; фон — самый тихий не цифровой кусок реплики.
const CHUNK_OVER_NOISE_DB: f32 = 6.0;
// Узнаём по звуку от начала речи по VAD из HA: громкость сама речь от фона не отличает, на Миди
// речь всего на 5-16 дБ громче фона (поле 2026-11-15). Без начала речи (одно активационное слово)
// говорящий неизвестен. VAD объявляет начало с опозданием, поэтому берём и столько мс до него.
const SPEECH_LEAD_SAMPLES: usize = 600 * 16;
// Если после итога на паузе было столько речи, итог на VAD_END считается заново.
const RESCORE_SPEECH_SAMPLES: usize = 500 * 16;

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

/// Кадры речи: не тише [`SPEECH_RANGE_DB`] от 95-го процентиля энергии и громче фона на
/// [`SPEECH_OVER_NOISE_DB`], не больше [`MAX_SPEECH_FRAMES`]; без вычитания среднего.
fn speech_frames(f: &Features) -> Vec<[f32; MEL_BINS]> {
    if f.frames.is_empty() {
        return Vec::new();
    }
    let mut sorted = f.energy_db.clone();
    sorted.sort_by(f32::total_cmp);
    let peak = sorted[(sorted.len() - 1) * 95 / 100];
    // Фон из цифровой тишины не считается: тогда порог задаёт только пик, как прежде.
    let noise = sorted[(sorted.len() - 1) / 10];
    let mut threshold = peak - SPEECH_RANGE_DB;
    if noise > SILENCE_DB + 1.0 {
        threshold = threshold.max(noise + SPEECH_OVER_NOISE_DB);
    }
    f.frames
        .iter()
        .zip(&f.energy_db)
        .filter(|(_, e)| **e > threshold)
        .map(|(frame, _)| *frame)
        .take(MAX_SPEECH_FRAMES)
        .collect()
}

/// Кадры речи (см. [`speech_frames`]) за вычетом среднего (CMN). Без CMN отпечаток зависит от
/// громкости и канала: замер — docs/voice.md, «Голосовая биометрия». None — речи слишком мало.
#[must_use]
pub fn select_speech(f: &Features) -> Option<Vec<[f32; MEL_BINS]>> {
    let speech = speech_frames(f);
    (speech.len() >= MIN_SPEECH_FRAMES).then(|| centered(speech))
}

fn centered(mut speech: Vec<[f32; MEL_BINS]>) -> Vec<[f32; MEL_BINS]> {
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
    speech
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
        self.embed_counted(pcm).map(|(emb, _)| emb)
    }

    /// Отпечаток и сколько кадров речи в нём (или нашлось, если речи мало).
    pub fn embed_counted(&mut self, pcm: &[i16]) -> Result<(Option<Vec<f32>>, usize), String> {
        let f = fbank(pcm);
        let speech = speech_frames(&f);
        let count = speech.len();
        if count < MIN_SPEECH_FRAMES {
            return Ok((None, count));
        }
        let speech = centered(speech);
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
        Ok((gallery::normalized(embedding), count))
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
            next: EARLY_MIN_SPEECH_SAMPLES,
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

/// Речь и паузы реплики по громкости кусков относительно фона: когда считать итог заранее.
struct SpeechTrack {
    /// Самый тихий кусок, кроме цифровой тишины, — фон.
    noise_db: f32,
    /// Уровень и длина каждого куска: речь до первой паузы пересчитывается, когда фон найден.
    chunks: Vec<(f32, usize)>,
    speech_samples: usize,
    quiet_samples: usize,
    /// Сколько речи было при последнем итоге на паузе, каким бы он ни был.
    tried_at: Option<usize>,
    /// Сколько речи было, когда на паузе узнан голос; `None` — не узнан.
    settled_at: Option<usize>,
    /// С какого куска речь по VAD из HA; до него речь и паузы не считаются.
    start: Option<usize>,
}

impl SpeechTrack {
    fn new() -> SpeechTrack {
        SpeechTrack {
            noise_db: f32::INFINITY,
            chunks: Vec::new(),
            speech_samples: 0,
            quiet_samples: 0,
            tried_at: None,
            settled_at: None,
            start: None,
        }
    }

    /// VAD из HA услышал речь: с этого куска считаем речь и паузы.
    fn start(&mut self) {
        if self.start.is_none() {
            self.start = Some(self.chunks.len());
        }
    }

    /// Следующий кусок звука; true — пауза после достаточной речи, пора посчитать итог.
    fn feed(&mut self, pcm: &[i16]) -> bool {
        let db = enroll::level_db(pcm);
        if pcm.is_empty() {
            return false;
        }
        if self.chunks.len() * 512 < MAX_SAMPLES {
            self.chunks.push((db, pcm.len()));
        }
        if db > SILENCE_DB + 1.0 && db < self.noise_db {
            self.noise_db = db;
            let threshold = db + CHUNK_OVER_NOISE_DB;
            let from = self
                .start
                .unwrap_or(self.chunks.len())
                .min(self.chunks.len());
            self.speech_samples = self.chunks[from..]
                .iter()
                .filter(|(level, _)| *level >= threshold)
                .map(|(_, len)| len)
                .sum();
        }
        if self.start.is_none() {
            return false;
        }
        if db >= self.noise_db + CHUNK_OVER_NOISE_DB {
            self.speech_samples += pcm.len();
            self.quiet_samples = 0;
            return false;
        }
        self.quiet_samples += pcm.len();
        self.quiet_samples >= PAUSE_SAMPLES
            && self.speech_samples >= PAUSE_MIN_SPEECH_SAMPLES
            && self.tried_at != Some(self.speech_samples)
    }

    /// Итог на этой паузе посчитан; `found` — голос узнан.
    /// Следующий итог — не раньше новой паузы: фон в паузе бывает ниже прежнего, речь тогда
    /// пересчитывается, и итог считался бы на каждом куске (4 раза за 0,4 с, поле 2026-11-16).
    fn scored(&mut self, found: bool) {
        self.tried_at = Some(self.speech_samples);
        self.quiet_samples = 0;
        if found {
            self.settled_at = Some(self.speech_samples);
        }
    }

    /// Итог на VAD_END считать заново: на паузе он не уходил или после него было много речи.
    fn rescore_at_end(&self) -> bool {
        self.settled_at
            .is_none_or(|at| self.speech_samples - at >= RESCORE_SPEECH_SAMPLES)
    }
}

enum Cmd {
    /// Номер реплики и образец ли она для записи голоса.
    Begin(u64, bool),
    Audio(Vec<i16>),
    /// VAD из HA услышал речь в реплике с этим номером.
    SpeechStart(u64),
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

    /// Начало речи по VAD из HA; повторный вызов ничего не делает.
    pub fn speech_start(&self) {
        self.send(Cmd::SpeechStart(self.seq.get()));
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

/// Кто говорит в `pcm`, и строка с оценками и длительностью речи для журнала.
fn identify(embedder: &mut Embedder, gallery: &Gallery, pcm: &[i16]) -> (Option<Match>, String) {
    match embedder.embed_counted(pcm) {
        Ok((Some(emb), frames)) => {
            let (found, detail) = gallery.identify(&emb);
            (
                found,
                format!("{detail}; speech {:.1} s", speech_secs(frames)),
            )
        }
        Ok((None, frames)) => (
            None,
            format!("too little speech ({:.1} s)", speech_secs(frames)),
        ),
        Err(e) => {
            warn!("speaker: {e}");
            (None, "too little speech".to_owned())
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn speech_secs(frames: usize) -> f32 {
    frames as f32 * FRAME_SECS
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
    let mut track = SpeechTrack::new();
    // С какого отсчёта `pcm` узнаём: начало речи по VAD из HA минус [`SPEECH_LEAD_SAMPLES`].
    let mut speech_from: Option<usize> = None;
    let mut last = String::new();
    let _ = tx.send(SpeakerEvent::Status(status_text(&gallery, None, &last)));
    let _ = tx.send(view(None, EnrollNote::None, None));
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Begin(seq, sample) => {
                active = Some((seq, sample));
                pcm.clear();
                early = EarlyCheck::new();
                track = SpeechTrack::new();
                speech_from = None;
            }
            Cmd::SpeechStart(seq) => {
                if active == Some((seq, false)) && speech_from.is_none() {
                    speech_from = Some(pcm.len().saturating_sub(SPEECH_LEAD_SAMPLES));
                    track.start();
                }
            }
            Cmd::Audio(chunk) => {
                if active.is_some() && pcm.len() < MAX_SAMPLES {
                    pcm.extend_from_slice(&chunk);
                }
                let paused = track.feed(&chunk);
                if let Some((seq, false)) = active
                    && let Some(from) = speech_from
                    && paused
                {
                    // Итог на паузе: VAD_END из HA приходит, когда текст уже почти у агента.
                    // «unknown» на паузе не окончателен: после неё речь могла продолжиться.
                    let (found, detail) = identify(&mut embedder, &gallery, &pcm[from..]);
                    track.scored(found.is_some());
                    last.clone_from(&detail);
                    let _ = tx.send(SpeakerEvent::Identified { seq, found, detail });
                    let _ = tx.send(SpeakerEvent::Status(status_text(
                        &gallery,
                        enrolling.as_ref(),
                        &last,
                    )));
                    continue;
                }
                if let Some((seq, false)) = active
                    && let Some(from) = speech_from
                    && track.settled_at.is_none()
                    && early.due(track.speech_samples)
                    && let Ok(Some(emb)) = embedder.embed(&pcm[from..])
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
                // Итог уже ушёл на паузе, и речи после неё было мало.
                if !sample && !track.rescore_at_end() {
                    pcm.clear();
                    continue;
                }
                if !sample {
                    let (found, detail) = match speech_from {
                        Some(from) => identify(&mut embedder, &gallery, &pcm[from..]),
                        None => (None, "no speech".to_owned()),
                    };
                    pcm.clear();
                    last.clone_from(&detail);
                    let _ = tx.send(SpeakerEvent::Identified { seq, found, detail });
                    let _ = tx.send(SpeakerEvent::Status(status_text(
                        &gallery,
                        enrolling.as_ref(),
                        &last,
                    )));
                    continue;
                }
                let embedding = embedder.embed(&pcm);
                pcm.clear();
                let embedding = match embedding {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("speaker: {e}");
                        None
                    }
                };
                // Запись могли отменить, пока шла фраза.
                if enrolling.is_none() {
                    continue;
                }
                let (note, sound, detail) =
                    enroll_sample(store, &mut gallery, &mut enrolling, embedding);
                info!("speaker: sample: {detail}");
                last = detail;
                let _ = tx.send(view(enrolling.as_ref(), note, Some(sound)));
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
        assert!(!e.due(EARLY_MIN_SPEECH_SAMPLES - 1));
        assert!(e.due(EARLY_MIN_SPEECH_SAMPLES));
        assert!(!e.due(EARLY_MIN_SPEECH_SAMPLES + 1));
        assert!(e.due(EARLY_MIN_SPEECH_SAMPLES + EARLY_STEP_SAMPLES));
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

    fn pause_after(t: &mut SpeechTrack, amp: i16, max: usize) -> Option<usize> {
        (0..max).find(|_| t.feed(&[amp; 512]))
    }

    /// Реплика, в которой VAD из HA уже услышал речь.
    fn started() -> SpeechTrack {
        let mut t = SpeechTrack::new();
        t.start();
        t
    }

    /// Куски по 512 отсчётов (32 мс): столько нужно на `ms` мс.
    fn chunks(ms: usize) -> usize {
        (ms * 16).div_ceil(512)
    }

    #[test]
    fn a_pause_counts_only_after_enough_speech() {
        let mut t = started();
        assert_eq!(pause_after(&mut t, 2, 20), None);
        // Секунда речи и пауза: мало, как «Проверь, …» на поле.
        assert_eq!(pause_after(&mut t, 300, chunks(1000)), None);
        assert_eq!(pause_after(&mut t, 2, 30), None);
        // Ещё полсекунды речи: теперь пауза — итог.
        assert_eq!(pause_after(&mut t, 300, chunks(600)), None);
        let quiet = pause_after(&mut t, 2, 30).expect("pause");
        assert_eq!(quiet + 1, PAUSE_SAMPLES.div_ceil(512));
    }

    #[test]
    fn a_found_voice_settles_and_new_speech_rescores_at_the_end() {
        let mut t = started();
        pause_after(&mut t, 2, 10);
        pause_after(&mut t, 300, chunks(1600));
        assert!(pause_after(&mut t, 2, 30).is_some());
        t.scored(true);
        // Тишина дальше итог не пересчитывает, и на VAD_END он тот же.
        assert_eq!(pause_after(&mut t, 2, 50), None);
        assert!(!t.rescore_at_end());
        // Короткое слово после паузы — тоже.
        pause_after(&mut t, 300, chunks(200));
        assert!(!t.rescore_at_end());
        pause_after(&mut t, 300, chunks(400));
        assert!(t.rescore_at_end());
    }

    #[test]
    fn unknown_at_a_pause_is_not_final() {
        let mut t = started();
        pause_after(&mut t, 2, 10);
        pause_after(&mut t, 300, chunks(1600));
        assert!(pause_after(&mut t, 2, 30).is_some());
        t.scored(false);
        // В той же паузе итог не считается на каждом куске.
        assert_eq!(pause_after(&mut t, 2, 50), None);
        assert!(t.rescore_at_end());
        // Речь после паузы: следующая пауза — снова итог.
        pause_after(&mut t, 300, chunks(100));
        assert!(pause_after(&mut t, 2, 30).is_some());
    }

    /// Фон в паузе тише прежнего: тихие куски до неё становятся речью, но итог не считается
    /// на каждом куске, а только после новой паузы.
    #[test]
    fn a_lower_floor_in_the_pause_waits_for_a_new_pause() {
        let mut t = started();
        pause_after(&mut t, 4, 5);
        pause_after(&mut t, 300, chunks(1600));
        assert!(pause_after(&mut t, 4, 30).is_some());
        t.scored(true);
        let next = pause_after(&mut t, 2, 50).expect("new pause");
        assert_eq!(next + 1, PAUSE_SAMPLES.div_ceil(512));
    }

    /// Речь сразу после сигнала станции, до первой тишины, тоже речь: фон найден позже.
    #[test]
    fn speech_before_the_first_quiet_chunk_counts() {
        let mut t = started();
        assert_eq!(pause_after(&mut t, 300, chunks(1600)), None);
        assert!(pause_after(&mut t, 2, 30).is_some());
    }

    /// Активационное слово без команды: в пре-ролле речь, дальше фон, VAD из HA речи не слышал.
    /// Ни речи, ни итога на паузе.
    #[test]
    fn nothing_counts_before_the_speech_start() {
        let mut t = SpeechTrack::new();
        assert_eq!(pause_after(&mut t, 300, chunks(2000)), None);
        assert_eq!(pause_after(&mut t, 2, chunks(14000)), None);
        assert_eq!(t.speech_samples, 0);
        // Речь после начала по VAD считается, слово до него — нет.
        t.start();
        assert_eq!(pause_after(&mut t, 300, chunks(1000)), None);
        assert_eq!(t.speech_samples, chunks(1000) * 512);
    }

    /// Тихая речь (после #81) и громкая дают паузу на одном и том же куске; цифровая тишина
    /// в паузах — тоже пауза.
    #[test]
    fn pause_does_not_depend_on_the_gain() {
        let at = |speech: i16, quiet: i16| {
            let mut t = started();
            pause_after(&mut t, quiet.max(1), 5);
            assert_eq!(pause_after(&mut t, speech, chunks(1600)), None);
            pause_after(&mut t, quiet, 50).expect("pause")
        };
        assert_eq!(at(15000, 100), at(300, 2));
        assert_eq!(at(300, 0), at(15000, 0));
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
    fn silence_db_is_the_energy_floor() {
        assert!((SILENCE_DB - 10.0 * LOG_FLOOR.log10()).abs() < 0.01);
        assert!((enroll::level_db(&[0; 512]) - SILENCE_DB).abs() < 0.01);
    }

    /// Шум размаха `amp`, `secs` секунд.
    fn noise(amp: f32, secs: f32) -> Vec<i16> {
        let mut seed = 7u32;
        (0..(secs * SAMPLE_RATE) as usize)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * amp).round() as i16
            })
            .collect()
    }

    /// Реплика с одним активационным словом при «Microphone gain» 0 (поле 2026-11-15): конец
    /// слова в пре-ролле и 14 с фона на 23 дБ тише речи. Фон — не речь, отпечатка нет.
    #[test]
    fn background_after_a_wake_word_is_not_speech() {
        let mut pcm = tone(300.0, 0.25, 37.0);
        pcm.extend(noise(6.2, 14.0));
        assert!(select_speech(&fbank(&pcm)).is_none());
        // Та же речь на том же фоне — речь.
        let mut pcm = noise(6.2, 1.0);
        pcm.extend(tone(300.0, 2.0, 37.0));
        pcm.extend(noise(6.2, 1.0));
        let speech = select_speech(&fbank(&pcm)).unwrap();
        assert!((70..=90).contains(&speech.len()), "{}", speech.len());
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
        // Реплика без единого тихого кадра нереальна: фоном тогда сочлась бы сама речь. В жизни
        // до команды есть пауза после сигнала станции, после — пауза до VAD_END.
        let mut long = vec![0i16; 3 * 16000];
        long.extend(tone(300.0, 20.0, 8000.0));
        let long = select_speech(&fbank(&long)).unwrap();
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
