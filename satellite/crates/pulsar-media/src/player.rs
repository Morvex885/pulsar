//! Audio playback through the GStreamer build bundled with the Yandex firmware into the model's
//! ALSA playback PCM (`Model::playback_pcm`: dmix + softvol). Two independent channels:
//!   Music    - media_player content, can be paused / ducked.
//!   Announce - TTS, announcements, earcons; plays over (ducked) music.
//!
//! Threading: a dedicated thread runs a GMainLoop on the default GMainContext; the bus watches and
//! timeouts live there, and so does the state machine ([`Machine`], in a thread-local). Handle
//! methods marshal work into it with g_idle_add; events go back through the channel `spawn`
//! returns. One player per process (it owns the default main context).

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ffi::{CString, c_void};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use log::{debug, error, info, warn};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::gst::{self, Api, Call, MainLoop, Pipeline};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Music,
    Announce,
}

impl Channel {
    fn index(self) -> usize {
        self as usize
    }

    fn from_index(i: usize) -> Option<Channel> {
        match i {
            0 => Some(Channel::Music),
            1 => Some(Channel::Announce),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayState {
    Idle,
    Loading,
    Playing,
    Paused,
    Error,
}

impl PlayState {
    fn name(self) -> &'static str {
        match self {
            PlayState::Idle => "IDLE",
            PlayState::Loading => "LOADING",
            PlayState::Playing => "PLAYING",
            PlayState::Paused => "PAUSED",
            PlayState::Error => "ERROR",
        }
    }

    fn from_u8(v: u8) -> PlayState {
        match v {
            1 => PlayState::Loading,
            2 => PlayState::Playing,
            3 => PlayState::Paused,
            4 => PlayState::Error,
            _ => PlayState::Idle,
        }
    }
}

/// Identifies one `play()` call; its [`PlayerEvent::Done`] carries it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct PlayId(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlayerEvent {
    State(Channel, PlayState),
    /// Exactly once per `play()`: ok = every url played to its end. A list cut short by `stop()`
    /// or by a later `play()` on the same channel gets ok = false, sent by that call itself (so
    /// it precedes anything the new playback reports). Intended difference from C++: a list that
    /// already ended on the GLib thread before that `stop()` / `play()` gets its actual ok (C++
    /// re-checked on the Loop and reported false), so ok = true can arrive for a list the
    /// caller has meanwhile replaced; callers match the id against what is current.
    Done {
        id: PlayId,
        ok: bool,
    },
}

// GstPlayFlags: AUDIO | SOFT_VOLUME (no video/text/visualisation). BUFFERING (1 << 8) is left out
// on purpose: in this playbin (GStreamer 1.14.5) it only sets uridecodebin "use-buffering", which
// reaches decodebin for non-stream uris (local files) alone. http(s) uris always get a queue2 with
// use-buffering=TRUE and adaptive (HLS) ones decodebin buffering regardless, so MSG_BUFFERING
// already arrives for every network source. Stock maind sets use-buffering because its own
// souphttpsrc ! decodebin pipelines have no queue2.
const PLAY_FLAGS: u32 = (1 << 1) | (1 << 4);
// ANNOUNCE network queue (playbin "buffer-duration" -> queue2 max-size-time, default 2 s). queue2
// starts out buffering and reports 100 % only once it holds that much input (estimated from the
// input rate), the whole response, or 2 MB. HA's streamed TTS arrives at about the speaking rate,
// so the default held each answer back by about 2 s; local files (earcons) have no queue2.
const ANNOUNCE_BUFFER_NS: i64 = 500_000_000;
// MUSIC retries of an http(s) uri after a network failure (1, 2, 4 s apart), and the playtime
// after which a failure counts as a new outage rather than as another retry of the same one.
const MAX_RETRIES: u32 = 3;
const RETRY_RESET_MS: i64 = 60_000;
// GstResourceError codes souphttpsrc (1.14) posts for connection/TLS failures, unexpected HTTP
// statuses and transfers broken after its own reconnect attempts (OPEN_READ, READ).
const RESOURCE_ERROR_OPEN_READ: i32 = 5;
const RESOURCE_ERROR_READ: i32 = 9;
// Linear gain ramp (0.05 per 20 ms, full scale in 400 ms) to avoid clicks when ducking.
const RAMP_STEP: f64 = 0.05;
const RAMP_MS: u32 = 20;

/// GstState values the player sets or compares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GstState {
    Null = 1,
    Paused = 3,
    Playing = 4,
}

/// A bus message, as far as the player reads it.
#[derive(Debug)]
pub(crate) enum Msg {
    Eos,
    Error(ErrorInfo),
    Warning(String),
    Buffering(i32),
    /// Of the pipeline itself; `new` is the GstState reached.
    StateChanged {
        new: i32,
    },
    ClockLost,
    Other,
}

/// A GError from an error / warning message.
#[derive(Debug)]
pub(crate) struct ErrorInfo {
    pub(crate) message: String,
    pub(crate) debug: Option<String>,
    /// (domain quark, code); None when the message carried no GError.
    pub(crate) domain_code: Option<(u32, i32)>,
}

/// GLib timeouts of the state machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Timer {
    Retry(Channel),
    Ramp(Channel),
}

impl Timer {
    fn tag(self) -> usize {
        match self {
            Timer::Retry(ch) => ch.index() * 2,
            Timer::Ramp(ch) => ch.index() * 2 + 1,
        }
    }

    fn from_tag(tag: usize) -> Option<Timer> {
        let ch = Channel::from_index(tag / 2)?;
        Some(if tag.is_multiple_of(2) {
            Timer::Retry(ch)
        } else {
            Timer::Ramp(ch)
        })
    }
}

/// http(s)://, file:// or other uris pass unchanged; an absolute path becomes file://.
pub(crate) fn to_uri(u: &str) -> Cow<'_, str> {
    if u.contains("://") {
        Cow::Borrowed(u)
    } else if u.starts_with('/') {
        Cow::Owned(format!("file://{u}"))
    } else {
        Cow::Borrowed(u)
    }
}

/// souphttpsrc (1.14) writes the libsoup status into the error's debug text as
/// "<reason> (<status>), URL: <uri>, Redirect to: ..."; -1 when there is none.
pub(crate) fn soup_status(dbg: Option<&str>) -> i32 {
    let Some(dbg) = dbg else { return -1 };
    let Some(p) = dbg.find("), URL: ") else {
        return -1;
    };
    let b = dbg.as_bytes();
    let mut q = p;
    while q > 0 && b[q - 1].is_ascii_digit() {
        q -= 1;
    }
    if q == p || q == 0 || b[q - 1] != b'(' {
        return -1;
    }
    // atoi of the digits; saturating where C's atoi would overflow.
    b[q..p].iter().fold(0i32, |n, d| {
        n.saturating_mul(10).saturating_add(i32::from(d - b'0'))
    })
}

/// MUSIC http(s) streams only: the network may come back, a local file or a TTS answer will not
/// (and a late answer is worse than none). `url` as given to play().
pub(crate) fn retryable(
    ch: Channel,
    url: &str,
    err: &ErrorInfo,
    resource_quark: impl FnOnce() -> u32,
) -> bool {
    let Some((domain, code)) = err.domain_code else {
        return false;
    };
    if ch != Channel::Music || !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    // Transport failures (libsoup status < 100: DNS, connect, TLS, broken transfer), 5xx, 408 and
    // 429 may pass; other HTTP statuses (404, 403, ...) will not.
    let status = soup_status(err.debug.as_deref());
    if status >= 0 {
        return !(100..500).contains(&status) || status == 408 || status == 429;
    }
    let resource = resource_quark();
    resource != 0
        && domain == resource
        && (code == RESOURCE_ERROR_OPEN_READ || code == RESOURCE_ERROR_READ)
}

/// The next retry of a url that failed `retries` times so far and last (re)started at
/// `started_ms`: (retries including this one, delay in ms), None when they are used up.
pub(crate) fn next_retry(retries: u32, started_ms: i64, now_ms: i64) -> Option<(u32, u32)> {
    let retries = if now_ms - started_ms >= RETRY_RESET_MS {
        0
    } else {
        retries
    };
    (retries < MAX_RETRIES).then(|| (retries + 1, 1000 << retries))
}

/// Channel gain 0..1 (NaN = 0), as C++ `std::min(1.0, std::max(0.0, double(gain)))`.
fn clamp_gain(gain: f32) -> f64 {
    let g = f64::from(gain);
    let g = if 0.0 < g { g } else { 0.0 };
    if g < 1.0 { g } else { 1.0 }
}

/// What the state machine does to GStreamer (and GLib timeouts); faked by the tests.
pub(crate) trait Backend: Send {
    /// false on GST_STATE_CHANGE_FAILURE.
    fn set_state(&mut self, ch: Channel, s: GstState) -> bool;
    fn set_uri(&mut self, ch: Channel, uri: &str);
    fn set_volume(&mut self, ch: Channel, v: f64);
    /// ns; None without gst_element_query_position or when the query fails.
    fn query_position(&mut self, ch: Channel) -> Option<i64>;
    /// Whether gst_element_seek_simple exists.
    fn can_seek(&self) -> bool;
    fn seek(&mut self, ch: Channel, pos_ns: i64) -> bool;
    fn resource_error_quark(&self) -> u32;
    fn timeout_add(&mut self, ms: u32, t: Timer) -> u32;
    fn source_remove(&mut self, id: u32);
    fn now_ms(&self) -> i64;
    /// The pipeline's address (message sources are compared with it).
    fn element(&self, ch: Channel) -> *const c_void;
}

/// State shared by the handle (runtime thread) and the GLib thread.
pub(crate) struct Shared {
    /// PlayState per channel; written by the GLib thread only, before the State event.
    states: [AtomicU8; 2],
    /// Per channel: the play() whose Done is still owed.
    pending: Mutex<[Option<PlayId>; 2]>,
    events: UnboundedSender<PlayerEvent>,
}

impl Shared {
    fn new(events: UnboundedSender<PlayerEvent>) -> Shared {
        Shared {
            states: [AtomicU8::new(0), AtomicU8::new(0)],
            pending: Mutex::new([None, None]),
            events,
        }
    }

    fn state(&self, ch: Channel) -> PlayState {
        PlayState::from_u8(self.states[ch.index()].load(Ordering::Relaxed))
    }

    fn send(&self, ev: PlayerEvent) {
        // The receiver may be gone at shutdown.
        let _ = self.events.send(ev);
    }

    /// Caller side: `new` (a play()) or nothing (stop()) replaces the list whose Done is owed,
    /// which gets Done(false) now.
    fn supersede(&self, ch: Channel, new: Option<PlayId>) {
        let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = std::mem::replace(&mut p[ch.index()], new) {
            self.send(PlayerEvent::Done { id: old, ok: false });
        }
    }

    /// Delivers the result of list `id` unless a later play() / stop() superseded it meanwhile
    /// (that call already delivered false).
    fn complete(&self, ch: Channel, id: PlayId, ok: bool) {
        let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if p[ch.index()] == Some(id) {
            p[ch.index()] = None;
            self.send(PlayerEvent::Done { id, ok });
        }
    }
}

/// GLib-thread state of one channel.
struct Chan {
    urls: Vec<String>,
    index: usize,
    /// The play() that loaded `urls`.
    item: PlayId,
    /// A url list is loaded.
    active: bool,
    /// The user wants PLAYING (false = paused).
    want_play: bool,
    buffering: bool,
    reached_playing: bool,
    all_ok: bool,
    /// Network retries of the current url.
    retries: u32,
    /// Last (re)start of the current url.
    started_ms: i64,
    /// Position to seek to once a retried url has prerolled.
    resume_ns: i64,
    /// GLib timeout of a pending retry.
    retry_src: u32,
    gain_cur: f64,
    gain_target: f64,
    ramp_src: u32,
}

impl Chan {
    fn new() -> Chan {
        Chan {
            urls: Vec::new(),
            index: 0,
            item: PlayId(0),
            active: false,
            want_play: false,
            buffering: false,
            reached_playing: false,
            all_ok: true,
            retries: 0,
            started_ms: 0,
            resume_ns: 0,
            retry_src: 0,
            gain_cur: 1.0,
            gain_target: 1.0,
            ramp_src: 0,
        }
    }
}

/// The player's state machine; runs on the GLib thread.
pub(crate) struct Machine {
    backend: Box<dyn Backend>,
    shared: Arc<Shared>,
    chans: [Chan; 2],
}

impl Machine {
    fn new(backend: Box<dyn Backend>, shared: Arc<Shared>) -> Machine {
        Machine {
            backend,
            shared,
            chans: [Chan::new(), Chan::new()],
        }
    }

    fn set_state(&mut self, ch: Channel, s: PlayState) {
        let prev =
            PlayState::from_u8(self.shared.states[ch.index()].swap(s as u8, Ordering::Relaxed));
        if prev == s {
            return;
        }
        debug!("player: ch{} {} -> {}", ch.index(), prev.name(), s.name());
        self.shared.send(PlayerEvent::State(ch, s));
    }

    fn loading_or_paused(&self, ch: Channel) -> PlayState {
        if self.chans[ch.index()].want_play {
            PlayState::Loading
        } else {
            PlayState::Paused
        }
    }

    fn cancel_retry(&mut self, ch: Channel) {
        let src = std::mem::take(&mut self.chans[ch.index()].retry_src);
        if src != 0 {
            self.backend.source_remove(src);
        }
    }

    fn halt(&mut self, ch: Channel, failed: bool) {
        self.cancel_retry(ch);
        self.backend.set_state(ch, GstState::Null);
        let c = &mut self.chans[ch.index()];
        c.active = false;
        c.buffering = false;
        c.urls.clear();
        if failed {
            self.set_state(ch, PlayState::Error);
        }
        self.set_state(ch, PlayState::Idle);
    }

    fn finish(&mut self, ch: Channel, ok: bool) {
        self.halt(ch, !ok);
        let item = self.chans[ch.index()].item;
        self.shared.complete(ch, item, ok);
    }

    // Every (re)start passes through NULL: GstPipeline's auto-flush-bus (on by default) drops
    // all queued bus messages on READY -> NULL, so an EOS/ERROR still pending from the previous
    // uri cannot end this one.
    fn start_current(&mut self, ch: Channel) {
        let c = &self.chans[ch.index()];
        let uri = to_uri(&c.urls[c.index]).into_owned();
        info!(
            "player: ch{} play [{}/{}] {uri}",
            ch.index(),
            c.index + 1,
            c.urls.len()
        );
        self.backend.set_state(ch, GstState::Null);
        self.backend.set_uri(ch, &uri);
        let now = self.backend.now_ms();
        let c = &mut self.chans[ch.index()];
        c.buffering = false;
        c.reached_playing = false;
        c.started_ms = now;
        let want = c.want_play;
        self.set_state(ch, self.loading_or_paused(ch));
        let target = if want {
            GstState::Playing
        } else {
            GstState::Paused
        };
        if !self.backend.set_state(ch, target) {
            error!("player: ch{} set_state failed for {uri}", ch.index());
            self.advance(ch, false);
        }
    }

    /// Current url finished (ok) or failed; go to the next or finish the list.
    fn advance(&mut self, ch: Channel, ok: bool) {
        let c = &mut self.chans[ch.index()];
        c.all_ok = c.all_ok && ok;
        c.retries = 0;
        c.resume_ns = 0;
        if c.index + 1 < c.urls.len() {
            c.index += 1;
            self.start_current(ch);
        } else {
            let all_ok = c.all_ok;
            self.finish(ch, all_ok);
        }
    }

    /// Restarts the current uri after a backoff, resuming at the position reached (live streams
    /// refuse the seek and simply restart); false when the retries are used up.
    fn schedule_retry(&mut self, ch: Channel) -> bool {
        let now = self.backend.now_ms();
        let c = &mut self.chans[ch.index()];
        let Some((retries, delay)) = next_retry(c.retries, c.started_ms, now) else {
            return false;
        };
        c.retries = retries;
        warn!(
            "player: ch{} network failure, retry {retries}/{MAX_RETRIES} in {delay} ms",
            ch.index()
        );
        if let Some(pos) = self.backend.query_position(ch)
            && pos > 0
        {
            self.chans[ch.index()].resume_ns = pos;
        }
        self.backend.set_state(ch, GstState::Null);
        let c = &mut self.chans[ch.index()];
        c.buffering = false;
        c.reached_playing = false;
        self.set_state(ch, self.loading_or_paused(ch));
        self.chans[ch.index()].retry_src = self.backend.timeout_add(delay, Timer::Retry(ch));
        true
    }

    fn retry_tick(&mut self, ch: Channel) {
        let c = &mut self.chans[ch.index()];
        c.retry_src = 0;
        if c.active {
            self.start_current(ch);
        }
    }

    fn apply_target_state(&mut self, ch: Channel) {
        let c = &self.chans[ch.index()];
        if !c.active {
            return;
        }
        if c.retry_src != 0 {
            // Waiting to retry: the pipeline stays in NULL until retry_tick.
            self.set_state(ch, self.loading_or_paused(ch));
            return;
        }
        let (want, buffering, reached) = (c.want_play, c.buffering, c.reached_playing);
        let st = if want && !buffering {
            GstState::Playing
        } else {
            GstState::Paused
        };
        self.backend.set_state(ch, st);
        if !want {
            self.set_state(ch, PlayState::Paused);
        } else if buffering || !reached {
            self.set_state(ch, PlayState::Loading);
        }
    }

    fn handle_message(&mut self, ch: Channel, msg: Msg) {
        if !self.chans[ch.index()].active {
            return;
        }
        let i = ch.index();
        match msg {
            Msg::Eos => {
                info!("player: ch{i} EOS");
                self.advance(ch, true);
            }
            Msg::Error(err) => {
                error!(
                    "player: ch{i} error: {} ({})",
                    err.message,
                    err.debug.as_deref().unwrap_or("")
                );
                let c = &self.chans[i];
                let backend = &self.backend;
                let retry = retryable(ch, &c.urls[c.index], &err, || {
                    backend.resource_error_quark()
                });
                if !retry || !self.schedule_retry(ch) {
                    self.advance(ch, false);
                }
            }
            Msg::Warning(message) => warn!("player: ch{i} warning: {message}"),
            Msg::Buffering(pct) => {
                let buf = pct < 100;
                if buf != self.chans[i].buffering {
                    debug!("player: ch{i} buffering {pct}%");
                    self.chans[i].buffering = buf;
                    self.apply_target_state(ch);
                }
            }
            Msg::StateChanged { new } => {
                if self.chans[i].resume_ns > 0
                    && new >= GstState::Paused as i32
                    && self.backend.can_seek()
                {
                    let pos = std::mem::take(&mut self.chans[i].resume_ns);
                    let sought = self.backend.seek(ch, pos);
                    info!(
                        "player: ch{i} {} at {} ms",
                        if sought {
                            "resumed"
                        } else {
                            "cannot seek, restarted"
                        },
                        pos / 1_000_000
                    );
                }
                if new == GstState::Playing as i32 {
                    let c = &mut self.chans[i];
                    c.reached_playing = true;
                    if c.want_play && !c.buffering {
                        self.set_state(ch, PlayState::Playing);
                    }
                }
            }
            Msg::ClockLost => {
                self.backend.set_state(ch, GstState::Paused);
                self.apply_target_state(ch);
            }
            Msg::Other => {}
        }
    }

    fn ramp_tick(&mut self, ch: Channel) -> bool {
        let c = &mut self.chans[ch.index()];
        let d = c.gain_target - c.gain_cur;
        if d.abs() <= RAMP_STEP {
            c.gain_cur = c.gain_target;
        } else {
            c.gain_cur += if d > 0.0 { RAMP_STEP } else { -RAMP_STEP };
        }
        let cur = c.gain_cur;
        let done = c.gain_cur == c.gain_target;
        self.backend.set_volume(ch, cur);
        if done {
            self.chans[ch.index()].ramp_src = 0;
        }
        !done
    }

    /// A GLib timeout fired; true keeps it.
    fn on_timer(&mut self, t: Timer) -> bool {
        match t {
            Timer::Retry(ch) => {
                self.retry_tick(ch);
                false
            }
            Timer::Ramp(ch) => self.ramp_tick(ch),
        }
    }

    fn play(&mut self, ch: Channel, id: PlayId, urls: Vec<String>) {
        if urls.is_empty() {
            self.halt(ch, true);
            self.shared.complete(ch, id, false);
            return;
        }
        self.cancel_retry(ch);
        let c = &mut self.chans[ch.index()];
        c.urls = urls;
        c.index = 0;
        c.item = id;
        c.active = true;
        c.want_play = true;
        c.all_ok = true;
        c.retries = 0;
        c.resume_ns = 0;
        self.start_current(ch);
    }

    fn pause(&mut self, ch: Channel) {
        let c = &mut self.chans[ch.index()];
        if !c.active || !c.want_play {
            return;
        }
        c.want_play = false;
        self.apply_target_state(ch);
    }

    fn resume(&mut self, ch: Channel) {
        let c = &mut self.chans[ch.index()];
        if !c.active || c.want_play {
            return;
        }
        c.want_play = true;
        self.apply_target_state(ch);
        let c = &self.chans[ch.index()];
        if c.reached_playing && !c.buffering {
            self.set_state(ch, PlayState::Playing);
        }
    }

    fn stop(&mut self, ch: Channel) {
        if !self.chans[ch.index()].active {
            return;
        }
        info!("player: ch{} stop", ch.index());
        self.halt(ch, false);
    }

    fn set_gain(&mut self, ch: Channel, gain: f64) {
        let c = &mut self.chans[ch.index()];
        c.gain_target = gain;
        if !c.active {
            // Nothing audible: apply at once, no ramp.
            let src = std::mem::take(&mut c.ramp_src);
            c.gain_cur = gain;
            if src != 0 {
                self.backend.source_remove(src);
            }
            self.backend.set_volume(ch, gain);
            return;
        }
        if c.ramp_src == 0 {
            self.chans[ch.index()].ramp_src = self.backend.timeout_add(RAMP_MS, Timer::Ramp(ch));
        }
    }

    /// After the main loop ended: no timeout may outlive the machine.
    fn teardown(&mut self) {
        for ch in [Channel::Music, Channel::Announce] {
            let src = std::mem::take(&mut self.chans[ch.index()].ramp_src);
            if src != 0 {
                self.backend.source_remove(src);
            }
            self.cancel_retry(ch);
        }
    }
}

thread_local! {
    // The GLib thread's machine: GLib dispatches every source of the default main context (bus
    // watches, timeouts, idle commands) on that thread. GStreamer calls made by the machine never
    // dispatch sources themselves, so the borrow is never re-entered. A panic in the machine
    // unwinds into an extern "C" callback and aborts the process (release builds abort anyway):
    // the supervisor restarts the daemon.
    static MACHINE: RefCell<Option<Machine>> = const { RefCell::new(None) };
}

fn with_machine<R>(f: impl FnOnce(&mut Machine) -> R) -> Option<R> {
    MACHINE.with_borrow_mut(|m| m.as_mut().map(f))
}

/// Bus watch of channel `tag`.
pub(crate) fn on_bus(tag: usize, msg: &gst::Message) {
    let Some(ch) = Channel::from_index(tag) else {
        return;
    };
    with_machine(|m| {
        if m.chans[ch.index()].active {
            let msg = msg.parse(m.backend.element(ch));
            m.handle_message(ch, msg);
        }
    });
}

/// GLib timeout `tag`; true keeps it.
pub(crate) fn on_timer(tag: usize) -> bool {
    Timer::from_tag(tag)
        .and_then(|t| with_machine(|m| m.on_timer(t)))
        .unwrap_or(false)
}

/// The firmware's GStreamer: one playbin per channel.
struct GstBackend {
    api: &'static Api,
    pipes: [Pipeline; 2],
}

impl Backend for GstBackend {
    fn set_state(&mut self, ch: Channel, s: GstState) -> bool {
        self.pipes[ch.index()].set_state(s as i32)
    }

    fn set_uri(&mut self, ch: Channel, uri: &str) {
        // As a C string: up to the first NUL.
        if let Ok(uri) = CString::new(uri.split('\0').next().unwrap_or_default()) {
            self.pipes[ch.index()].set_uri(&uri);
        }
    }

    fn set_volume(&mut self, ch: Channel, v: f64) {
        self.pipes[ch.index()].set_volume(v);
    }

    fn query_position(&mut self, ch: Channel) -> Option<i64> {
        self.pipes[ch.index()].query_position()
    }

    fn can_seek(&self) -> bool {
        self.pipes[0].can_seek()
    }

    fn seek(&mut self, ch: Channel, pos_ns: i64) -> bool {
        self.pipes[ch.index()].seek(pos_ns)
    }

    fn resource_error_quark(&self) -> u32 {
        gst::resource_error_quark(self.api)
    }

    fn timeout_add(&mut self, ms: u32, t: Timer) -> u32 {
        gst::timeout_add(self.api, ms, t.tag())
    }

    fn source_remove(&mut self, id: u32) {
        gst::source_remove(self.api, id);
    }

    fn now_ms(&self) -> i64 {
        pulsar_core::now_ms()
    }

    fn element(&self, ch: Channel) -> *const c_void {
        self.pipes[ch.index()].addr()
    }
}

/// Where handle commands run.
trait Exec {
    fn invoke(&self, call: Call);
}

/// The GLib thread; stopped (after the commands queued before) and joined on drop.
struct GlibThread {
    api: &'static Api,
    main_loop: Arc<MainLoop>,
    thread: Option<JoinHandle<()>>,
}

impl Exec for GlibThread {
    fn invoke(&self, call: Call) {
        gst::idle_add(self.api, call);
    }
}

impl Drop for GlibThread {
    fn drop(&mut self) {
        let ml = Arc::clone(&self.main_loop);
        gst::idle_add(self.api, Box::new(move || ml.quit()));
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            error!("player: GLib thread panicked");
        }
    }
}

fn new_pipeline(api: &'static Api, ch: Channel, pcm: &str) -> Option<Pipeline> {
    let desc = format!(
        "playbin name=ch{} audio-sink=\"alsasink device={pcm}\"",
        ch.index()
    );
    let mut p = Pipeline::launch(api, &CString::new(desc).ok()?)?;
    p.set_flags(PLAY_FLAGS);
    p.set_volume(1.0);
    if ch == Channel::Announce {
        p.set_buffer_duration(ANNOUNCE_BUFFER_NS);
    }
    p.add_bus_watch(ch.index());
    Some(p)
}

/// dlopen + gst_init + static plugin registration + the GLib thread (C++ Player::init).
fn start(shared: &Arc<Shared>, pcm: &str) -> Option<GlibThread> {
    // One player per process: its GLib thread owns the default main context.
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        error!("player: already started, one player per process");
        return None;
    }
    let api = gst::load()?;
    gst::set_ssl_cert_file();
    // Process-global: alsa-lib messages go to the debug log.
    pulsar_alsa::quiet_error_handler();
    if !gst::init(api) {
        return None;
    }
    let pipes = [
        new_pipeline(api, Channel::Music, pcm)?,
        new_pipeline(api, Channel::Announce, pcm)?,
    ];
    let main_loop = Arc::new(MainLoop::new(api)?);
    let machine = Machine::new(Box::new(GstBackend { api, pipes }), Arc::clone(shared));
    let ml = Arc::clone(&main_loop);
    let thread = std::thread::Builder::new()
        .name("gst".into())
        .spawn(move || {
            MACHINE.set(Some(machine));
            ml.run();
            if let Some(mut m) = MACHINE.take() {
                m.teardown();
            }
        });
    match thread {
        Ok(t) => Some(GlibThread {
            api,
            main_loop,
            thread: Some(t),
        }),
        Err(e) => {
            error!("player: thread: {e}");
            None
        }
    }
}

struct Inner {
    shared: Arc<Shared>,
    next_id: Cell<u64>,
    /// None: degraded (init failed), every play() is Done(false) at once.
    exec: Option<Box<dyn Exec>>,
}

/// Player commands; the GLib thread stops when the last clone is dropped.
#[derive(Clone)]
pub struct PlayerHandle {
    inner: Rc<Inner>,
}

/// Starts the player on ALSA PCM `pcm` (`Model::playback_pcm`). A failed dlopen / gst_init is
/// logged ("player init failed") and gives a degraded handle: every play() ends with Done(false)
/// at once and state() stays Idle.
pub fn spawn(pcm: &str) -> (PlayerHandle, UnboundedReceiver<PlayerEvent>) {
    let (tx, rx) = unbounded_channel();
    let shared = Arc::new(Shared::new(tx));
    let exec = start(&shared, pcm).map(|t| Box::new(t) as Box<dyn Exec>);
    if exec.is_none() {
        error!("player init failed");
    }
    (PlayerHandle::new(shared, exec), rx)
}

impl PlayerHandle {
    fn new(shared: Arc<Shared>, exec: Option<Box<dyn Exec>>) -> PlayerHandle {
        PlayerHandle {
            inner: Rc::new(Inner {
                shared,
                next_id: Cell::new(1),
                exec,
            }),
        }
    }

    fn invoke(&self, f: impl FnOnce(&mut Machine) + Send + 'static) {
        if let Some(exec) = &self.inner.exec {
            exec.invoke(Box::new(move || {
                with_machine(f);
            }));
        }
    }

    /// Plays urls sequentially (http(s)://, file://, or an absolute path), replacing the
    /// channel's current playback. MUSIC retries an http(s) url that fails for network reasons
    /// (3 times, 1/2/4 s apart, resuming at the position reached) before moving on; local files
    /// and ANNOUNCE fail at once.
    pub fn play(&self, ch: Channel, urls: Vec<String>) -> PlayId {
        let id = PlayId(self.inner.next_id.get());
        self.inner.next_id.set(id.0 + 1);
        self.inner.shared.supersede(ch, Some(id));
        if self.inner.exec.is_none() {
            self.inner.shared.complete(ch, id, false);
        }
        self.invoke(move |m| m.play(ch, id, urls));
        id
    }

    pub fn pause(&self, ch: Channel) {
        self.invoke(move |m| m.pause(ch));
    }

    pub fn resume(&self, ch: Channel) {
        self.invoke(move |m| m.resume(ch));
    }

    pub fn stop(&self, ch: Channel) {
        self.inner.shared.supersede(ch, None);
        self.invoke(move |m| m.stop(ch));
    }

    /// Changed by the GLib thread only: right after a command it still reports the state from
    /// before it.
    pub fn state(&self, ch: Channel) -> PlayState {
        self.inner.shared.state(ch)
    }

    /// Per-channel gain 0..1 (ducking MUSIC while listening / speaking), ramped while playing.
    pub fn set_channel_gain(&self, ch: Channel, gain: f32) {
        let gain = clamp_gain(gain);
        self.invoke(move |m| m.set_gain(ch, gain));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    use Channel::{Announce, Music};
    use PlayState::{Error as Failed, Idle, Loading, Paused, Playing};
    use PlayerEvent::{Done, State};

    fn unescape(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(o) => out.push(o),
                None => {}
            }
        }
        out
    }

    #[test]
    fn to_uri_matches_cxx() {
        let golden = include_str!("../tests/golden/to_uri.txt");
        let mut n = 0;
        for line in golden.lines() {
            let (input, want) = line.split_once('\t').unwrap();
            assert_eq!(to_uri(&unescape(input)), unescape(want), "{line}");
            n += 1;
        }
        assert_eq!(n, 12);
    }

    #[test]
    fn soup_status_matches_cxx() {
        let golden = include_str!("../tests/golden/soup_status.txt");
        let mut n = 0;
        for line in golden.lines() {
            let (want, input) = line.split_once('\t').unwrap();
            let input = (input != "<null>").then(|| unescape(input));
            assert_eq!(
                soup_status(input.as_deref()),
                want.parse::<i32>().unwrap(),
                "{line}"
            );
            n += 1;
        }
        assert_eq!(n, 25);
    }

    fn err(debug: Option<&str>, domain_code: Option<(u32, i32)>) -> ErrorInfo {
        ErrorInfo {
            message: "x".into(),
            debug: debug.map(str::to_owned),
            domain_code,
        }
    }

    #[test]
    fn retryable_cases() {
        let q = || 7;
        let status =
            |s: i32| format!("Reason ({s}), URL: http://192.168.1.50/a, Redirect to: (NULL)");
        let http = "http://192.168.1.50/a.mp3";
        for (s, want) in [
            (0, true),
            (2, true),
            (7, true),
            (99, true),
            (100, false),
            (200, false),
            (403, false),
            (404, false),
            (408, true),
            (429, true),
            (499, false),
            (500, true),
            (503, true),
        ] {
            let e = err(Some(&status(s)), Some((7, 1)));
            assert_eq!(retryable(Music, http, &e, q), want, "status {s}");
            assert_eq!(
                retryable(Music, "https://example.com/a.mp3", &e, q),
                want,
                "https status {s}"
            );
            assert!(!retryable(Announce, http, &e, q), "announce status {s}");
        }
        // No status: the resource-error domain with OPEN_READ / READ.
        assert!(retryable(Music, http, &err(None, Some((7, 5))), q));
        assert!(retryable(
            Music,
            http,
            &err(Some("no status"), Some((7, 9))),
            q
        ));
        assert!(!retryable(Music, http, &err(None, Some((7, 1))), q));
        assert!(!retryable(Music, http, &err(None, Some((8, 5))), q));
        assert!(!retryable(Music, http, &err(None, Some((0, 5))), || 0));
        // Not a network url, or no GError at all.
        let e500 = err(Some(&status(500)), Some((7, 5)));
        assert!(!retryable(Music, "/data/a.mp3", &e500, q));
        assert!(!retryable(Music, "file:///data/a.mp3", &e500, q));
        assert!(!retryable(Music, "HTTP://x/a.mp3", &e500, q));
        assert!(!retryable(Music, http, &err(Some(&status(500)), None), q));
        // The quark is looked up only when there is no status.
        assert!(retryable(Music, http, &e500, || unreachable!()));
    }

    #[test]
    fn backoff_doubles_and_resets_after_a_minute() {
        assert_eq!(next_retry(0, 0, 10), Some((1, 1000)));
        assert_eq!(next_retry(1, 0, 10), Some((2, 2000)));
        assert_eq!(next_retry(2, 0, 10), Some((3, 4000)));
        assert_eq!(next_retry(3, 0, 10), None);
        assert_eq!(next_retry(3, 0, 59_999), None);
        assert_eq!(next_retry(3, 0, 60_000), Some((1, 1000)));
        assert_eq!(next_retry(2, 1_000, 61_000), Some((1, 1000)));
    }

    #[test]
    fn gain_is_clamped_like_cxx() {
        assert_eq!(clamp_gain(0.3), f64::from(0.3f32));
        assert_eq!(clamp_gain(-1.0), 0.0);
        assert_eq!(clamp_gain(2.0), 1.0);
        assert_eq!(clamp_gain(f32::NAN), 0.0);
        assert_eq!(clamp_gain(f32::INFINITY), 1.0);
    }

    #[test]
    fn timer_tags_round_trip() {
        for t in [
            Timer::Retry(Music),
            Timer::Ramp(Music),
            Timer::Retry(Announce),
            Timer::Ramp(Announce),
        ] {
            assert_eq!(Timer::from_tag(t.tag()), Some(t));
        }
        assert_eq!(Timer::from_tag(4), None);
    }

    // ---- state machine on a fake GStreamer ----

    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        timers: Vec<(u32, u32, Timer)>,
        next_src: u32,
        now: i64,
        fail_next_start: bool,
        position: Option<i64>,
        seek_ok: bool,
    }

    struct FakeBackend(Arc<Mutex<Fake>>);

    impl FakeBackend {
        fn log(&self, s: String) {
            self.0.lock().unwrap().calls.push(s);
        }
    }

    impl Backend for FakeBackend {
        fn set_state(&mut self, ch: Channel, s: GstState) -> bool {
            self.log(format!("ch{} {s:?}", ch.index()));
            let mut f = self.0.lock().unwrap();
            !(s != GstState::Null && std::mem::take(&mut f.fail_next_start))
        }
        fn set_uri(&mut self, ch: Channel, uri: &str) {
            self.log(format!("ch{} uri {uri}", ch.index()));
        }
        fn set_volume(&mut self, ch: Channel, v: f64) {
            self.log(format!("ch{} volume {v}", ch.index()));
        }
        fn query_position(&mut self, ch: Channel) -> Option<i64> {
            self.log(format!("ch{} position", ch.index()));
            self.0.lock().unwrap().position
        }
        fn can_seek(&self) -> bool {
            true
        }
        fn seek(&mut self, ch: Channel, pos_ns: i64) -> bool {
            self.log(format!("ch{} seek {pos_ns}", ch.index()));
            self.0.lock().unwrap().seek_ok
        }
        fn resource_error_quark(&self) -> u32 {
            7
        }
        fn timeout_add(&mut self, ms: u32, t: Timer) -> u32 {
            let mut f = self.0.lock().unwrap();
            f.next_src += 1;
            let id = f.next_src;
            f.timers.push((id, ms, t));
            id
        }
        fn source_remove(&mut self, id: u32) {
            let mut f = self.0.lock().unwrap();
            let n = f.timers.len();
            f.timers.retain(|&(i, _, _)| i != id);
            assert_eq!(f.timers.len() + 1, n, "removed an unknown source {id}");
        }
        fn now_ms(&self) -> i64 {
            self.0.lock().unwrap().now
        }
        fn element(&self, _: Channel) -> *const c_void {
            std::ptr::null()
        }
    }

    struct QueueExec(Rc<RefCell<VecDeque<Call>>>);

    impl Exec for QueueExec {
        fn invoke(&self, call: Call) {
            self.0.borrow_mut().push_back(call);
        }
    }

    /// A handle whose commands queue up until `pump` (the GLib thread's turn) and a machine on
    /// a fake backend, both on the test thread.
    struct Rig {
        h: PlayerHandle,
        rx: UnboundedReceiver<PlayerEvent>,
        queue: Rc<RefCell<VecDeque<Call>>>,
        fake: Arc<Mutex<Fake>>,
    }

    impl Rig {
        fn new() -> Rig {
            let (tx, rx) = unbounded_channel();
            let shared = Arc::new(Shared::new(tx));
            let fake = Arc::new(Mutex::new(Fake::default()));
            MACHINE.set(Some(Machine::new(
                Box::new(FakeBackend(Arc::clone(&fake))),
                Arc::clone(&shared),
            )));
            let queue = Rc::new(RefCell::new(VecDeque::new()));
            let h = PlayerHandle::new(shared, Some(Box::new(QueueExec(Rc::clone(&queue)))));
            Rig { h, rx, queue, fake }
        }

        fn pump(&self) {
            loop {
                let call = self.queue.borrow_mut().pop_front();
                match call {
                    Some(call) => call(),
                    None => break,
                }
            }
        }

        fn msg(&self, ch: Channel, m: Msg) {
            with_machine(|mm| mm.handle_message(ch, m));
        }

        fn state_changed(&self, ch: Channel, s: GstState) {
            self.msg(ch, Msg::StateChanged { new: s as i32 });
        }

        fn events(&mut self) -> Vec<PlayerEvent> {
            let mut v = Vec::new();
            while let Ok(e) = self.rx.try_recv() {
                v.push(e);
            }
            v
        }

        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut self.fake.lock().unwrap().calls)
        }

        fn timers(&self) -> Vec<(u32, Timer)> {
            let f = self.fake.lock().unwrap();
            f.timers.iter().map(|&(_, ms, t)| (ms, t)).collect()
        }

        /// Fires pending timeout `t` as GLib would; returns whether it stays armed.
        fn fire(&self, t: Timer) -> bool {
            let entry = {
                let mut f = self.fake.lock().unwrap();
                let i = f
                    .timers
                    .iter()
                    .position(|&(_, _, x)| x == t)
                    .expect("timer armed");
                f.timers.remove(i)
            };
            let keep = with_machine(|m| m.on_timer(t)).unwrap();
            if keep {
                self.fake.lock().unwrap().timers.push(entry);
            }
            keep
        }

        fn now(&self, ms: i64) {
            self.fake.lock().unwrap().now = ms;
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            MACHINE.take();
        }
    }

    fn urls(u: &[&str]) -> Vec<String> {
        u.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn plays_to_eos_with_states_before_done() {
        let mut r = Rig::new();
        r.h.set_channel_gain(Music, 0.3);
        let id = r.h.play(Music, urls(&["/a.mp3"]));
        assert_eq!(id, PlayId(1));
        // Nothing changes before the GLib thread ran the commands.
        assert_eq!(r.h.state(Music), Idle);
        assert!(r.events().is_empty());
        r.pump();
        assert_eq!(
            r.calls(),
            [
                "ch0 volume 0.30000001192092896",
                "ch0 Null",
                "ch0 uri file:///a.mp3",
                "ch0 Playing"
            ]
        );
        assert_eq!(r.events(), [State(Music, Loading)]);
        assert_eq!(r.h.state(Music), Loading);
        r.state_changed(Music, GstState::Paused);
        assert!(r.events().is_empty());
        r.state_changed(Music, GstState::Playing);
        assert_eq!(r.events(), [State(Music, Playing)]);
        assert_eq!(r.h.state(Music), Playing);
        r.msg(Music, Msg::Eos);
        assert_eq!(r.calls(), ["ch0 Null"]);
        assert_eq!(r.events(), [State(Music, Idle), Done { id, ok: true }]);
        // Messages of an idle channel are ignored.
        r.msg(Music, Msg::Eos);
        assert!(r.events().is_empty());
    }

    #[test]
    fn failed_item_continues_the_list_and_fails_it() {
        let mut r = Rig::new();
        let id =
            r.h.play(Announce, urls(&["/a.wav", "http://192.168.1.50/b.flac"]));
        r.pump();
        r.state_changed(Announce, GstState::Playing);
        r.msg(Announce, Msg::Eos);
        assert_eq!(
            r.events(),
            [
                State(Announce, Loading),
                State(Announce, Playing),
                State(Announce, Loading)
            ]
        );
        r.calls();
        // ANNOUNCE never retries, even a 5xx.
        r.msg(
            Announce,
            Msg::Error(err(
                Some("Internal Server Error (500), URL: x"),
                Some((7, 5)),
            )),
        );
        assert_eq!(r.calls(), ["ch1 Null"]);
        assert!(r.timers().is_empty());
        assert_eq!(
            r.events(),
            [
                State(Announce, Failed),
                State(Announce, Idle),
                Done { id, ok: false }
            ]
        );
    }

    #[test]
    fn replacing_play_delivers_false_once_before_the_new_list_reports() {
        let mut r = Rig::new();
        let a = r.h.play(Music, urls(&["/a.mp3"]));
        r.pump();
        r.state_changed(Music, GstState::Playing);
        r.events();
        let b = r.h.play(Music, urls(&["/b.mp3"]));
        assert_eq!(b, PlayId(2));
        // Queued by play() itself; the state still shows the old playback.
        assert_eq!(r.events(), [Done { id: a, ok: false }]);
        assert_eq!(r.h.state(Music), Playing);
        r.pump();
        assert_eq!(r.events(), [State(Music, Loading)]);
        r.state_changed(Music, GstState::Playing);
        r.msg(Music, Msg::Eos);
        assert_eq!(
            r.events(),
            [
                State(Music, Playing),
                State(Music, Idle),
                Done { id: b, ok: true }
            ]
        );
    }

    #[test]
    fn list_finished_after_its_replacement_was_requested_is_not_reported_again() {
        let mut r = Rig::new();
        let a = r.h.play(Music, urls(&["/a.mp3"]));
        r.pump();
        let b = r.h.play(Music, urls(&["/b.mp3"]));
        // The GLib thread ends `a` before it runs the second play().
        r.msg(Music, Msg::Eos);
        r.pump();
        assert_eq!(
            r.events(),
            [
                State(Music, Loading),
                Done { id: a, ok: false },
                State(Music, Idle),
                State(Music, Loading)
            ]
        );
        r.msg(Music, Msg::Eos);
        assert_eq!(r.events(), [State(Music, Idle), Done { id: b, ok: true }]);
    }

    #[test]
    fn stop_delivers_false_once() {
        let mut r = Rig::new();
        // Nothing owed, nothing playing: no event.
        r.h.stop(Music);
        r.pump();
        assert!(r.events().is_empty());
        let a = r.h.play(Music, urls(&["/a.mp3"]));
        r.pump();
        r.state_changed(Music, GstState::Playing);
        r.events();
        r.calls();
        r.h.stop(Music);
        assert_eq!(r.events(), [Done { id: a, ok: false }]);
        assert_eq!(r.h.state(Music), Playing);
        r.pump();
        assert_eq!(r.calls(), ["ch0 Null"]);
        assert_eq!(r.events(), [State(Music, Idle)]);
        r.h.stop(Music);
        r.pump();
        assert!(r.events().is_empty());
        assert!(r.calls().is_empty());
    }

    #[test]
    fn empty_list_reports_error_then_idle() {
        let mut r = Rig::new();
        let id = r.h.play(Announce, Vec::new());
        r.pump();
        assert_eq!(
            r.events(),
            [
                State(Announce, Failed),
                State(Announce, Idle),
                Done { id, ok: false }
            ]
        );
    }

    #[test]
    fn pause_and_resume() {
        let mut r = Rig::new();
        // An inactive channel ignores both.
        r.h.pause(Music);
        r.h.resume(Music);
        r.pump();
        assert!(r.events().is_empty());
        assert!(r.calls().is_empty());
        r.h.play(Music, urls(&["/a.mp3"]));
        r.pump();
        r.state_changed(Music, GstState::Playing);
        r.events();
        r.calls();
        r.h.pause(Music);
        r.pump();
        assert_eq!(r.calls(), ["ch0 Paused"]);
        assert_eq!(r.events(), [State(Music, Paused)]);
        r.h.pause(Music);
        r.pump();
        assert!(r.calls().is_empty());
        r.h.resume(Music);
        r.pump();
        assert_eq!(r.calls(), ["ch0 Playing"]);
        assert_eq!(r.events(), [State(Music, Playing)]);
    }

    #[test]
    fn paused_list_starts_its_next_item_paused() {
        let mut r = Rig::new();
        r.h.play(Music, urls(&["/a.mp3", "/b.mp3"]));
        r.pump();
        r.h.pause(Music);
        r.pump();
        r.msg(Music, Msg::Eos);
        assert_eq!(r.events(), [State(Music, Loading), State(Music, Paused)]);
        assert_eq!(
            r.calls(),
            [
                "ch0 Null",
                "ch0 uri file:///a.mp3",
                "ch0 Playing",
                "ch0 Paused",
                "ch0 Null",
                "ch0 uri file:///b.mp3",
                "ch0 Paused"
            ]
        );
    }

    #[test]
    fn buffering_holds_the_pipeline_and_reports_loading() {
        let mut r = Rig::new();
        r.h.play(Music, urls(&["http://192.168.1.50/a.mp3"]));
        r.pump();
        r.calls();
        r.msg(Music, Msg::Buffering(10));
        assert_eq!(r.calls(), ["ch0 Paused"]);
        r.msg(Music, Msg::Buffering(50));
        assert!(r.calls().is_empty());
        r.msg(Music, Msg::Buffering(100));
        assert_eq!(r.calls(), ["ch0 Playing"]);
        r.state_changed(Music, GstState::Playing);
        assert_eq!(r.events(), [State(Music, Loading), State(Music, Playing)]);
        r.msg(Music, Msg::Buffering(20));
        assert_eq!(r.calls(), ["ch0 Paused"]);
        assert_eq!(r.events(), [State(Music, Loading)]);
        r.msg(Music, Msg::Buffering(100));
        assert_eq!(r.calls(), ["ch0 Playing"]);
        // PLAYING again only once the pipeline says so.
        assert!(r.events().is_empty());
        r.state_changed(Music, GstState::Playing);
        assert_eq!(r.events(), [State(Music, Playing)]);
    }

    #[test]
    fn network_failure_retries_at_the_position_reached() {
        let mut r = Rig::new();
        let url = "http://192.168.1.50/a.mp3";
        let e500 = || {
            Msg::Error(err(
                Some("Internal Server Error (500), URL: x"),
                Some((7, 5)),
            ))
        };
        let id = r.h.play(Music, urls(&[url]));
        r.pump();
        r.state_changed(Music, GstState::Playing);
        r.events();
        r.calls();
        r.fake.lock().unwrap().position = Some(5_000_000_000);
        r.fake.lock().unwrap().seek_ok = true;
        r.now(10_000);
        r.msg(Music, e500());
        assert_eq!(r.calls(), ["ch0 position", "ch0 Null"]);
        assert_eq!(r.events(), [State(Music, Loading)]);
        assert_eq!(r.timers(), [(1000, Timer::Retry(Music))]);
        assert!(!r.fire(Timer::Retry(Music)));
        assert_eq!(
            r.calls(),
            ["ch0 Null", format!("ch0 uri {url}").as_str(), "ch0 Playing"]
        );
        r.state_changed(Music, GstState::Paused);
        assert_eq!(r.calls(), ["ch0 seek 5000000000"]);
        r.state_changed(Music, GstState::Playing);
        assert_eq!(r.events(), [State(Music, Playing)]);
        // Further failures of the same outage: 2 s, 4 s, then the url fails.
        r.fake.lock().unwrap().position = None;
        r.msg(Music, e500());
        assert_eq!(r.timers(), [(2000, Timer::Retry(Music))]);
        r.fire(Timer::Retry(Music));
        r.msg(Music, e500());
        assert_eq!(r.timers(), [(4000, Timer::Retry(Music))]);
        r.fire(Timer::Retry(Music));
        r.calls();
        // No seek without a position.
        r.state_changed(Music, GstState::Paused);
        assert!(r.calls().is_empty());
        r.msg(Music, e500());
        assert!(r.timers().is_empty());
        assert_eq!(
            r.events(),
            [
                State(Music, Loading),
                State(Music, Failed),
                State(Music, Idle),
                Done { id, ok: false }
            ]
        );
    }

    #[test]
    fn retries_count_again_after_a_minute_of_playback() {
        let r = Rig::new();
        let e = || Msg::Error(err(None, Some((7, 9))));
        r.h.play(Music, urls(&["https://example.com/a.mp3"]));
        r.pump();
        for delay in [1000, 2000] {
            r.msg(Music, e());
            assert_eq!(r.timers(), [(delay, Timer::Retry(Music))]);
            r.fire(Timer::Retry(Music));
        }
        r.now(60_000);
        r.msg(Music, e());
        assert_eq!(r.timers(), [(1000, Timer::Retry(Music))]);
    }

    #[test]
    fn retry_wait_keeps_the_pipeline_down_and_stop_cancels_it() {
        let mut r = Rig::new();
        let id = r.h.play(Music, urls(&["http://192.168.1.50/a.mp3"]));
        r.pump();
        r.msg(
            Music,
            Msg::Error(err(Some("Service Unavailable (503), URL: x"), Some((7, 5)))),
        );
        r.events();
        r.calls();
        r.h.pause(Music);
        r.pump();
        assert!(r.calls().is_empty());
        assert_eq!(r.events(), [State(Music, Paused)]);
        r.h.stop(Music);
        r.pump();
        assert!(r.timers().is_empty());
        assert_eq!(r.events(), [Done { id, ok: false }, State(Music, Idle)]);
    }

    #[test]
    fn http_status_and_local_file_failures_are_final() {
        let mut r = Rig::new();
        let id = r.h.play(Music, urls(&["http://192.168.1.50/missing.flac"]));
        r.pump();
        r.msg(
            Music,
            Msg::Error(err(
                Some("Not Found (404), URL: http://192.168.1.50/missing.flac"),
                Some((7, 5)),
            )),
        );
        assert!(r.timers().is_empty());
        assert_eq!(
            r.events(),
            [
                State(Music, Loading),
                State(Music, Failed),
                State(Music, Idle),
                Done { id, ok: false }
            ]
        );
        let id = r.h.play(Music, urls(&["/data/a.mp3"]));
        r.pump();
        r.msg(Music, Msg::Error(err(None, Some((7, 5)))));
        assert!(r.timers().is_empty());
        assert_eq!(r.events().last(), Some(&Done { id, ok: false }));
    }

    #[test]
    fn failed_start_moves_on() {
        let mut r = Rig::new();
        r.fake.lock().unwrap().fail_next_start = true;
        let id = r.h.play(Music, urls(&["/a.mp3", "/b.mp3"]));
        r.pump();
        assert_eq!(
            r.calls(),
            [
                "ch0 Null",
                "ch0 uri file:///a.mp3",
                "ch0 Playing",
                "ch0 Null",
                "ch0 uri file:///b.mp3",
                "ch0 Playing"
            ]
        );
        r.msg(Music, Msg::Eos);
        assert_eq!(
            r.events(),
            [
                State(Music, Loading),
                State(Music, Failed),
                State(Music, Idle),
                Done { id, ok: false }
            ]
        );
    }

    #[test]
    fn clock_lost_restarts_the_pipeline_clock() {
        let mut r = Rig::new();
        r.h.play(Music, urls(&["/a.mp3"]));
        r.pump();
        r.state_changed(Music, GstState::Playing);
        r.events();
        r.calls();
        r.msg(Music, Msg::ClockLost);
        assert_eq!(r.calls(), ["ch0 Paused", "ch0 Playing"]);
        assert!(r.events().is_empty());
        r.msg(Music, Msg::Warning("w".into()));
        r.msg(Music, Msg::Other);
        assert!(r.calls().is_empty());
        assert!(r.events().is_empty());
    }

    #[test]
    fn gain_ramps_while_playing_and_jumps_while_idle() {
        let r = Rig::new();
        r.h.set_channel_gain(Music, 0.2);
        r.pump();
        assert_eq!(r.calls(), ["ch0 volume 0.20000000298023224"]);
        assert!(r.timers().is_empty());
        r.h.set_channel_gain(Music, 1.0);
        r.pump();
        r.calls();
        r.h.play(Music, urls(&["/a.mp3"]));
        r.pump();
        r.calls();
        r.h.set_channel_gain(Music, 0.08);
        r.h.set_channel_gain(Music, 0.08);
        r.pump();
        assert_eq!(r.timers(), [(20, Timer::Ramp(Music))]);
        let mut ticks = 0;
        while r.fire(Timer::Ramp(Music)) {
            ticks += 1;
        }
        let vols: Vec<f64> = r
            .calls()
            .iter()
            .map(|c| c.strip_prefix("ch0 volume ").unwrap().parse().unwrap())
            .collect();
        assert_eq!(vols.len(), ticks + 1);
        assert!((vols[0] - 0.95).abs() < 1e-12);
        assert!(vols.windows(2).all(|w| w[1] < w[0]));
        assert_eq!(*vols.last().unwrap(), f64::from(0.08f32));
        assert!(r.timers().is_empty());
    }

    #[test]
    fn degraded_player_fails_every_play_at_once() {
        // No firmware GStreamer on the host: spawn() gives the degraded handle.
        let (h, mut rx) = spawn("speakers");
        let a = h.play(Music, urls(&["/a.mp3"]));
        assert_eq!(rx.try_recv().unwrap(), Done { id: a, ok: false });
        h.pause(Music);
        h.stop(Music);
        h.set_channel_gain(Music, 0.5);
        let b = h.play(Announce, Vec::new());
        assert_eq!(b, PlayId(2));
        assert_eq!(rx.try_recv().unwrap(), Done { id: b, ok: false });
        assert!(rx.try_recv().is_err());
        assert_eq!(h.state(Music), Idle);
        assert_eq!(h.state(Announce), Idle);
    }
}
