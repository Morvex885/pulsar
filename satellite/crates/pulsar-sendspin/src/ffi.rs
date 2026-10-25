//! Bindings of shim/sendspin_shim.h and a safe `Client` over them. Every unsafe block of the
//! crate is here.
//!
//! Panics: the callbacks are `extern "C"`, so a panic in one aborts the process instead of
//! unwinding into C++ (and the release profile aborts on panic anyway). C++ exceptions never
//! reach Rust: every `ss_*` function catches them.

use std::ffi::{OsStr, c_char, c_void};
use std::marker::{PhantomData, PhantomPinned};
use std::mem::{offset_of, size_of};
use std::os::unix::ffi::OsStrExt as _;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::event::{LibEvent, StreamParams};

#[repr(C)]
struct SsClient {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

#[repr(C)]
struct SsBlob {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SsStr {
    ptr: *const c_char,
    len: usize,
}

impl SsStr {
    fn new(s: &str) -> Self {
        Self {
            ptr: s.as_ptr().cast(),
            len: s.len(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SsFormat {
    sample_rate: u32,
    codec: u8,
    channels: u8,
    bit_depth: u8,
}

#[repr(C)]
struct SsConfig {
    name: SsStr,
    product_name: SsStr,
    manufacturer: SsStr,
    software_version: SsStr,
    mac: SsStr,
    pairing_psk_location: SsStr,
    formats: *const SsFormat,
    n_formats: usize,
    vis_types: *const u8,
    n_vis_types: usize,
    vis_buffer_capacity: usize,
    vis_display_offset_ms: i32,
    port: u16,
    vis_rate_max: u16,
    vis_f_min: u16,
    vis_f_max: u16,
    vis_bins: u8,
    vis_scale: u8,
    pairing_code_display_digits: bool,
    unpaired_access: bool,
}

type User = *mut c_void;

#[repr(C)]
struct SsCallbacks {
    group_update: unsafe extern "C" fn(User, i8, *const c_char, usize),
    time_sync: unsafe extern "C" fn(User, f32),
    pairing_started: unsafe extern "C" fn(User, *const c_char, usize),
    pairing_succeeded: unsafe extern "C" fn(User, *const c_char, usize),
    pairing_failed: unsafe extern "C" fn(User, *const c_char, usize, u8),
    trust_changed: unsafe extern "C" fn(User, bool),
    display_pairing_code: unsafe extern "C" fn(User, *const c_char, usize),
    clear_pairing_code: unsafe extern "C" fn(User),
    open_pairing_window: unsafe extern "C" fn(User),
    close_pairing_window: unsafe extern "C" fn(User),
    stream_start: unsafe extern "C" fn(User, bool, u8, u32, u8, u8),
    stream_end: unsafe extern "C" fn(User),
    volume_changed: unsafe extern "C" fn(User, u8),
    mute_changed: unsafe extern "C" fn(User, bool),
    output_delay_changed: unsafe extern "C" fn(User, u16),
    controller_state: unsafe extern "C" fn(User, usize, u8, bool),
    metadata: unsafe extern "C" fn(User, *const c_char, usize, *const c_char, usize),
    visualizer_stream_start: unsafe extern "C" fn(User, usize, u16),
    visualizer_stream_end: unsafe extern "C" fn(User),
    load_blob: unsafe extern "C" fn(User, *const c_char, usize, *mut SsBlob) -> bool,
    save_blob: unsafe extern "C" fn(User, *const c_char, usize, *const u8, usize) -> bool,
    audio_write: unsafe extern "C" fn(User, *const u8, usize, u32, *mut u32, *mut i64) -> usize,
    loudness: unsafe extern "C" fn(User, u16),
    beat: unsafe extern "C" fn(User),
    spectrum: unsafe extern "C" fn(User, *const u16, usize),
}

type SsPut = unsafe extern "C" fn(*mut c_void, *const c_char, usize);

// Same numbers as the static_asserts of sendspin_shim.cpp (P = pointer size).
const P: usize = size_of::<usize>();
const _: () = {
    assert!(size_of::<SsStr>() == 2 * P && offset_of!(SsStr, len) == P);
    assert!(size_of::<SsFormat>() == 8);
    assert!(offset_of!(SsFormat, codec) == 4);
    assert!(offset_of!(SsFormat, channels) == 5);
    assert!(offset_of!(SsFormat, bit_depth) == 6);
    assert!(offset_of!(SsConfig, product_name) == 2 * P);
    assert!(offset_of!(SsConfig, manufacturer) == 4 * P);
    assert!(offset_of!(SsConfig, software_version) == 6 * P);
    assert!(offset_of!(SsConfig, mac) == 8 * P);
    assert!(offset_of!(SsConfig, pairing_psk_location) == 10 * P);
    assert!(offset_of!(SsConfig, formats) == 12 * P);
    assert!(offset_of!(SsConfig, n_formats) == 13 * P);
    assert!(offset_of!(SsConfig, vis_types) == 14 * P);
    assert!(offset_of!(SsConfig, n_vis_types) == 15 * P);
    assert!(offset_of!(SsConfig, vis_buffer_capacity) == 16 * P);
    assert!(offset_of!(SsConfig, vis_display_offset_ms) == 17 * P);
    assert!(offset_of!(SsConfig, port) == 17 * P + 4);
    assert!(offset_of!(SsConfig, vis_rate_max) == 17 * P + 6);
    assert!(offset_of!(SsConfig, vis_f_min) == 17 * P + 8);
    assert!(offset_of!(SsConfig, vis_f_max) == 17 * P + 10);
    assert!(offset_of!(SsConfig, vis_bins) == 17 * P + 12);
    assert!(offset_of!(SsConfig, vis_scale) == 17 * P + 13);
    assert!(offset_of!(SsConfig, pairing_code_display_digits) == 17 * P + 14);
    assert!(offset_of!(SsConfig, unpaired_access) == 17 * P + 15);
    assert!(size_of::<SsConfig>() == 17 * P + 16);
    assert!(size_of::<SsCallbacks>() == 25 * P);
    assert!(offset_of!(SsCallbacks, group_update) == 0);
    assert!(offset_of!(SsCallbacks, time_sync) == P);
    assert!(offset_of!(SsCallbacks, pairing_started) == 2 * P);
    assert!(offset_of!(SsCallbacks, pairing_succeeded) == 3 * P);
    assert!(offset_of!(SsCallbacks, pairing_failed) == 4 * P);
    assert!(offset_of!(SsCallbacks, trust_changed) == 5 * P);
    assert!(offset_of!(SsCallbacks, display_pairing_code) == 6 * P);
    assert!(offset_of!(SsCallbacks, clear_pairing_code) == 7 * P);
    assert!(offset_of!(SsCallbacks, open_pairing_window) == 8 * P);
    assert!(offset_of!(SsCallbacks, close_pairing_window) == 9 * P);
    assert!(offset_of!(SsCallbacks, stream_start) == 10 * P);
    assert!(offset_of!(SsCallbacks, stream_end) == 11 * P);
    assert!(offset_of!(SsCallbacks, volume_changed) == 12 * P);
    assert!(offset_of!(SsCallbacks, mute_changed) == 13 * P);
    assert!(offset_of!(SsCallbacks, output_delay_changed) == 14 * P);
    assert!(offset_of!(SsCallbacks, controller_state) == 15 * P);
    assert!(offset_of!(SsCallbacks, metadata) == 16 * P);
    assert!(offset_of!(SsCallbacks, visualizer_stream_start) == 17 * P);
    assert!(offset_of!(SsCallbacks, visualizer_stream_end) == 18 * P);
    assert!(offset_of!(SsCallbacks, load_blob) == 19 * P);
    assert!(offset_of!(SsCallbacks, save_blob) == 20 * P);
    assert!(offset_of!(SsCallbacks, audio_write) == 21 * P);
    assert!(offset_of!(SsCallbacks, loudness) == 22 * P);
    assert!(offset_of!(SsCallbacks, beat) == 23 * P);
    assert!(offset_of!(SsCallbacks, spectrum) == 24 * P);
};

unsafe extern "C" {
    safe fn ss_set_log_level(level: u8);
    safe fn ss_now_us() -> i64;
    fn ss_create(cfg: *const SsConfig, cb: *const SsCallbacks, user: User) -> *mut SsClient;
    fn ss_destroy(c: *mut SsClient);
    fn ss_start(c: *mut SsClient) -> i32;
    fn ss_loop(c: *mut SsClient) -> bool;
    fn ss_is_connected(c: *const SsClient) -> bool;
    fn ss_server_info(
        c: *const SsClient,
        put: SsPut,
        name_dst: *mut c_void,
        id_dst: *mut c_void,
    ) -> bool;
    fn ss_client_id(c: *const SsClient, put: SsPut, dst: *mut c_void) -> bool;
    fn ss_pairing_token(c: *const SsClient, put: SsPut, dst: *mut c_void) -> bool;
    fn ss_player_set_output_delay_adjustable(c: *mut SsClient, adjustable: bool);
    fn ss_player_volume(c: *const SsClient) -> u8;
    fn ss_player_muted(c: *const SsClient) -> bool;
    fn ss_player_update_volume(c: *mut SsClient, volume: u8);
    fn ss_player_update_muted(c: *mut SsClient, muted: bool);
    fn ss_controller_send(c: *mut SsClient, command: u8) -> bool;
    fn ss_leave(c: *mut SsClient);
    fn ss_confirm_pairing_window(c: *mut SsClient);
    fn ss_blob_set(blob: *mut SsBlob, data: *const u8, len: usize) -> bool;
}

/// sendspin::LogLevel::INFO for the library's own log lines (stderr).
pub(crate) fn set_log_level_info() {
    ss_set_log_level(3);
}

/// The library's clock in µs, any thread: deadlines and notify_audio_played timestamps.
pub(crate) fn now_us() -> i64 {
    ss_now_us()
}

/// sendspin::SendspinCodecFormat.
#[derive(Clone, Copy)]
pub(crate) enum Codec {
    Flac = 0,
    Opus = 1,
    Pcm = 2,
}

/// sendspin::VisualizerDataType (the ones the service asks for).
#[derive(Clone, Copy)]
pub(crate) enum VisType {
    Beat = 0,
    Loudness = 1,
    Spectrum = 3,
}

/// sendspin::VisualizerSpectrumScale.
#[derive(Clone, Copy)]
pub(crate) enum Scale {
    Log = 1,
}

/// sendspin::SendspinControllerCommand (transport subset).
#[derive(Clone, Copy)]
pub(crate) enum Command {
    Play = 0,
    Pause = 1,
    Stop = 2,
    Next = 3,
    Previous = 4,
}

pub(crate) struct Format {
    pub codec: Codec,
    pub channels: u8,
    pub rate: u32,
    pub bits: u8,
}

pub(crate) struct Visualizer<'a> {
    pub buffer_capacity: usize,
    pub types: &'a [VisType],
    pub rate_max: u16,
    pub bins: u8,
    pub scale: Scale,
    pub f_min: u16,
    pub f_max: u16,
    pub display_offset_ms: i32,
}

pub(crate) struct Config<'a> {
    pub name: &'a str,
    pub product_name: &'a str,
    pub manufacturer: &'a str,
    pub software_version: &'a str,
    /// Empty: the library detects it.
    pub mac: &'a str,
    pub port: u16,
    pub formats: &'a [Format],
    pub visualizer: Visualizer<'a>,
    /// Dynamic pairing code on the DISPLAY channel in the DIGITS format.
    pub pairing_code_display_digits: bool,
    /// Where the operator finds the pairing token; empty: no hint.
    pub pairing_psk_location: &'a str,
    pub unpaired_access: bool,
}

/// Receiver of every library callback. None of its methods may call into the `Client`.
pub(crate) trait Listener: Sync {
    /// Main loop thread: from inside `Client::run_loop`, `Client::start` and the drop.
    fn event(&self, ev: LibEvent);
    /// Persistence provider, main loop thread. None: not stored.
    fn load_blob(&self, key: &OsStr) -> Option<Vec<u8>>;
    /// true once durable.
    fn save_blob(&self, key: &OsStr, data: &[u8]) -> bool;
    /// Player sync task: (bytes consumed, frames played to report, their finish time in µs of
    /// `now_us`). May block up to `timeout_ms`.
    fn audio_write(&self, data: &[u8], timeout_ms: u32) -> (usize, u32, i64);
    /// Visualizer drain thread.
    fn loudness(&self, v: u16);
    fn beat(&self);
    fn spectrum(&self, bins: &[u16]);
}

/// # Safety
/// `user` is the listener pointer `Client::new` registered for this `L`, and the client is not
/// destroyed yet (it is kept alive by the `Client` until after `ss_destroy` returns).
unsafe fn listener<'a, L: Listener>(user: User) -> &'a L {
    // SAFETY: see the function contract; only shared references are ever made.
    unsafe { &*user.cast_const().cast::<L>() }
}

/// # Safety
/// `ptr` is NULL, or `len` elements from it are readable and stay unchanged for `'a`.
unsafe fn slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if ptr.is_null() || len == 0 {
        return &[];
    }
    // SAFETY: see the function contract (non-NULL here).
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// # Safety
/// As `slice`.
unsafe fn string(ptr: *const c_char, len: usize) -> String {
    // SAFETY: forwarded contract.
    String::from_utf8_lossy(unsafe { slice(ptr.cast::<u8>(), len) }).into_owned()
}

/// # Safety
/// As `slice`; NULL = absent.
unsafe fn opt_string(ptr: *const c_char, len: usize) -> Option<String> {
    // SAFETY: forwarded contract.
    (!ptr.is_null()).then(|| unsafe { string(ptr, len) })
}

// Callback trampolines. The shim passes valid strings / buffers for the duration of each call
// (see sendspin_shim.h) and the `user` pointer of `Client::new`.

unsafe extern "C" fn cb_group<L: Listener>(u: User, state: i8, name: *const c_char, len: usize) {
    // SAFETY: shim contract (see above).
    let (l, name) = unsafe { (listener::<L>(u), opt_string(name, len)) };
    let playing = (state >= 0).then_some(state == 0);
    l.event(LibEvent::Group { playing, name });
}

unsafe extern "C" fn cb_time_sync<L: Listener>(u: User, error: f32) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::TimeSync(error));
}

unsafe extern "C" fn cb_pairing_started<L: Listener>(u: User, id: *const c_char, len: usize) {
    // SAFETY: shim contract.
    let (l, id) = unsafe { (listener::<L>(u), string(id, len)) };
    l.event(LibEvent::PairingStarted(id));
}

unsafe extern "C" fn cb_pairing_succeeded<L: Listener>(u: User, id: *const c_char, len: usize) {
    // SAFETY: shim contract.
    let (l, id) = unsafe { (listener::<L>(u), string(id, len)) };
    l.event(LibEvent::PairingSucceeded(id));
}

unsafe extern "C" fn cb_pairing_failed<L: Listener>(
    u: User,
    id: *const c_char,
    len: usize,
    reason: u8,
) {
    // SAFETY: shim contract.
    let (l, server_id) = unsafe { (listener::<L>(u), string(id, len)) };
    l.event(LibEvent::PairingFailed { server_id, reason });
}

unsafe extern "C" fn cb_trust<L: Listener>(u: User, paired: bool) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::Trust { paired });
}

unsafe extern "C" fn cb_pairing_code<L: Listener>(u: User, code: *const c_char, len: usize) {
    // SAFETY: shim contract.
    let (l, code) = unsafe { (listener::<L>(u), string(code, len)) };
    l.event(LibEvent::PairingCode(code));
}

unsafe extern "C" fn cb_pairing_code_cleared<L: Listener>(u: User) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::PairingCodeCleared);
}

unsafe extern "C" fn cb_pairing_window_open<L: Listener>(u: User) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::PairingWindow(true));
}

unsafe extern "C" fn cb_pairing_window_close<L: Listener>(u: User) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::PairingWindow(false));
}

unsafe extern "C" fn cb_stream_start<L: Listener>(
    u: User,
    complete: bool,
    codec: u8,
    rate: u32,
    channels: u8,
    bits: u8,
) {
    let params = complete.then_some(StreamParams {
        codec,
        rate,
        channels,
        bits,
    });
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::StreamStart(params));
}

unsafe extern "C" fn cb_stream_end<L: Listener>(u: User) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::StreamEnd);
}

unsafe extern "C" fn cb_volume<L: Listener>(u: User, volume: u8) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::Volume(volume));
}

unsafe extern "C" fn cb_mute<L: Listener>(u: User, muted: bool) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::Mute(muted));
}

unsafe extern "C" fn cb_output_delay<L: Listener>(u: User, ms: u16) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::OutputDelay(ms));
}

unsafe extern "C" fn cb_controller_state<L: Listener>(
    u: User,
    commands: usize,
    volume: u8,
    muted: bool,
) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::ControllerState {
        commands,
        volume,
        muted,
    });
}

unsafe extern "C" fn cb_metadata<L: Listener>(
    u: User,
    artist: *const c_char,
    artist_len: usize,
    title: *const c_char,
    title_len: usize,
) {
    // SAFETY: shim contract.
    let (l, artist, title) = unsafe {
        (
            listener::<L>(u),
            opt_string(artist, artist_len),
            opt_string(title, title_len),
        )
    };
    l.event(LibEvent::Metadata { artist, title });
}

unsafe extern "C" fn cb_visualizer_start<L: Listener>(u: User, types: usize, rate_max: u16) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::VisualizerStart { types, rate_max });
}

unsafe extern "C" fn cb_visualizer_end<L: Listener>(u: User) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.event(LibEvent::VisualizerEnd);
}

unsafe extern "C" fn cb_load_blob<L: Listener>(
    u: User,
    key: *const c_char,
    len: usize,
    out: *mut SsBlob,
) -> bool {
    // SAFETY: shim contract.
    let (l, key) = unsafe { (listener::<L>(u), slice(key.cast::<u8>(), len)) };
    let Some(blob) = l.load_blob(OsStr::from_bytes(key)) else {
        return false;
    };
    // SAFETY: `out` is the blob of this load_blob call; `blob` is readable for its length.
    unsafe { ss_blob_set(out, blob.as_ptr(), blob.len()) }
}

unsafe extern "C" fn cb_save_blob<L: Listener>(
    u: User,
    key: *const c_char,
    key_len: usize,
    data: *const u8,
    len: usize,
) -> bool {
    // SAFETY: shim contract.
    let (l, key, data) = unsafe {
        (
            listener::<L>(u),
            slice(key.cast::<u8>(), key_len),
            slice(data, len),
        )
    };
    l.save_blob(OsStr::from_bytes(key), data)
}

unsafe extern "C" fn cb_audio_write<L: Listener>(
    u: User,
    data: *const u8,
    len: usize,
    timeout_ms: u32,
    played: *mut u32,
    finish_us: *mut i64,
) -> usize {
    // SAFETY: shim contract: `data` holds `len` bytes for this call.
    let (l, data) = unsafe { (listener::<L>(u), slice(data, len)) };
    let (n, frames, finish) = l.audio_write(data, timeout_ms);
    // SAFETY: the shim passes pointers to its own locals for this call.
    unsafe {
        played.write(frames);
        finish_us.write(finish);
    }
    n
}

unsafe extern "C" fn cb_loudness<L: Listener>(u: User, v: u16) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.loudness(v);
}

unsafe extern "C" fn cb_beat<L: Listener>(u: User) {
    // SAFETY: shim contract.
    unsafe { listener::<L>(u) }.beat();
}

unsafe extern "C" fn cb_spectrum<L: Listener>(u: User, bins: *const u16, n: usize) {
    // SAFETY: shim contract: the library's bin vector for this call.
    let (l, bins) = unsafe { (listener::<L>(u), slice(bins, n)) };
    l.spectrum(bins);
}

fn callbacks<L: Listener>() -> SsCallbacks {
    SsCallbacks {
        group_update: cb_group::<L>,
        time_sync: cb_time_sync::<L>,
        pairing_started: cb_pairing_started::<L>,
        pairing_succeeded: cb_pairing_succeeded::<L>,
        pairing_failed: cb_pairing_failed::<L>,
        trust_changed: cb_trust::<L>,
        display_pairing_code: cb_pairing_code::<L>,
        clear_pairing_code: cb_pairing_code_cleared::<L>,
        open_pairing_window: cb_pairing_window_open::<L>,
        close_pairing_window: cb_pairing_window_close::<L>,
        stream_start: cb_stream_start::<L>,
        stream_end: cb_stream_end::<L>,
        volume_changed: cb_volume::<L>,
        mute_changed: cb_mute::<L>,
        output_delay_changed: cb_output_delay::<L>,
        controller_state: cb_controller_state::<L>,
        metadata: cb_metadata::<L>,
        visualizer_stream_start: cb_visualizer_start::<L>,
        visualizer_stream_end: cb_visualizer_end::<L>,
        load_blob: cb_load_blob::<L>,
        save_blob: cb_save_blob::<L>,
        audio_write: cb_audio_write::<L>,
        loudness: cb_loudness::<L>,
        beat: cb_beat::<L>,
        spectrum: cb_spectrum::<L>,
    }
}

/// `ss_put` target: `dst` is a `String`.
unsafe extern "C" fn put_string(dst: *mut c_void, s: *const c_char, len: usize) {
    // SAFETY: the callers below pass a `&mut String` that lives across the ss_* call, and the
    // shim passes a live string.
    unsafe { *dst.cast::<String>() = string(s, len) };
}

/// One sendspin-cpp client with all its roles. Not `Send`: every call (and the drop, which stops
/// the library and joins its threads) happens on the thread that created it, which is the
/// library's main loop thread. The listener is never called back into from here.
pub(crate) struct Client<L: Listener> {
    raw: NonNull<SsClient>,
    // Dropped after `ss_destroy` (see Drop): the library's threads use it until then.
    _listener: Arc<L>,
}

impl<L: Listener> Client<L> {
    /// None if the library could not build the client (allocation failure).
    pub(crate) fn new(cfg: &Config<'_>, listener: Arc<L>) -> Option<Self> {
        let formats: Vec<SsFormat> = cfg
            .formats
            .iter()
            .map(|f| SsFormat {
                sample_rate: f.rate,
                codec: f.codec as u8,
                channels: f.channels,
                bit_depth: f.bits,
            })
            .collect();
        let vis = &cfg.visualizer;
        let vis_types: Vec<u8> = vis.types.iter().map(|&t| t as u8).collect();
        let raw_cfg = SsConfig {
            name: SsStr::new(cfg.name),
            product_name: SsStr::new(cfg.product_name),
            manufacturer: SsStr::new(cfg.manufacturer),
            software_version: SsStr::new(cfg.software_version),
            mac: SsStr::new(cfg.mac),
            pairing_psk_location: SsStr::new(cfg.pairing_psk_location),
            formats: formats.as_ptr(),
            n_formats: formats.len(),
            vis_types: vis_types.as_ptr(),
            n_vis_types: vis_types.len(),
            vis_buffer_capacity: vis.buffer_capacity,
            vis_display_offset_ms: vis.display_offset_ms,
            port: cfg.port,
            vis_rate_max: vis.rate_max,
            vis_f_min: vis.f_min,
            vis_f_max: vis.f_max,
            vis_bins: vis.bins,
            vis_scale: vis.scale as u8,
            pairing_code_display_digits: cfg.pairing_code_display_digits,
            unpaired_access: cfg.unpaired_access,
        };
        let cb = callbacks::<L>();
        let user: User = Arc::as_ptr(&listener).cast_mut().cast();
        // SAFETY: `raw_cfg`, the strings and arrays it points to and `cb` outlive the call
        // (ss_create copies all of them); `user` points into `listener`, which the Client keeps
        // until after ss_destroy, and the trampolines only take shared references to it
        // (`L: Sync`, as the library calls them from three threads).
        let raw = unsafe { ss_create(&raw_cfg, &cb, user) };
        NonNull::new(raw).map(|raw| Self {
            raw,
            _listener: listener,
        })
    }

    /// SendspinClient::start(); None: it threw.
    pub(crate) fn start(&self) -> Option<bool> {
        // SAFETY: `raw` is live until drop; main loop thread (Client is not Send).
        match unsafe { ss_start(self.raw.as_ptr()) } {
            r if r < 0 => None,
            r => Some(r > 0),
        }
    }

    /// SendspinClient::loop(): delivers the main loop callbacks to the listener. false: it
    /// threw.
    pub(crate) fn run_loop(&self) -> bool {
        // SAFETY: as `start`.
        unsafe { ss_loop(self.raw.as_ptr()) }
    }

    pub(crate) fn is_connected(&self) -> bool {
        // SAFETY: as `start`.
        unsafe { ss_is_connected(self.raw.as_ptr()) }
    }

    /// (name, server_id) of the active connection.
    pub(crate) fn server_info(&self) -> Option<(String, String)> {
        let (mut name, mut id) = (String::new(), String::new());
        // SAFETY: as `start`; `put_string` gets the two Strings, alive across the call.
        let ok = unsafe {
            ss_server_info(
                self.raw.as_ptr(),
                put_string,
                (&raw mut name).cast(),
                (&raw mut id).cast(),
            )
        };
        ok.then_some((name, id))
    }

    pub(crate) fn client_id(&self) -> String {
        let mut id = String::new();
        // SAFETY: as `server_info`.
        unsafe { ss_client_id(self.raw.as_ptr(), put_string, (&raw mut id).cast()) };
        id
    }

    pub(crate) fn pairing_token(&self) -> Option<String> {
        let mut tok = String::new();
        // SAFETY: as `server_info`.
        let ok = unsafe { ss_pairing_token(self.raw.as_ptr(), put_string, (&raw mut tok).cast()) };
        ok.then_some(tok)
    }

    pub(crate) fn set_output_delay_adjustable(&self, adjustable: bool) {
        // SAFETY: as `start`.
        unsafe { ss_player_set_output_delay_adjustable(self.raw.as_ptr(), adjustable) }
    }

    pub(crate) fn volume(&self) -> u8 {
        // SAFETY: as `start`.
        unsafe { ss_player_volume(self.raw.as_ptr()) }
    }

    pub(crate) fn muted(&self) -> bool {
        // SAFETY: as `start`.
        unsafe { ss_player_muted(self.raw.as_ptr()) }
    }

    pub(crate) fn update_volume(&self, volume: u8) {
        // SAFETY: as `start`.
        unsafe { ss_player_update_volume(self.raw.as_ptr(), volume) }
    }

    pub(crate) fn update_muted(&self, muted: bool) {
        // SAFETY: as `start`.
        unsafe { ss_player_update_muted(self.raw.as_ptr(), muted) }
    }

    /// false: not offered by the server (or not queued).
    pub(crate) fn send_command(&self, c: Command) -> bool {
        // SAFETY: as `start`.
        unsafe { ss_controller_send(self.raw.as_ptr(), c as u8) }
    }

    /// client/leave.
    pub(crate) fn leave(&self) {
        // SAFETY: as `start`.
        unsafe { ss_leave(self.raw.as_ptr()) }
    }

    pub(crate) fn confirm_pairing_window(&self) {
        // SAFETY: as `start`.
        unsafe { ss_confirm_pairing_window(self.raw.as_ptr()) }
    }
}

impl<L: Listener> Drop for Client<L> {
    fn drop(&mut self) {
        // SAFETY: `raw` came from ss_create and is freed only here; this stops the library
        // (its last callbacks run inside this call) before the listener is released.
        unsafe { ss_destroy(self.raw.as_ptr()) }
    }
}
