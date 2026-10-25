//! Yandex VQE (echo cancellation, beamforming, noise reduction) and phrase spotters from the
//! firmware's libyandex_evil.so. The safe wrappers ([`Lib`], [`Vqe`], [`SpotterConf`],
//! [`Spotter`]) and [`spotter_layout`] are public for ww_offline; `Engine` is the daemon's use
//! of them.

#[allow(unsafe_code)]
mod ffi;

use std::fmt::Write as _;
use std::path::Path;

use log::{error, info, warn};

pub use ffi::{Lib, LoadError, Spotter, SpotterConf, Vqe, VqeStep, preload_deps};

pub const VQE_MODELS: &str = "/system/vendor/quasar/vqe_models";

// voicetech.AudioInputChannelType (VQE output channel types).
pub const RAW_MAIN_MIC_SYNC: i32 = 2;
pub const VQE_OMNI: i32 = 6;
pub const VQE_OMNI_WITH_GAIN: i32 = 9;

/// Spotter channel type (0..5) -> VQE output feeding it. maind maps spotter types to
/// {UNKNOWN(main), RAW_MAIN_MIC_SYNC, VQE_OMNI_WITH_GAIN, RAW_AUX_MIC_SYNC, VQE_OMNI,
/// VQE_BEAMFORMING}; the cucumber preset has no AUX/BEAMFORMING outputs, so the closest available
/// ones are used. Type 0 (main) is chosen per model: see [`spotter_layout`].
pub const SPOTTER_TO_VQE: [i32; 6] = [
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

// Spotter events (event_id_to_str). Hits are confirmations; 0/4/7/9 mean "need more data /
// finished".
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

/// Layout from the per-type channel counts of a spotter config
/// (spotter_conf_get_required_channel_count); `main_vqe` feeds type 0. Measured offline on the
/// device: the wake-word model works with VQE_OMNI_WITH_GAIN, the command model only with
/// VQE_OMNI (10/14 synthetic commands vs 0/14 with the AGC'd channel).
pub fn spotter_layout(counts: &[i64; SPOTTER_TYPES], main_vqe: i32) -> SpotterLayout {
    let mut l = SpotterLayout::default();
    for (t, &n) in counts.iter().enumerate() {
        if !(0..TYPE_CHANNELS_LIMIT).contains(&n) {
            continue;
        }
        for _ in 0..n {
            l.types.push(t as i32);
            l.vqe
                .push(if t == 0 { main_vqe } else { SPOTTER_TO_VQE[t] });
        }
    }
    l
}

impl std::fmt::Display for SpotterLayout {
    /// "0<-vqe9,3<-vqe2,4<-vqe6"
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, (t, v)) in self.types.iter().zip(&self.vqe).enumerate() {
            if i > 0 {
                f.write_char(',')?;
            }
            write!(f, "{t}<-vqe{v}")?;
        }
        Ok(())
    }
}

/// VQE sample in [-1, 1] -> s16 as the C++ daemon converts it (clamped, lrint: ties to even).
pub(crate) fn to_s16(v: f32) -> i16 {
    let v = v * 32768.0;
    if v > 32767.0 {
        32767
    } else if v < -32768.0 {
        -32768
    } else {
        v.round_ties_even() as i16
    }
}

/// Where the events of one fed chunk come from (a spotter, or a script in tests).
pub(crate) trait EventSource {
    /// Next event; None after an error (logged).
    fn next_event(&mut self) -> Option<i32>;
    /// Phrase of the current event; "" when the library has none.
    fn phrase(&mut self) -> String;
}

/// Reads the events of one fed chunk (at most 16, until a stop event) and returns the confirmed
/// phrases, one per utterance (`DEBOUNCE_MS` after the previous hit).
pub(crate) fn drain_events(
    src: &mut impl EventSource,
    what: &str,
    last_hit_ms: &mut i64,
    mut now_ms: impl FnMut() -> i64,
) -> Vec<String> {
    let mut hits = Vec::new();
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
        info!("{what} detected: '{phrase}' (event {ev})");
        hits.push(phrase);
    }
    hits
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
}

/// One spotter of the daemon with its channel layout.
pub(crate) struct Slot {
    what: &'static str,
    sp: Option<Spotter>,
    /// Per spotter input channel: VQE output type.
    vqe: Vec<i32>,
    buf: Vec<i16>,
    last_hit_ms: i64,
}

impl Slot {
    fn new(what: &'static str) -> Self {
        Slot {
            what,
            sp: None,
            vqe: Vec::new(),
            buf: Vec::new(),
            last_hit_ms: 0,
        }
    }

    /// (Re)creates the spotter from the model in `dir`; the previous one is destroyed first.
    fn init(&mut self, lib: &Lib, dir: &Path, main_vqe: i32) -> bool {
        self.sp = None;
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
        let layout = spotter_layout(&counts, main_vqe);
        self.vqe.clone_from(&layout.vqe);
        let (rate, err) = conf.native_rate();
        if let Some(e) = err {
            error!("spotter_conf_native_sample_rate: {e}");
        }
        let (sp, err) = Spotter::create(&conf, SPOTTER_RATE, &layout.types);
        let ok = err.is_none() && sp.is_some();
        if let Some(e) = err {
            error!("spotter_create_multichannel: {e}");
        }
        // Kept even when created with an error, as the C++ daemon did.
        self.sp = sp;
        drop(conf);
        if ok {
            info!(
                "{} spotter {}: native rate {rate}, channels [{layout}]",
                self.what,
                dir.display()
            );
        }
        ok
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
    /// Capture frames per VQE chunk.
    pub chunk: usize,
    /// VQE output samples per chunk.
    frame: usize,
}

impl Engine {
    /// dlopen, VQE for the model's preset, the wake word spotter (required) and the command spotter
    /// (optional: a warning without it). Errors are logged; None = no front-end.
    pub fn start(
        model: &pulsar_core::model::Model,
        wake_dir: &Path,
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
        let vqe = match Vqe::new(&lib, preset, Path::new(VQE_MODELS), nmic, nspk) {
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
        let mut eng = Engine {
            wake: Slot::new("wake word"),
            cmd: Slot::new("command"),
            vqe,
            lib,
            chunk: chunk_n,
            frame: frame_n,
        };
        if !eng.wake.init(&lib, wake_dir, VQE_OMNI_WITH_GAIN) {
            return None;
        }
        if let Some(dir) = command_dir
            && !eng.cmd.init(&lib, dir, VQE_OMNI)
        {
            warn!("command spotter unavailable; voice commands without the wake word are disabled");
        }
        Some(eng)
    }

    /// Switches the wake word model.
    pub fn set_wake_model(&mut self, dir: &Path) -> bool {
        self.wake.init(&self.lib, dir, VQE_OMNI_WITH_GAIN)
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

    /// Feeds the last chunk to the wake word spotter; its confirmed phrases.
    pub fn feed_wake(&mut self) -> Vec<String> {
        feed(&self.vqe, self.frame, &mut self.wake)
    }

    /// Feeds the last chunk to the command spotter; its confirmed phrases.
    pub fn feed_command(&mut self) -> Vec<String> {
        feed(&self.vqe, self.frame, &mut self.cmd)
    }
}

fn feed(vqe: &Vqe, frame: usize, s: &mut Slot) -> Vec<String> {
    let nch = s.vqe.len();
    let Some(sp) = s.sp.as_mut() else {
        return Vec::new();
    };
    if nch == 0 {
        return Vec::new();
    }
    let mut src: [&[f32]; MAX_SPOTTER_CHANNELS] = [&[]; MAX_SPOTTER_CHANNELS];
    let mut n = frame;
    for (k, &ty) in s.vqe.iter().enumerate() {
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
        &mut s.last_hit_ms,
        pulsar_core::now_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_from_channel_counts() {
        // Midi alisa: types 0, 3, 4 (one channel each).
        let l = spotter_layout(&[1, 0, 0, 1, 1, 0], VQE_OMNI_WITH_GAIN);
        assert_eq!(l.types, [0, 3, 4]);
        assert_eq!(l.vqe, [9, 2, 6]);
        assert_eq!(l.to_string(), "0<-vqe9,3<-vqe2,4<-vqe6");
        // Lite 2 alisa / yandex: types 0 and 4.
        let l = spotter_layout(&[1, 0, 0, 0, 1, 0], VQE_OMNI_WITH_GAIN);
        assert_eq!(
            (l.types.as_slice(), l.vqe.as_slice()),
            (&[0, 4][..], &[9, 6][..])
        );
        // Command model: type 0 only, fed from VQE_OMNI.
        let l = spotter_layout(&[1, 0, 0, 0, 0, 0], VQE_OMNI);
        assert_eq!(l.to_string(), "0<-vqe6");
        // Several channels of a type, every type mapped.
        let l = spotter_layout(&[2, 1, 1, 1, 1, 2], VQE_OMNI_WITH_GAIN);
        assert_eq!(l.types, [0, 0, 1, 2, 3, 4, 5, 5]);
        assert_eq!(l.vqe, [9, 9, 2, 9, 2, 6, 6, 6]);
    }

    #[test]
    fn layout_skips_bad_counts() {
        // 8 or more channels of a type, or a negative count (error value): none of that type.
        let l = spotter_layout(&[8, -1, 0, 7, 100, 1], VQE_OMNI);
        assert_eq!(l.types, [3, 3, 3, 3, 3, 3, 3, 5]);
        assert_eq!(l.vqe, [2, 2, 2, 2, 2, 2, 2, 6]);
        assert!(spotter_layout(&[0; 6], VQE_OMNI).types.is_empty());
        // The largest possible layout fits the per-chunk channel table.
        assert_eq!(
            spotter_layout(&[7; 6], VQE_OMNI).types.len(),
            MAX_SPOTTER_CHANNELS
        );
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
    }

    impl Script {
        fn new(events: &[Result<i32, ()>]) -> Self {
            Script {
                events: events.to_vec(),
                polled: 0,
                phrases: 0,
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
    }

    #[test]
    fn events_stop_on_stop_events_errors_and_out_of_range() {
        for stop in [Ok(0), Ok(4), Ok(7), Ok(9), Ok(-1), Ok(10), Err(())] {
            let mut s = Script::new(&[Ok(1), Ok(3), stop, Ok(2)]);
            let mut last = 0;
            assert!(drain_events(&mut s, "t", &mut last, || 10_000).is_empty());
            assert_eq!(s.polled, 3, "{stop:?}");
            assert_eq!(s.phrases, 0);
        }
    }

    #[test]
    fn events_confirmations_are_hits() {
        // 1, 3, 6, 8 are skipped; 2 and 5 confirm (the phrase is read only for those).
        let mut s = Script::new(&[Ok(1), Ok(2), Ok(3), Ok(6), Ok(8), Ok(0), Ok(5)]);
        let mut last = 0;
        assert_eq!(drain_events(&mut s, "t", &mut last, || 5000), ["p1"]);
        assert_eq!((s.polled, s.phrases, last), (6, 1, 5000));
    }

    #[test]
    fn events_at_most_16_per_chunk() {
        let mut s = Script::new(&[Ok(1); 40]);
        let mut last = 0;
        assert!(drain_events(&mut s, "t", &mut last, || 5000).is_empty());
        assert_eq!(s.polled, 16);
    }

    #[test]
    fn events_debounced_for_1200_ms() {
        let mut last = 0;
        let mut now = 10_000;
        // Two confirmations of one utterance (1st and 2nd stage): one hit.
        let mut s = Script::new(&[Ok(2), Ok(5)]);
        assert_eq!(drain_events(&mut s, "t", &mut last, || now), ["p0"]);
        assert_eq!((s.phrases, last), (2, 10_000));
        // 1199 ms later: still the same utterance.
        now += 1199;
        let mut s = Script::new(&[Ok(2)]);
        assert!(drain_events(&mut s, "t", &mut last, || now).is_empty());
        assert_eq!(last, 10_000);
        // 1200 ms after the hit: a new one.
        now += 1;
        let mut s = Script::new(&[Ok(5)]);
        assert_eq!(drain_events(&mut s, "t", &mut last, || now), ["p0"]);
        assert_eq!(last, 11_200);
    }
}
