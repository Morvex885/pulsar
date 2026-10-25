//! The firmware's GLib and GStreamer (/system/vendor/quasar/libs/libquasar_gstreamer.so), loaded
//! with dlopen and used without headers. Every unsafe call of the player lives here.
//!
//! Library loading: libquasar_gstreamer.so has RUNPATH=/system/vendor/quasar/libs, but
//! libglib.so has none and NEEDs libiconv.so (which lives only in that directory). libiconv.so,
//! libglib.so and libquasar_gstreamer.so are opened by absolute path, in that order, with
//! RTLD_GLOBAL; the already-loaded "libiconv.so" SONAME then satisfies glib's dependency, so
//! LD_LIBRARY_PATH is not needed.
//!
//! Struct layouts used without headers (GStreamer 1.x public structs):
//!   GstMessage: GstMiniObject | GstMessageType type | guint64 timestamp | GstObject *src;
//!               type/src at 64/80 on aarch64, 36/48 on armhf (GstMiniObject is 36 bytes there;
//!               the ARM EABI 8-aligns the timestamp).
//!   GError:     GQuark domain @0 | gint code @4 | gchar *message @8 (same on both ABIs).

use std::error::Error as _;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::os::unix::ffi::OsStrExt as _;
use std::ptr::{self, NonNull};
use std::sync::LazyLock;

use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};
use log::{error, info, warn};

use crate::player::{ErrorInfo, Msg};

type Gp = *mut c_void;
type GSourceFunc = unsafe extern "C" fn(Gp) -> c_int;
type GstBusFunc = unsafe extern "C" fn(Gp, Gp, Gp) -> c_int;
type ParseFn = unsafe extern "C" fn(Gp, *mut Gp, *mut *mut c_char);

/// A command for the GLib thread (see [`idle_add`]).
pub(crate) type Call = Box<dyn FnOnce() + Send>;

const LIB_DIR: &str = "/system/vendor/quasar/libs/";
const CA_BUNDLE: &CStr = c"/etc/ssl/certs/ca-certificates.crt";

// GstState NULL (the other values: player::GstState), GstStateChangeReturn FAILURE
const STATE_NULL: c_int = 1;
const STATE_CHANGE_FAILURE: c_int = 0;

// GstMessageType
const MSG_EOS: u32 = 1 << 0;
const MSG_ERROR: u32 = 1 << 1;
const MSG_WARNING: u32 = 1 << 2;
const MSG_BUFFERING: u32 = 1 << 5;
const MSG_STATE_CHANGED: u32 = 1 << 6;
const MSG_CLOCK_LOST: u32 = 1 << 10;

const FORMAT_TIME: c_int = 3;
// GST_SEEK_FLAG_FLUSH | GST_SEEK_FLAG_KEY_UNIT
const SEEK_FLAG_FLUSH_KEY_UNIT: c_int = (1 << 0) | (1 << 2);

// Static plugins compiled into libquasar_gstreamer.so that playbin needs for our formats.
const PLUGINS: [&str; 24] = [
    "coreelements",
    "playback",
    "typefindfunctions",
    "audioconvert",
    "audioresample",
    "audiorate",
    "volume",
    "alsa_nocapturelog",
    "wavparse",
    "ogg",
    "opus",
    "opusparse",
    "flac",
    "audioparsers",
    "libav",
    "soup",
    "isomp4",
    "id3demux",
    "app",
    "gio",
    "autodetect",
    "pbtypes",
    "hls",
    "mpegtsdemux",
];

#[allow(
    dead_code,
    reason = "layout mirror of GstMiniObject: only its size and alignment matter"
)]
#[repr(C)]
struct MiniObjectView {
    type_: usize, // GType (gsize)
    refcount: c_int,
    lockstate: c_int,
    flags: c_uint,
    copy: Gp,
    dispose: Gp,
    free: Gp,
    n_qdata: c_uint,
    qdata: Gp,
}

#[allow(
    dead_code,
    reason = "layout mirror of GstMessage: only type_ and src are read"
)]
#[repr(C)]
struct MessageView {
    mini_object: MiniObjectView,
    type_: u32, // GstMessageType
    timestamp: u64,
    src: Gp, // GstObject *
}

#[cfg(target_arch = "aarch64")]
const _: () = assert!(
    core::mem::offset_of!(MessageView, type_) == 64
        && core::mem::offset_of!(MessageView, src) == 80
);
#[cfg(target_arch = "arm")]
const _: () = assert!(
    core::mem::offset_of!(MessageView, type_) == 36
        && core::mem::offset_of!(MessageView, src) == 48
);

#[allow(dead_code, reason = "only read through GError pointers")]
#[repr(C)]
struct GErrorView {
    domain: u32, // GQuark
    code: c_int,
    message: *const c_char,
}

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
const _: () = assert!(
    core::mem::offset_of!(GErrorView, domain) == 0
        && core::mem::offset_of!(GErrorView, code) == 4
        && core::mem::offset_of!(GErrorView, message) == 8
);

/// Every symbol the player uses, resolved once.
pub(crate) struct Api {
    _iconv: Option<Library>,
    _glib: Library,
    gst: Library,
    // glib
    main_loop_new: unsafe extern "C" fn(Gp, c_int) -> Gp,
    main_loop_run: unsafe extern "C" fn(Gp),
    main_loop_quit: unsafe extern "C" fn(Gp),
    main_loop_unref: unsafe extern "C" fn(Gp),
    idle_add: unsafe extern "C" fn(GSourceFunc, Gp) -> c_uint,
    timeout_add: unsafe extern "C" fn(c_uint, GSourceFunc, Gp) -> c_uint,
    source_remove: unsafe extern "C" fn(c_uint) -> c_int,
    object_set: unsafe extern "C" fn(Gp, *const c_char, ...),
    error_free: unsafe extern "C" fn(Gp),
    free: unsafe extern "C" fn(Gp),
    // Optional, like the two below: retry / resume only.
    quark_try_string: Option<unsafe extern "C" fn(*const c_char) -> u32>,
    // gstreamer
    init_check: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char, *mut Gp) -> c_int,
    parse_launch: unsafe extern "C" fn(*const c_char, *mut Gp) -> Gp,
    element_set_state: unsafe extern "C" fn(Gp, c_int) -> c_int,
    pipeline_get_bus: unsafe extern "C" fn(Gp) -> Gp,
    bus_add_watch: unsafe extern "C" fn(Gp, GstBusFunc, Gp) -> c_uint,
    message_parse_error: ParseFn,
    message_parse_warning: ParseFn,
    message_parse_buffering: unsafe extern "C" fn(Gp, *mut c_int),
    message_parse_state_changed: unsafe extern "C" fn(Gp, *mut c_int, *mut c_int, *mut c_int),
    object_unref: unsafe extern "C" fn(Gp),
    element_query_position: Option<unsafe extern "C" fn(Gp, c_int, *mut i64) -> c_int>,
    element_seek_simple: Option<unsafe extern "C" fn(Gp, c_int, c_int, i64) -> c_int>,
}

fn dl_error(e: libloading::Error) -> String {
    // The source carries dlerror()'s text ("<file>: cannot open shared object file: ...").
    match e.source() {
        Some(src) => src.to_string(),
        None => e.to_string(),
    }
}

fn open(name: &str) -> Result<Library, String> {
    // SAFETY: the firmware's own libraries; their initialisers have no preconditions and they
    // are never unloaded (the handles live in the static Api).
    unsafe { Library::open(Some(format!("{LIB_DIR}{name}")), RTLD_NOW | RTLD_GLOBAL) }
        .map_err(dl_error)
}

/// `name` as a function pointer of type `T`, None if the library does not export it (or
/// exports it as NULL).
///
/// # Safety
/// `T` must be the function pointer type matching the C prototype of `name`.
unsafe fn opt<T: Copy>(lib: &Library, name: &CStr) -> Option<T> {
    const { assert!(size_of::<T>() == size_of::<*mut c_void>()) };
    // SAFETY: the symbol's address only; the pointer is only used while `lib` stays loaded.
    let p = unsafe { lib.get::<*mut c_void>(name) }.ok().map(|s| *s)?;
    if p.is_null() {
        return None;
    }
    // SAFETY: a non-null code address of a function whose prototype is T (caller guarantee);
    // the sizes match (asserted above).
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

/// As [`opt`], logging a missing symbol and clearing `ok`.
///
/// # Safety
/// As [`opt`].
unsafe fn req<T: Copy>(lib: &Library, name: &CStr, ok: &mut bool) -> Option<T> {
    // SAFETY: forwarded caller guarantee.
    let s = unsafe { opt::<T>(lib, name) };
    if s.is_none() {
        error!("player: missing symbol {}", name.to_string_lossy());
        *ok = false;
    }
    s
}

static API: LazyLock<Option<Api>> = LazyLock::new(load_api);

/// dlopen + symbol resolution, once per process; None (logged) on failure.
pub(crate) fn load() -> Option<&'static Api> {
    API.as_ref()
}

fn load_api() -> Option<Api> {
    // Preload libiconv so libglib.so's DT_NEEDED "libiconv.so" resolves without LD_LIBRARY_PATH.
    // A failed load keeps the libraries already opened (never dlclose: their initialisers may
    // have registered state, e.g. GLib types, that outlives a close).
    let iconv = open("libiconv.so")
        .inspect_err(|e| warn!("player: dlopen libiconv: {e}"))
        .ok();
    let glib = match open("libglib.so") {
        Ok(l) => l,
        Err(e) => {
            error!("player: dlopen libglib.so: {e}");
            std::mem::forget(iconv);
            return None;
        }
    };
    let gst = match open("libquasar_gstreamer.so") {
        Ok(l) => l,
        Err(e) => {
            error!("player: dlopen libquasar_gstreamer.so: {e}");
            std::mem::forget((iconv, glib));
            return None;
        }
    };
    let mut ok = true;
    // SAFETY: every type below is the prototype from the GLib 2.x / GStreamer 1.x headers, with
    // gboolean, enums and GstState as int, guint as unsigned int, gint64 as i64, GQuark as u32
    // and every object pointer as void *.
    unsafe {
        let main_loop_new = req(&glib, c"g_main_loop_new", &mut ok);
        let main_loop_run = req(&glib, c"g_main_loop_run", &mut ok);
        let main_loop_quit = req(&glib, c"g_main_loop_quit", &mut ok);
        let main_loop_unref = req(&glib, c"g_main_loop_unref", &mut ok);
        let idle_add = req(&glib, c"g_idle_add", &mut ok);
        let timeout_add = req(&glib, c"g_timeout_add", &mut ok);
        let source_remove = req(&glib, c"g_source_remove", &mut ok);
        let object_set = req(&glib, c"g_object_set", &mut ok);
        let error_free = req(&glib, c"g_error_free", &mut ok);
        let free = req(&glib, c"g_free", &mut ok);
        let quark_try_string = opt(&glib, c"g_quark_try_string");
        let element_query_position = opt(&gst, c"gst_element_query_position");
        let element_seek_simple = opt(&gst, c"gst_element_seek_simple");
        let init_check = req(&gst, c"gst_init_check", &mut ok);
        let parse_launch = req(&gst, c"gst_parse_launch", &mut ok);
        let element_set_state = req(&gst, c"gst_element_set_state", &mut ok);
        let pipeline_get_bus = req(&gst, c"gst_pipeline_get_bus", &mut ok);
        let bus_add_watch = req(&gst, c"gst_bus_add_watch", &mut ok);
        let message_parse_error = req(&gst, c"gst_message_parse_error", &mut ok);
        let message_parse_warning = req(&gst, c"gst_message_parse_warning", &mut ok);
        let message_parse_buffering = req(&gst, c"gst_message_parse_buffering", &mut ok);
        let message_parse_state_changed = req(&gst, c"gst_message_parse_state_changed", &mut ok);
        let object_unref = req(&gst, c"gst_object_unref", &mut ok);
        if !ok {
            std::mem::forget((iconv, glib, gst));
            return None;
        }
        Some(Api {
            main_loop_new: main_loop_new?,
            main_loop_run: main_loop_run?,
            main_loop_quit: main_loop_quit?,
            main_loop_unref: main_loop_unref?,
            idle_add: idle_add?,
            timeout_add: timeout_add?,
            source_remove: source_remove?,
            object_set: object_set?,
            error_free: error_free?,
            free: free?,
            quark_try_string,
            init_check: init_check?,
            parse_launch: parse_launch?,
            element_set_state: element_set_state?,
            pipeline_get_bus: pipeline_get_bus?,
            bus_add_watch: bus_add_watch?,
            message_parse_error: message_parse_error?,
            message_parse_warning: message_parse_warning?,
            message_parse_buffering: message_parse_buffering?,
            message_parse_state_changed: message_parse_state_changed?,
            object_unref: object_unref?,
            element_query_position,
            element_seek_simple,
            _iconv: iconv,
            _glib: glib,
            gst,
        })
    }
}

/// Points OpenSSL (souphttpsrc's TLS backend, below) at the system CA bundle unless the
/// environment already names one.
pub(crate) fn set_ssl_cert_file() {
    // SAFETY: NUL-terminated path.
    if unsafe { libc::access(CA_BUNDLE.as_ptr(), libc::R_OK) } != 0
        || std::env::var_os("SSL_CERT_FILE").is_some()
    {
        return;
    }
    // SAFETY: std's environment lock covers Rust readers. C code reading the environment
    // (getenv) on another thread at this moment would race; the daemon calls
    // pulsar_media::spawn() before it starts any other service thread, so none exists yet.
    unsafe {
        std::env::set_var(
            "SSL_CERT_FILE",
            std::ffi::OsStr::from_bytes(CA_BUNDLE.to_bytes()),
        )
    };
}

/// The text of a GError (or "?" without one).
///
/// # Safety
/// `err` is null or a live GError.
unsafe fn gerror_message(err: Gp) -> String {
    if err.is_null() {
        return "?".to_owned();
    }
    // SAFETY: live GError, layout asserted above.
    let msg = unsafe { (*err.cast::<GErrorView>()).message };
    if msg.is_null() {
        return "(null)".to_owned();
    }
    // SAFETY: GError messages are NUL-terminated.
    unsafe { CStr::from_ptr(msg) }
        .to_string_lossy()
        .into_owned()
}

/// Frees a GError the caller received (null allowed).
fn free_error(api: &Api, err: Gp) {
    if !err.is_null() {
        // SAFETY: a GError owned by the caller, freed once.
        unsafe { (api.error_free)(err) };
    }
}

/// gst_init_check, the static plugins and the TLS backend; false (logged) on failure.
pub(crate) fn init(api: &Api) -> bool {
    let mut err: Gp = ptr::null_mut();
    // SAFETY: no argv; out-pointer to a local.
    if unsafe { (api.init_check)(ptr::null_mut(), ptr::null_mut(), &mut err) } == 0 {
        // SAFETY: null or the GError gst_init_check set.
        error!("player: gst_init_check failed: {}", unsafe {
            gerror_message(err)
        });
        free_error(api, err);
        return false;
    }
    let mut regs = 0;
    for p in PLUGINS {
        let Ok(name) = CString::new(format!("gst_plugin_{p}_register")) else {
            continue;
        };
        // SAFETY: GST_PLUGIN_DEFINE's static registration entry point, void (void).
        match unsafe { opt::<unsafe extern "C" fn()>(&api.gst, &name) } {
            Some(register) => {
                // SAFETY: called after gst_init, as gst_init_static_plugins does.
                unsafe { register() };
                regs += 1;
            }
            None => warn!("player: plugin {p} not present"),
        }
    }
    // TLS backend for souphttpsrc (https://): glib-networking's OpenSSL module is linked
    // statically into libquasar_gstreamer.so; loading it with a NULL module registers it as a
    // static type. Its default CA store is OpenSSL's compiled-in path, which does not exist on
    // the device ("Unacceptable TLS certificate"); SSL_CERT_FILE (set_ssl_cert_file) points it at
    // the system bundle.
    // SAFETY: void g_io_openssl_load (GIOModule *module); NULL = static registration.
    if let Some(tls) = unsafe { opt::<unsafe extern "C" fn(Gp)>(&api.gst, c"g_io_openssl_load") } {
        // SAFETY: see above.
        unsafe { tls(ptr::null_mut()) };
    }
    info!("player: gstreamer initialised, {regs} plugins registered");
    true
}

/// A GLib main loop on the default main context.
pub(crate) struct MainLoop {
    api: &'static Api,
    ml: NonNull<c_void>,
}

// SAFETY: GMainLoop is reference counted with atomic operations; g_main_loop_quit may be called
// from any thread, g_main_loop_run runs on one thread (the player's).
unsafe impl Send for MainLoop {}
// SAFETY: see Send.
unsafe impl Sync for MainLoop {}

impl MainLoop {
    pub(crate) fn new(api: &'static Api) -> Option<MainLoop> {
        // SAFETY: NULL = the default main context, not running.
        let ml = unsafe { (api.main_loop_new)(ptr::null_mut(), 0) };
        NonNull::new(ml).map(|ml| MainLoop { api, ml })
    }

    pub(crate) fn run(&self) {
        // SAFETY: live loop.
        unsafe { (self.api.main_loop_run)(self.ml.as_ptr()) };
    }

    pub(crate) fn quit(&self) {
        // SAFETY: live loop.
        unsafe { (self.api.main_loop_quit)(self.ml.as_ptr()) };
    }
}

impl Drop for MainLoop {
    fn drop(&mut self) {
        // SAFETY: the reference g_main_loop_new returned, dropped once.
        unsafe { (self.api.main_loop_unref)(self.ml.as_ptr()) };
    }
}

unsafe extern "C" fn run_call(data: Gp) -> c_int {
    // SAFETY: `data` is the Box<Call> leaked by idle_add; this source returns G_SOURCE_REMOVE,
    // so GLib calls it exactly once.
    let call = unsafe { Box::from_raw(data.cast::<Call>()) };
    call();
    0 // G_SOURCE_REMOVE
}

/// Runs `call` on the thread of the default main context (the player's GLib thread).
pub(crate) fn idle_add(api: &Api, call: Call) {
    let data = Box::into_raw(Box::new(call));
    // SAFETY: run_call matches GSourceFunc and takes ownership of `data`.
    unsafe { (api.idle_add)(run_call, data.cast()) };
}

unsafe extern "C" fn on_timeout(user: Gp) -> c_int {
    c_int::from(crate::player::on_timer(user.addr()))
}

/// A GLib timeout calling `player::on_timer(tag)` every `ms` while it returns true.
pub(crate) fn timeout_add(api: &Api, ms: u32, tag: usize) -> u32 {
    // SAFETY: on_timeout matches GSourceFunc; the tag is a plain integer, never dereferenced.
    unsafe { (api.timeout_add)(ms, on_timeout, ptr::without_provenance_mut(tag)) }
}

pub(crate) fn source_remove(api: &Api, id: u32) {
    // SAFETY: removing an id GLib no longer knows only logs a critical warning.
    unsafe { (api.source_remove)(id) };
}

/// The resource-error domain of souphttpsrc's errors; 0 when unknown.
pub(crate) fn resource_error_quark(api: &Api) -> u32 {
    // SAFETY: NUL-terminated string; try_string never creates a quark.
    api.quark_try_string
        .map_or(0, |f| unsafe { f(c"gst-resource-error-quark".as_ptr()) })
}

/// A playbin element.
pub(crate) struct Pipeline {
    api: &'static Api,
    el: NonNull<c_void>,
    /// GLib source id of the bus watch (0 = none).
    watch: c_uint,
}

// SAFETY: GstElement is a thread-safe GObject; the player uses it from one thread at a time
// (created on the caller's, then only on the GLib thread).
unsafe impl Send for Pipeline {}

impl Pipeline {
    /// gst_parse_launch; None (logged) on failure.
    pub(crate) fn launch(api: &'static Api, desc: &CStr) -> Option<Pipeline> {
        let mut err: Gp = ptr::null_mut();
        // SAFETY: NUL-terminated description, out-pointer to a local.
        let el = unsafe { (api.parse_launch)(desc.as_ptr(), &mut err) };
        let Some(el) = NonNull::new(el) else {
            // SAFETY: null or the GError gst_parse_launch set.
            error!("player: cannot create playbin: {}", unsafe {
                gerror_message(err)
            });
            free_error(api, err);
            return None;
        };
        if !err.is_null() {
            // SAFETY: the GError gst_parse_launch set for a recoverable problem.
            warn!("player: playbin: {}", unsafe { gerror_message(err) });
            free_error(api, err);
        }
        Some(Pipeline { api, el, watch: 0 })
    }

    /// The element's address, for comparing with a message source.
    pub(crate) fn addr(&self) -> *const c_void {
        self.el.as_ptr()
    }

    /// gst_element_set_state; false on GST_STATE_CHANGE_FAILURE.
    pub(crate) fn set_state(&self, state: c_int) -> bool {
        // SAFETY: live element, GstState value.
        unsafe { (self.api.element_set_state)(self.el.as_ptr(), state) != STATE_CHANGE_FAILURE }
    }

    // One setter per property, name and GType fixed here: g_object_set reads the variadic value
    // as the property's type, which no compiler checks.

    /// playbin "uri" (gchararray).
    pub(crate) fn set_uri(&self, uri: &CStr) {
        let (el, null) = (self.el.as_ptr(), ptr::null::<c_char>());
        // SAFETY: "uri" is a string property; NULL-terminated argument list.
        unsafe { (self.api.object_set)(el, c"uri".as_ptr(), uri.as_ptr(), null) };
    }

    /// playbin "volume" (gdouble).
    pub(crate) fn set_volume(&self, v: f64) {
        let (el, null) = (self.el.as_ptr(), ptr::null::<c_char>());
        // SAFETY: "volume" is a gdouble property; NULL-terminated argument list.
        unsafe { (self.api.object_set)(el, c"volume".as_ptr(), v, null) };
    }

    /// playbin "flags" (GstPlayFlags, a guint flags type).
    pub(crate) fn set_flags(&self, v: u32) {
        let (el, null) = (self.el.as_ptr(), ptr::null::<c_char>());
        // SAFETY: "flags" is a flags (guint) property; NULL-terminated argument list.
        unsafe { (self.api.object_set)(el, c"flags".as_ptr(), v, null) };
    }

    /// playbin "buffer-duration" (gint64, ns).
    pub(crate) fn set_buffer_duration(&self, ns: i64) {
        let (el, null) = (self.el.as_ptr(), ptr::null::<c_char>());
        // SAFETY: "buffer-duration" is a gint64 property; NULL-terminated argument list.
        unsafe { (self.api.object_set)(el, c"buffer-duration".as_ptr(), ns, null) };
    }

    /// Position in ns; None without gst_element_query_position or when the query fails.
    pub(crate) fn query_position(&self) -> Option<i64> {
        let f = self.api.element_query_position?;
        let mut pos = 0i64;
        // SAFETY: live element, out-pointer to a local.
        (unsafe { f(self.el.as_ptr(), FORMAT_TIME, &mut pos) } != 0).then_some(pos)
    }

    pub(crate) fn can_seek(&self) -> bool {
        self.api.element_seek_simple.is_some()
    }

    /// Flushing key-unit seek to `pos` ns; false when refused (or without the symbol).
    pub(crate) fn seek(&self, pos: i64) -> bool {
        self.api.element_seek_simple.is_some_and(|f| {
            // SAFETY: live element, GstFormat / GstSeekFlags values.
            unsafe { f(self.el.as_ptr(), FORMAT_TIME, SEEK_FLAG_FLUSH_KEY_UNIT, pos) != 0 }
        })
    }

    /// Bus watch on the default main context calling `player::on_bus(tag, …)`; removed on drop.
    pub(crate) fn add_bus_watch(&mut self, tag: usize) {
        // SAFETY: live pipeline; the bus reference it returns is dropped after the watch took
        // its own. The tag is a plain integer, never dereferenced.
        unsafe {
            let bus = (self.api.pipeline_get_bus)(self.el.as_ptr());
            if bus.is_null() {
                return;
            }
            self.watch = (self.api.bus_add_watch)(bus, on_bus, ptr::without_provenance_mut(tag));
            (self.api.object_unref)(bus);
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if self.watch != 0 {
            source_remove(self.api, self.watch);
        }
        self.set_state(STATE_NULL);
        // SAFETY: the reference gst_parse_launch returned, dropped once.
        unsafe { (self.api.object_unref)(self.el.as_ptr()) };
    }
}

/// A bus message during its watch callback.
pub(crate) struct Message {
    api: &'static Api,
    msg: NonNull<c_void>,
}

unsafe extern "C" fn on_bus(_bus: Gp, msg: Gp, user: Gp) -> c_int {
    if let (Some(api), Some(msg)) = (load(), NonNull::new(msg)) {
        crate::player::on_bus(user.addr(), &Message { api, msg });
    }
    1 // keep the watch
}

impl Message {
    /// The message as the player sees it; state changes of elements other than `own` (the
    /// pipeline) are `Msg::Other`.
    pub(crate) fn parse(&self, own: *const c_void) -> Msg {
        let view = self.msg.as_ptr().cast::<MessageView>();
        // SAFETY: a live GstMessage during the bus callback; layout asserted above.
        let kind = unsafe { (*view).type_ };
        match kind {
            MSG_EOS => Msg::Eos,
            MSG_ERROR => Msg::Error(self.error(self.api.message_parse_error)),
            MSG_WARNING => Msg::Warning(self.error(self.api.message_parse_warning).message),
            MSG_BUFFERING => {
                let mut pct: c_int = 100;
                // SAFETY: a buffering message, out-pointer to a local.
                unsafe { (self.api.message_parse_buffering)(self.msg.as_ptr(), &mut pct) };
                Msg::Buffering(pct)
            }
            MSG_STATE_CHANGED => {
                // SAFETY: as above.
                let src = unsafe { (*view).src };
                if !ptr::eq(src.cast_const(), own) {
                    return Msg::Other;
                }
                let (mut old, mut new, mut pending) = (0, 0, 0);
                // SAFETY: a state-changed message, out-pointers to locals.
                unsafe {
                    (self.api.message_parse_state_changed)(
                        self.msg.as_ptr(),
                        &mut old,
                        &mut new,
                        &mut pending,
                    )
                };
                Msg::StateChanged { new }
            }
            MSG_CLOCK_LOST => Msg::ClockLost,
            _ => Msg::Other,
        }
    }

    /// gst_message_parse_error / _warning; the GError and debug text are copied and freed.
    fn error(&self, parse: ParseFn) -> ErrorInfo {
        let (mut err, mut dbg): (Gp, *mut c_char) = (ptr::null_mut(), ptr::null_mut());
        // SAFETY: an error / warning message (checked by the caller), out-pointers to locals;
        // both results are owned by us afterwards.
        unsafe { parse(self.msg.as_ptr(), &mut err, &mut dbg) };
        // SAFETY: null or the GError just returned.
        let message = unsafe { gerror_message(err) };
        let domain_code = (!err.is_null()).then(|| {
            // SAFETY: live GError, layout asserted above.
            let e = unsafe { &*err.cast::<GErrorView>() };
            (e.domain, e.code)
        });
        let debug = (!dbg.is_null()).then(|| {
            // SAFETY: NUL-terminated debug text.
            unsafe { CStr::from_ptr(dbg) }
                .to_string_lossy()
                .into_owned()
        });
        free_error(self.api, err);
        if !dbg.is_null() {
            // SAFETY: the debug string just returned, freed once.
            unsafe { (self.api.free)(dbg.cast()) };
        }
        ErrorInfo {
            message,
            debug,
            domain_code,
        }
    }
}
