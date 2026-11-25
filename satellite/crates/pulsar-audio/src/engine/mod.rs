//! VQE и спотеры из libyandex_evil.so прошивки. Безопасные обёртки [`Lib`], [`Vqe`],
//! [`SpotterConf`], [`Spotter`], таблицы каналов спотера и [`spotter_layout`] открыты утилите
//! ww_offline; `Engine` обслуживает фронтенд демона.

#[allow(unsafe_code)]
mod ffi;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use log::{error, info, warn};

pub use ffi::{Lib, LoadError, Spotter, SpotterConf, Vqe, VqeStep, preload_deps};

pub const VQE_MODELS: &str = "/system/vendor/quasar/vqe_models";

/// Файлы, без которых каталог не считается моделью спотера.
// см. docs/hardware.md, «Библиотеки Яндекса (C API)»
pub const MODEL_FILES: [&str; 2] = ["acoustic_model.nnet", "flags.txt"];

// voicetech.AudioInputChannelType (VQE output channel types);
// см. docs/hardware.md, «Библиотеки Яндекса (C API)».
pub const RAW_MAIN_MIC_SYNC: i32 = 2;
pub const RAW_AUX_MIC_SYNC: i32 = 3;
pub const RAW_FEEDBACK_SYNC: i32 = 5;
pub const VQE_OMNI: i32 = 6;
pub const VQE_BEAMFORMING: i32 = 7;
pub const VQE_BNR: i32 = 8;
pub const VQE_OMNI_WITH_GAIN: i32 = 9;

/// Тип канала спотера (0..5) -> выход VQE: контракт библиотеки, общий для всех моделей.
/// Командная модель на VQE_OMNI_WITH_GAIN не срабатывала (ww_offline: 0/14 против 10/14 на
/// VQE_OMNI). См. docs/hardware.md, «Библиотеки Яндекса (C API)».
pub const SPOTTER_TO_VQE: [i32; 6] = [
    VQE_OMNI,
    VQE_BEAMFORMING,
    RAW_FEEDBACK_SYNC,
    VQE_BNR,
    RAW_MAIN_MIC_SYNC,
    RAW_AUX_MIC_SYNC,
];

/// Раскладка wake-спотера pulsar до таблицы библиотеки (с ней wake word работал на всех
/// моделях): запасная, когда в пресете VQE нет выхода из [`SPOTTER_TO_VQE`]. Командный спотер
/// получал на тип 0 VQE_OMNI (на VQE_OMNI_WITH_GAIN он не срабатывает): его тип 0 в запасной
/// раскладке задаёт `Slot::init`.
pub const LEGACY_SPOTTER_TO_VQE: [i32; 6] = [
    VQE_OMNI_WITH_GAIN,
    RAW_MAIN_MIC_SYNC,
    VQE_OMNI_WITH_GAIN,
    RAW_MAIN_MIC_SYNC,
    VQE_OMNI,
    VQE_OMNI,
];

/// Channel types a spotter config can ask for (0..5).
pub const SPOTTER_TYPES: usize = SPOTTER_TO_VQE.len();
// A type asking for this many channels or more gets none (the C++ daemon's guard).
const TYPE_CHANNELS_LIMIT: i64 = 8;
const MAX_SPOTTER_CHANNELS: usize = SPOTTER_TYPES * (TYPE_CHANNELS_LIMIT as usize - 1);

// События спотера (event_id_to_str): 2 / 5 — подтверждения (хиты); 0/4/7/9 — нужны данные /
// конец.
const EV_CONFIRM_1ST: i32 = 2;
const EV_CONFIRM_2ND: i32 = 5;
const EVENTS_STOP_MASK: u32 = (1 << 0) | (1 << 4) | (1 << 7) | (1 << 9);
const MAX_EVENTS: usize = 16;
// One hit per utterance.
const DEBOUNCE_MS: i64 = 1200;

/// Rate the spotters are fed at: the VQE output.
pub const SPOTTER_RATE: i32 = 16000;

/// Spotter input channels: per channel its spotter type and the VQE output feeding it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpotterLayout {
    pub types: Vec<i32>,
    pub vqe: Vec<i32>,
}

/// Раскладка каналов по числу каналов каждого типа в конфиге спотера
/// (spotter_conf_get_required_channel_count) и таблице тип -> выход VQE.
pub fn spotter_layout(
    counts: &[i64; SPOTTER_TYPES],
    mapping: &[i32; SPOTTER_TYPES],
) -> SpotterLayout {
    let mut l = SpotterLayout::default();
    for (t, &n) in counts.iter().enumerate() {
        if !(0..TYPE_CHANNELS_LIMIT).contains(&n) {
            continue;
        }
        for _ in 0..n {
            l.types.push(t as i32);
            l.vqe.push(mapping[t]);
        }
    }
    l
}

fn vqe_name(ty: i32) -> &'static str {
    match ty {
        RAW_MAIN_MIC_SYNC => "main_mic_sync",
        RAW_AUX_MIC_SYNC => "aux_mic_sync",
        RAW_FEEDBACK_SYNC => "feedback_sync",
        VQE_OMNI => "omni",
        VQE_BEAMFORMING => "beamforming",
        VQE_BNR => "bnr",
        VQE_OMNI_WITH_GAIN => "omni_with_gain",
        _ => "?",
    }
}

impl std::fmt::Display for SpotterLayout {
    /// "0<-omni(6),3<-bnr(8),4<-main_mic_sync(2)"
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, (t, v)) in self.types.iter().zip(&self.vqe).enumerate() {
            if i > 0 {
                f.write_char(',')?;
            }
            write!(f, "{t}<-{}({v})", vqe_name(*v))?;
        }
        Ok(())
    }
}

/// VQE sample in [-1, 1] -> s16 as the C++ daemon converts it (clamped, lrint: ties to even).
pub fn to_s16(v: f32) -> i16 {
    let v = v * 32768.0;
    if v > 32767.0 {
        32767
    } else if v < -32768.0 {
        -32768
    } else {
        v.round_ties_even() as i16
    }
}

/// Усиление в дБ -> множитель отсчётов перед [`to_s16`].
pub fn db_to_gain(db: u32) -> f32 {
    10f32.powf(db as f32 / 20.0)
}

/// Where the events of one fed chunk come from (a spotter, or a script in tests).
pub(crate) trait EventSource {
    /// Next event; None after an error (logged).
    fn next_event(&mut self) -> Option<i32>;
    /// Phrase of the current event; "" when the library has none.
    fn phrase(&mut self) -> String;
    /// spotter_get_unhandled_nbytes текущего хита; None без символа или после ошибки (в логе).
    fn unhandled(&mut self) -> Option<i32>;
}

/// Подтверждённая фраза и точка хита спотера.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Hit {
    pub phrase: String,
    /// spotter_get_unhandled_nbytes сразу после подтверждения; None: не читали, нет символа
    /// или ошибка.
    pub unhandled: Option<i32>,
    /// Сэмплов на канал от точки хита до конца поданного звука; None при `unhandled` None,
    /// отрицательном или не кратном кадру всех каналов.
    pub after: Option<usize>,
}

/// Байты чередованного s16 всех `nch` каналов → сэмплов на канал; None, если значение
/// отрицательно или не делится на целые кадры.
fn anchor_samples(unhandled: Option<i32>, nch: usize) -> Option<usize> {
    let v = usize::try_from(unhandled?).ok()?;
    let frame = 2 * nch;
    (frame != 0 && v % frame == 0).then(|| v / frame)
}

/// Читает события одного поданного чанка (не больше 16, до события остановки): подтверждённые
/// фразы, по одной на высказывание (`DEBOUNCE_MS` после предыдущего хита). При `nch` Some у
/// каждого хита читается якорь: сразу после phrase и до следующего события, как в `FeedData`
/// библиотеки, — в другой момент библиотека делает abort.
pub(crate) fn drain_events(
    src: &mut impl EventSource,
    what: &str,
    nch: Option<usize>,
    last_hit_ms: &mut i64,
    mut now_ms: impl FnMut() -> i64,
) -> Vec<Hit> {
    let mut out = Vec::new();
    for _ in 0..MAX_EVENTS {
        let Some(ev) = src.next_event() else { break };
        if !(0..=9).contains(&ev) || (EVENTS_STOP_MASK >> ev) & 1 != 0 {
            break;
        }
        if ev != EV_CONFIRM_1ST && ev != EV_CONFIRM_2ND {
            continue;
        }
        let phrase = src.phrase();
        let now = now_ms();
        if now - *last_hit_ms < DEBOUNCE_MS {
            continue;
        }
        *last_hit_ms = now;
        let unhandled = nch.and_then(|_| src.unhandled());
        let after = nch.and_then(|n| anchor_samples(unhandled, n));
        if let (Some(n), Some(v @ 0..), None) = (nch, unhandled, after) {
            warn!("{what}: unhandled bytes {v} are not whole {n}-channel frames");
        }
        info!("{what} detected: '{phrase}' (event {ev})");
        out.push(Hit {
            phrase,
            unhandled,
            after,
        });
    }
    out
}

struct Logged<'a>(&'a mut Spotter);

impl EventSource for Logged<'_> {
    fn next_event(&mut self) -> Option<i32> {
        self.0
            .next_event()
            .inspect_err(|e| error!("spotter_get_next_event: {e}"))
            .ok()
    }

    fn phrase(&mut self) -> String {
        let (phrase, err) = self.0.phrase();
        if let Some(e) = err {
            error!("spotter_get_current_phrase: {e}");
        }
        phrase.unwrap_or_default()
    }

    fn unhandled(&mut self) -> Option<i32> {
        match self.0.unhandled_bytes()? {
            Ok(v) => Some(v),
            Err(e) => {
                error!("spotter_get_unhandled_nbytes: {e}");
                None
            }
        }
    }
}

/// One spotter of the daemon with its channel layout.
pub(crate) struct Slot {
    what: &'static str,
    /// Читать якорь хита (только у спотера wake word).
    anchor: bool,
    sp: Option<Spotter>,
    /// Каталог последней загруженной модели. Нужен и после неудачного пересоздания.
    dir: PathBuf,
    layout: SpotterLayout,
    legacy: SpotterLayout,
    buf: Vec<i16>,
    last_hit_ms: i64,
}

impl Slot {
    fn new(what: &'static str, anchor: bool) -> Self {
        Slot {
            what,
            anchor,
            sp: None,
            dir: PathBuf::new(),
            layout: SpotterLayout::default(),
            legacy: SpotterLayout::default(),
            buf: Vec::new(),
            last_hit_ms: 0,
        }
    }

    /// (Re)creates the spotter from the model in `dir`; the previous one is destroyed first.
    /// `legacy_main` — выход VQE для типа 0 в запасной раскладке.
    fn init(&mut self, lib: &Lib, dir: &Path, legacy_main: i32, outputs: u32) -> bool {
        self.sp = None;
        self.dir = PathBuf::new();
        // см. docs/hardware.md, «Библиотеки Яндекса (C API)»
        if let Some(f) = MODEL_FILES.iter().find(|f| !dir.join(f).is_file()) {
            error!("{} spotter: no {f} in {}", self.what, dir.display());
            return false;
        }
        let conf = match SpotterConf::read(lib, dir) {
            Ok(conf) => conf,
            Err(e) => {
                if let Some(e) = e {
                    error!("spotter_conf_read: {e}");
                }
                return false;
            }
        };
        let mut counts = [0i64; SPOTTER_TYPES];
        for (t, n) in counts.iter_mut().enumerate() {
            *n = conf.channel_count(t as i64).unwrap_or_else(|e| {
                error!("spotter_conf_get_required_channel_count: {e}");
                0
            });
        }
        self.set_layouts(&counts, legacy_main);
        if !self.resolve(|ty| outputs & (1 << ty) != 0) {
            return false;
        }
        let (rate, err) = conf.native_rate();
        if let Some(e) = err {
            error!("spotter_conf_native_sample_rate: {e}");
        }
        // Типы каналов обеих раскладок одинаковы: они зависят только от counts.
        let (sp, err) = Spotter::create(&conf, SPOTTER_RATE, &self.layout.types);
        let ok = err.is_none() && sp.is_some();
        if let Some(e) = err {
            error!("spotter_create_multichannel: {e}");
        }
        // Kept even when created with an error, as the C++ daemon did.
        self.sp = sp;
        drop(conf);
        if ok {
            info!(
                "{} spotter {}: native rate {rate}, channels [{}]",
                self.what,
                dir.display(),
                self.layout
            );
            self.dir = dir.to_path_buf();
        }
        ok
    }

    /// Смена модели; если новая не создалась, заново создаётся прежняя. Второй спотер в RAM на
    /// время смены не держим: на станциях с ~238 МиБ это не замерено, поэтому прежняя модель
    /// пересоздаётся только на пути ошибки.
    fn switch(&mut self, lib: &Lib, dir: &Path, legacy_main: i32, outputs: u32) -> bool {
        let prev = std::mem::take(&mut self.dir);
        if self.init(lib, dir, legacy_main, outputs) {
            return true;
        }
        if !prev.as_os_str().is_empty() && !self.init(lib, &prev, legacy_main, outputs) {
            error!(
                "{} spotter: previous model {} did not reload",
                self.what,
                prev.display()
            );
        }
        false
    }

    fn restart(&mut self, lib: &Lib, legacy_main: i32, outputs: u32) {
        self.last_hit_ms = 0;
        let dir = std::mem::take(&mut self.dir);
        if dir.as_os_str().is_empty() {
            self.sp = None;
        } else if !self.init(lib, &dir, legacy_main, outputs) {
            // Оставить путь для следующего восстановления, но не кормить частично созданную модель.
            self.sp = None;
            self.dir = dir;
        }
    }

    /// Раскладка по таблице и запасная, `legacy_main` — выход VQE для типа 0 в запасной.
    fn set_layouts(&mut self, counts: &[i64; SPOTTER_TYPES], legacy_main: i32) {
        self.layout = spotter_layout(counts, &SPOTTER_TO_VQE);
        let mut legacy = LEGACY_SPOTTER_TO_VQE;
        legacy[0] = legacy_main;
        self.legacy = spotter_layout(counts, &legacy);
    }

    /// Выбирает раскладку до создания спотера: без нужных выходов он не получит данные.
    fn resolve(&mut self, present: impl Fn(i32) -> bool) -> bool {
        if self.layout.vqe.is_empty() {
            error!(
                "{} spotter: model requires no supported input channels",
                self.what
            );
            return false;
        }
        let Some(&ty) = self.layout.vqe.iter().find(|&&ty| !present(ty)) else {
            return true;
        };
        if self.legacy.vqe.iter().all(|&ty| present(ty)) {
            warn!(
                "{} spotter: VQE has no output {}({ty}); legacy channels [{}]",
                self.what,
                vqe_name(ty),
                self.legacy
            );
            self.layout = std::mem::take(&mut self.legacy);
            true
        } else {
            error!(
                "{} spotter: VQE has no output {}({ty}), nor the legacy channels; spotter off",
                self.what,
                vqe_name(ty)
            );
            false
        }
    }
}

/// The daemon's VQE + wake word spotter + optional command spotter.
pub(crate) struct Engine {
    // Fields drop in declaration order: the spotters first, then the VQE (instance, then its
    // config), as the C++ engine's destructor freed them.
    wake: Slot,
    cmd: Slot,
    vqe: Vqe,
    lib: Lib,
    outputs: u32,
    /// Capture frames per VQE chunk.
    pub chunk: usize,
    /// VQE output samples per chunk.
    frame: usize,
}

impl Engine {
    /// dlopen, VQE по пресету модели, спотер wake word (обязателен; модель — первая загрузившаяся
    /// из `wake_dirs`) и командный спотер (необязателен: без него предупреждение). Ошибки пишутся в
    /// лог; None — фронтенда нет.
    pub fn start(
        model: &pulsar_core::model::Model,
        wake_dirs: &[PathBuf],
        command_dir: Option<&Path>,
    ) -> Option<Engine> {
        if let Err(e) = preload_deps() {
            warn!("libtflite: {e}");
        }
        let lib = match Lib::load() {
            Ok(lib) => lib,
            Err(LoadError::Open(e)) => {
                error!("dlopen libyandex_evil: {e}");
                return None;
            }
            Err(LoadError::Missing(name)) => {
                error!("libyandex_evil: missing {name}");
                return None;
            }
        };
        let preset = model.vqe_preset;
        let (nmic, nspk) = (model.mic_channels.len(), model.spk_channels.len());
        let mut vqe = match Vqe::new(&lib, preset, Path::new(VQE_MODELS), nmic, nspk) {
            Ok(vqe) => vqe,
            Err((step, e)) => {
                let what = match step {
                    VqeStep::Config => "VQE config",
                    VqeStep::Create => "VQE create",
                };
                error!("{what}: {e}");
                return None;
            }
        };
        let (chunk, frame) = (vqe.chunk(), vqe.frame());
        // A zero chunk would spin the capture and processing threads.
        let (Ok(chunk_n @ 1..), Ok(frame_n @ 1..)) =
            (usize::try_from(chunk), usize::try_from(frame))
        else {
            error!("VQE config: {chunk} input frames, {frame} output samples per chunk");
            return None;
        };
        info!("VQE '{preset}': {chunk} input frames -> {frame} output samples per chunk");
        let outputs = vqe
            .output_mask()
            .inspect_err(|e| error!("VQE output channels: {e}"))
            .ok()?;
        // Якорь читается после событий 2/5, и без результата хита библиотека делает abort: при
        // другой нумерации событий в сборке лучше остаться без пре-ролла, чем падать.
        // см. docs/hardware.md, «Библиотеки Яндекса (C API)»
        let (c1, c2) = (
            lib.event_name(EV_CONFIRM_1ST),
            lib.event_name(EV_CONFIRM_2ND),
        );
        let events_ok = c1
            .as_deref()
            .is_some_and(|n| n.ends_with("CONFIRMATION_1ST_STAGE"))
            && c2
                .as_deref()
                .is_some_and(|n| n.ends_with("CONFIRMATION_2ND_STAGE"));
        let anchor = lib.has_unhandled_bytes() && events_ok;
        if !lib.has_unhandled_bytes() {
            warn!(
                "libyandex_evil: no spotter_get_unhandled_nbytes; no pre-roll after the wake word"
            );
        } else if !events_ok {
            warn!(
                "spotter event table differs: {EV_CONFIRM_1ST}={c1:?} {EV_CONFIRM_2ND}={c2:?}; \
                 no pre-roll after the wake word"
            );
        }
        let mut eng = Engine {
            wake: Slot::new("wake word", anchor),
            cmd: Slot::new("command", false),
            vqe,
            lib,
            outputs,
            chunk: chunk_n,
            frame: frame_n,
        };
        let mut wake_ok = false;
        for (i, dir) in wake_dirs.iter().enumerate() {
            wake_ok = eng.wake.init(&lib, dir, VQE_OMNI_WITH_GAIN, outputs);
            if wake_ok {
                break;
            }
            if i + 1 < wake_dirs.len() {
                warn!(
                    "wake word model {} did not load; trying the next",
                    dir.display()
                );
            }
        }
        if !wake_ok {
            return None;
        }
        if let Some(dir) = command_dir
            && !eng.cmd.init(&lib, dir, VQE_OMNI, outputs)
        {
            warn!("command spotter unavailable; voice commands without the wake word are disabled");
        }
        Some(eng)
    }

    /// `false` — запрошенная модель не загрузилась; фактический результат в `wake_dir()`.
    pub fn set_wake_model(&mut self, dir: &Path) -> bool {
        self.wake
            .switch(&self.lib, dir, VQE_OMNI_WITH_GAIN, self.outputs)
    }

    pub(crate) fn restart_spotters(&mut self) {
        self.wake
            .restart(&self.lib, VQE_OMNI_WITH_GAIN, self.outputs);
        self.cmd.restart(&self.lib, VQE_OMNI, self.outputs);
    }

    /// Каталог активной модели wake word.
    pub fn wake_dir(&self) -> &Path {
        if self.wake.sp.is_some() {
            &self.wake.dir
        } else {
            Path::new("")
        }
    }

    /// VQE output samples per chunk.
    pub fn frame(&self) -> usize {
        self.frame
    }

    /// One VQE chunk; false after an error (logged).
    pub fn process(&mut self, mics: &[f32], spk: &[f32]) -> bool {
        self.vqe
            .process(mics, spk)
            .inspect_err(|e| error!("VQE process: {e}"))
            .is_ok()
    }

    /// VQE output channel `ty` of the last chunk; None after an error (logged).
    pub fn output(&self, ty: i32) -> Option<&[f32]> {
        self.vqe
            .output(ty)
            .inspect_err(|e| error!("VQE output: {e}"))
            .ok()
    }

    /// Подаёт последний чанк спотеру wake word: хиты с якорем.
    pub fn feed_wake(&mut self) -> Vec<Hit> {
        feed(&self.vqe, self.frame, &mut self.wake)
    }

    /// Feeds the last chunk to the command spotter; its confirmed phrases.
    pub fn feed_command(&mut self) -> Vec<String> {
        feed(&self.vqe, self.frame, &mut self.cmd)
            .into_iter()
            .map(|h| h.phrase)
            .collect()
    }
}

fn feed(vqe: &Vqe, frame: usize, s: &mut Slot) -> Vec<Hit> {
    let nch = s.layout.vqe.len();
    let Some(sp) = s.sp.as_mut() else {
        return Vec::new();
    };
    let mut src: [&[f32]; MAX_SPOTTER_CHANNELS] = [&[]; MAX_SPOTTER_CHANNELS];
    let mut n = frame;
    for (k, &ty) in s.layout.vqe.iter().enumerate() {
        match vqe.output(ty) {
            Ok(d) => {
                n = n.min(d.len());
                src[k] = d;
            }
            Err(e) => {
                error!("VQE output: {e}");
                return Vec::new();
            }
        }
    }
    s.buf.resize(n * nch, 0);
    for (i, out) in s.buf.chunks_exact_mut(nch).enumerate() {
        for (o, d) in out.iter_mut().zip(&src[..nch]) {
            *o = to_s16(d[i]);
        }
    }
    if let Err(e) = sp.submit(&s.buf) {
        error!("spotter_submit_raw_data: {e}");
        return Vec::new();
    }
    drain_events(
        &mut Logged(sp),
        s.what,
        s.anchor.then_some(nch),
        &mut s.last_hit_ms,
        pulsar_core::now_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_from_channel_counts() {
        // Миди alisa: bnr-channel и freq-filter-audio-channel raw-mic (типы 3 и 4).
        let l = spotter_layout(&[1, 0, 0, 1, 1, 0], &SPOTTER_TO_VQE);
        assert_eq!(l.types, [0, 3, 4]);
        assert_eq!(l.vqe, [6, 8, 2]);
        assert_eq!(l.to_string(), "0<-omni(6),3<-bnr(8),4<-main_mic_sync(2)");
        // Запасная раскладка — та, с которой wake word работал раньше.
        let l = spotter_layout(&[1, 0, 0, 1, 1, 0], &LEGACY_SPOTTER_TO_VQE);
        assert_eq!(l.vqe, [9, 2, 6]);
        // Lite 2 alisa / yandex: types 0 and 4.
        let l = spotter_layout(&[1, 0, 0, 0, 1, 0], &SPOTTER_TO_VQE);
        assert_eq!(
            (l.types.as_slice(), l.vqe.as_slice()),
            (&[0, 4][..], &[6, 2][..])
        );
        // Командная модель: только тип 0.
        let l = spotter_layout(&[1, 0, 0, 0, 0, 0], &SPOTTER_TO_VQE);
        assert_eq!(l.to_string(), "0<-omni(6)");
        // Several channels of a type, every type mapped.
        let l = spotter_layout(&[2, 1, 1, 1, 1, 2], &SPOTTER_TO_VQE);
        assert_eq!(l.types, [0, 0, 1, 2, 3, 4, 5, 5]);
        assert_eq!(l.vqe, [6, 6, 7, 5, 8, 2, 3, 3]);
    }

    #[test]
    fn layout_skips_bad_counts() {
        // 8 or more channels of a type, or a negative count (error value): none of that type.
        let l = spotter_layout(&[8, -1, 0, 7, 100, 1], &SPOTTER_TO_VQE);
        assert_eq!(l.types, [3, 3, 3, 3, 3, 3, 3, 5]);
        assert_eq!(l.vqe, [8, 8, 8, 8, 8, 8, 8, 3]);
        assert!(spotter_layout(&[0; 6], &SPOTTER_TO_VQE).types.is_empty());
        // The largest possible layout fits the per-chunk channel table.
        assert_eq!(
            spotter_layout(&[7; 6], &SPOTTER_TO_VQE).types.len(),
            MAX_SPOTTER_CHANNELS
        );
    }

    #[test]
    fn missing_vqe_output_switches_to_legacy_channels() {
        let slot = |counts: &[i64; SPOTTER_TYPES], legacy_main| {
            let mut s = Slot::new("test", false);
            s.set_layouts(counts, legacy_main);
            s
        };
        assert!(!slot(&[0; SPOTTER_TYPES], VQE_OMNI_WITH_GAIN).resolve(|_| true));
        let wake = [1, 0, 0, 1, 1, 0];
        let mut s = slot(&wake, VQE_OMNI_WITH_GAIN);
        assert!(s.resolve(|_| true));
        assert_eq!(s.layout.vqe, [6, 8, 2]);
        // Запасная раскладка не нужна, когда есть все выходы таблицы.
        let mut s = slot(&wake, VQE_OMNI_WITH_GAIN);
        assert!(s.resolve(|ty| ty != VQE_OMNI_WITH_GAIN));
        assert_eq!(s.layout.vqe, [6, 8, 2]);
        let mut s = slot(&wake, VQE_OMNI_WITH_GAIN);
        assert!(s.resolve(|ty| ty != VQE_BNR));
        assert_eq!(s.layout.vqe, [9, 2, 6]);
        let mut s = slot(&wake, VQE_OMNI_WITH_GAIN);
        assert!(!s.resolve(|ty| ty != VQE_BNR && ty != VQE_OMNI_WITH_GAIN));
        // Командный спотер в запасной раскладке остаётся на VQE_OMNI.
        let mut s = slot(&[1, 0, 0, 1, 0, 0], VQE_OMNI);
        assert!(s.resolve(|ty| ty != VQE_BNR));
        assert_eq!(s.layout.vqe, [6, 2]);
    }

    #[test]
    fn float_to_s16_clamps_and_rounds_to_even() {
        assert_eq!(to_s16(0.0), 0);
        assert_eq!(to_s16(0.5), 16384);
        assert_eq!(to_s16(-0.5), -16384);
        assert_eq!(to_s16(1.0), 32767);
        assert_eq!(to_s16(-1.0), -32768);
        assert_eq!(to_s16(2.0), 32767);
        assert_eq!(to_s16(-2.0), -32768);
        // lrint: ties to even.
        assert_eq!(to_s16(1.5 / 32768.0), 2);
        assert_eq!(to_s16(2.5 / 32768.0), 2);
        assert_eq!(to_s16(-2.5 / 32768.0), -2);
        assert_eq!(to_s16(0.4 / 32768.0), 0);
        assert_eq!(to_s16(0.6 / 32768.0), 1);
        assert_eq!(to_s16(32766.5 / 32768.0), 32766);
        assert_eq!(to_s16(f32::NAN), 0);
    }

    struct Script {
        events: Vec<Result<i32, ()>>,
        polled: usize,
        phrases: usize,
        unhandled: Option<i32>,
        /// (polled, phrases) в момент каждого чтения якоря.
        unhandled_at: Vec<(usize, usize)>,
    }

    impl Script {
        fn new(events: &[Result<i32, ()>]) -> Self {
            Script {
                events: events.to_vec(),
                polled: 0,
                phrases: 0,
                unhandled: Some(3072),
                unhandled_at: Vec::new(),
            }
        }
    }

    impl EventSource for Script {
        fn next_event(&mut self) -> Option<i32> {
            let ev = self.events.get(self.polled).copied();
            self.polled += 1;
            // Past the script: "need more data".
            ev.map_or(Some(0), Result::ok)
        }

        fn phrase(&mut self) -> String {
            self.phrases += 1;
            format!("p{}", self.polled - 1)
        }

        fn unhandled(&mut self) -> Option<i32> {
            self.unhandled_at.push((self.polled, self.phrases));
            self.unhandled
        }
    }

    fn phrases(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.phrase.as_str()).collect()
    }

    #[test]
    fn events_stop_on_stop_events_errors_and_out_of_range() {
        for stop in [Ok(0), Ok(4), Ok(7), Ok(9), Ok(-1), Ok(10), Err(())] {
            let mut s = Script::new(&[Ok(1), Ok(3), stop, Ok(2)]);
            let mut last = 0;
            assert_eq!(drain_events(&mut s, "t", Some(3), &mut last, || 10_000), []);
            assert_eq!(s.polled, 3, "{stop:?}");
            assert_eq!((s.phrases, s.unhandled_at.len()), (0, 0));
        }
    }

    #[test]
    fn events_confirmations_are_hits() {
        // 1, 3, 6, 8 are skipped; 2 and 5 confirm (the phrase is read only for those).
        let mut s = Script::new(&[Ok(1), Ok(2), Ok(3), Ok(6), Ok(8), Ok(0), Ok(5)]);
        let mut last = 0;
        assert_eq!(
            drain_events(&mut s, "t", Some(3), &mut last, || 5000),
            [Hit {
                phrase: "p1".into(),
                unhandled: Some(3072),
                after: Some(512),
            }]
        );
        assert_eq!((s.polled, s.phrases, last), (6, 1, 5000));
    }

    #[test]
    fn events_at_most_16_per_chunk() {
        let mut s = Script::new(&[Ok(1); 40]);
        let mut last = 0;
        assert_eq!(drain_events(&mut s, "t", Some(3), &mut last, || 5000), []);
        assert_eq!(s.polled, 16);
    }

    #[test]
    fn events_debounced_for_1200_ms() {
        let mut last = 0;
        let mut now = 10_000;
        // Два подтверждения одного высказывания (1-я и 2-я стадия): один хит, якорь читается один
        // раз.
        let mut s = Script::new(&[Ok(2), Ok(5)]);
        let hits = drain_events(&mut s, "t", Some(3), &mut last, || now);
        assert_eq!(phrases(&hits), ["p0"]);
        assert_eq!((s.phrases, s.unhandled_at.len(), last), (2, 1, 10_000));
        // Через 1199 мс — то же высказывание, якорь не читается.
        now += 1199;
        let mut s = Script::new(&[Ok(2)]);
        assert_eq!(drain_events(&mut s, "t", Some(3), &mut last, || now), []);
        assert_eq!((s.unhandled_at.len(), last), (0, 10_000));
        // 1200 ms after the hit: a new one.
        now += 1;
        let mut s = Script::new(&[Ok(5)]);
        let hits = drain_events(&mut s, "t", Some(3), &mut last, || now);
        assert_eq!(phrases(&hits), ["p0"]);
        assert_eq!(last, 11_200);
    }

    #[test]
    fn anchor_is_not_read_without_channels() {
        // Спотер команд: якорь не читается вовсе.
        let mut s = Script::new(&[Ok(2)]);
        let mut last = 0;
        assert_eq!(
            drain_events(&mut s, "t", None, &mut last, || 5000),
            [Hit {
                phrase: "p0".into(),
                unhandled: None,
                after: None,
            }]
        );
        assert!(s.unhandled_at.is_empty());
    }

    #[test]
    fn anchor_is_read_right_after_the_phrase_before_the_next_event() {
        // Иначе библиотека делает abort: якорь — после phrase своего подтверждения и до следующего
        // next_event.
        let mut s = Script::new(&[Ok(1), Ok(2), Ok(3), Ok(0)]);
        let mut last = 0;
        drain_events(&mut s, "t", Some(3), &mut last, || 5000);
        assert_eq!(s.unhandled_at, [(2, 1)]);
    }

    #[test]
    fn hit_without_anchor_is_still_a_hit() {
        // Нет символа или значение не кратно кадру: wake word срабатывает, пре-ролл пустой.
        for unhandled in [None, Some(3070)] {
            let mut s = Script::new(&[Ok(2)]);
            s.unhandled = unhandled;
            let mut last = 0;
            assert_eq!(
                drain_events(&mut s, "t", Some(3), &mut last, || 5000),
                [Hit {
                    phrase: "p0".into(),
                    unhandled,
                    after: None,
                }]
            );
        }
    }

    #[test]
    fn anchor_is_whole_frames_of_all_channels() {
        assert_eq!(anchor_samples(Some(3072), 3), Some(512));
        assert_eq!(anchor_samples(Some(3070), 3), None);
        assert_eq!(anchor_samples(Some(-6), 3), None);
        assert_eq!(anchor_samples(Some(0), 3), Some(0));
        assert_eq!(anchor_samples(None, 3), None);
        assert_eq!(anchor_samples(Some(4), 0), None);
    }
}
