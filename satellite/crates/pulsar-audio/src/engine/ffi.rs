//! The firmware's VQE and spotter libraries through dlopen: every symbol is resolved once into
//! [`Lib`]; the wrappers below are the only callers of those pointers.
//!
//! Two generations of the libraries exist ([`Abi`]); the wrappers hide the difference:
//! - [`Abi::Evil`] (Midi, Lite 2, Mini 2): one libyandex_evil.so (+ libtflite.so);
//! - [`Abi::Split`] (Mini 3, firmware 3.21): libyandex_vqe.so + libyandex_spotter.so, an older API.
//!   `YandexVqeC_Create` has no models directory argument (the models are built in), most spotter
//!   calls have no error argument at all (an exception inside the library ends the process with
//!   `quick_exit(1)`), and `spotter_conf_read` / `spotter_create_multichannel` report a failure
//!   (null result) through a `const char*` message written to their error cell instead of a
//!   `spotter_error*`.
//!
//! Signatures recovered by reverse engineering maind, verified on the device:
//! - VQE functions return a `YandexVqeC_Error*` (null on success);
//! - spotter functions ([`Abi::Evil`]) take a trailing `spotter_error**`, set on failure;
//! - VQE I/O is float in [-1, 1], frame-interleaved; Process consumes GetInputChunkSize() frames
//!   (Midi: 3072 @ 96 kHz, Lite 2: 1536 @ 48 kHz) and yields GetProcessingFrame() samples
//!   (512 @ 16 kHz) per output channel; output channel types follow
//!   voicetech.AudioInputChannelType;
//! - spotter input is s16le interleaved in the channel order given to
//!   spotter_create_multichannel.

use std::error::Error as _;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_void};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::LazyLock;

use super::SPOTTER_TYPES;

use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};

// Highest voicetech.AudioInputChannelType value passed to GetOutputChannelData.
const MAX_CHANNEL_TYPE: i32 = 16;

const ICONV: &str = "/system/vendor/quasar/libs/libiconv.so";
const TFLITE: &str = "/system/vendor/quasar/libs/libtflite.so";
const EVIL: &str = "/system/vendor/quasar/libs/libyandex_evil.so";
const SPLIT_VQE: &str = "/system/vendor/quasar/libs/libyandex_vqe.so";
const SPLIT_SPOTTER: &str = "/system/vendor/quasar/libs/libyandex_spotter.so";

type P = *mut c_void;

/// Which generation of the firmware's libraries the station has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    /// libyandex_evil.so: VQE and spotters in one library.
    Evil,
    /// libyandex_vqe.so + libyandex_spotter.so.
    Split,
}

/// The generation on this station: libyandex_evil.so when it exists, the split pair when only
/// that is there. Neither: [`Abi::Evil`], so the load error names the usual library.
fn detect_abi() -> Abi {
    if !Path::new(EVIL).exists() && Path::new(SPLIT_VQE).exists() {
        Abi::Split
    } else {
        Abi::Evil
    }
}

/// `YandexVqeC_Create`: the models directory argument exists only in [`Abi::Evil`].
#[derive(Clone, Copy)]
enum VqeCreate {
    Models(unsafe extern "C" fn(P, *const c_char, usize, *const c_void, *mut P) -> P),
    Builtin(unsafe extern "C" fn(P, usize, *const c_void, *mut P) -> P),
}

/// Spotter calls of [`Abi::Evil`]: a trailing `spotter_error**`.
#[derive(Clone, Copy)]
struct EvilSpotter {
    conf_read: unsafe extern "C" fn(*const c_char, *mut P) -> P,
    conf_free: unsafe extern "C" fn(P, *mut P),
    conf_rate: unsafe extern "C" fn(P, *mut P) -> c_int,
    conf_chans: unsafe extern "C" fn(P, c_long, *mut P) -> c_long,
    create: unsafe extern "C" fn(P, c_int, c_int, *const c_int, bool, *mut P) -> P,
    destroy: unsafe extern "C" fn(P, *mut P),
    submit: unsafe extern "C" fn(P, *const c_void, usize, *mut P),
    next: unsafe extern "C" fn(P, *mut P) -> c_int,
    phrase: unsafe extern "C" fn(P, *mut P) -> *const c_char,
    errmsg: unsafe extern "C" fn(P) -> *const c_char,
    errfree: unsafe extern "C" fn(P),
}

/// Spotter calls of [`Abi::Split`]: only `conf_read` and `create` have an error cell (a
/// `const char*` message on failure, null otherwise); the other calls have none. `create`'s two
/// trailing arguments are zero in maind.
#[derive(Clone, Copy)]
struct SplitSpotter {
    conf_read: unsafe extern "C" fn(*const c_char, *mut P) -> P,
    conf_free: unsafe extern "C" fn(P),
    conf_rate: unsafe extern "C" fn(P) -> c_int,
    conf_chans: unsafe extern "C" fn(P, c_long) -> c_long,
    create: unsafe extern "C" fn(P, c_int, c_int, *const c_int, *mut P, usize, usize) -> P,
    destroy: unsafe extern "C" fn(P),
    submit: unsafe extern "C" fn(P, *const c_void, usize),
    next: unsafe extern "C" fn(P) -> c_int,
    phrase: unsafe extern "C" fn(P) -> *const c_char,
}

#[derive(Clone, Copy)]
enum Spotters {
    Evil(EvilSpotter),
    Split(SplitSpotter),
}

/// The library's C API as function pointers; the library is never unloaded, so they stay valid
/// for the life of the process.
#[derive(Clone, Copy)]
pub struct Lib {
    cfg_create: unsafe extern "C" fn(*const c_char, *mut P) -> P,
    cfg_free: unsafe extern "C" fn(P),
    cfg_chunk: unsafe extern "C" fn(P) -> c_int,
    cfg_frame: unsafe extern "C" fn(P) -> c_int,
    vqe_create: VqeCreate,
    vqe_free: unsafe extern "C" fn(P),
    vqe_process: unsafe extern "C" fn(P, *const f32, usize, *const f32, usize) -> P,
    vqe_out: unsafe extern "C" fn(P, c_int, usize, *mut *const f32, *mut usize) -> P,
    vqe_errmsg: unsafe extern "C" fn(P) -> *const c_char,
    vqe_errfree: unsafe extern "C" fn(P),
    sp: Spotters,
    // Only ww_offline prints event names; the daemon does not need the symbol.
    event_name: Option<unsafe extern "C" fn(c_int) -> *const c_char>,
}

/// Why [`Lib::load`] failed.
#[derive(Clone, Debug)]
pub enum LoadError {
    /// dlerror() text.
    Open(String),
    /// The first required symbol the library does not export.
    Missing(&'static str),
}

fn dl_error(e: libloading::Error) -> String {
    // The source carries dlerror()'s text ("<file>: cannot open shared object file: ...").
    match e.source() {
        Some(src) => src.to_string(),
        None => e.to_string(),
    }
}

#[allow(
    clippy::useless_conversion,
    reason = "long is 32-bit on armhf, 64-bit on aarch64"
)]
fn from_long(v: c_long) -> i64 {
    i64::from(v)
}

/// Resolves `name` as a function pointer of type `T`.
///
/// # Safety
/// `T` must be the function pointer type matching the C prototype of `name`.
unsafe fn sym<T: Copy>(lib: &Library, name: &'static str) -> Result<T, LoadError> {
    // SAFETY: the caller guarantees T matches the symbol's prototype; the library is never
    // unloaded (Lib::load leaks its handle), so the pointer stays valid.
    unsafe { lib.get::<T>(name.as_bytes()) }
        .map(|s| *s)
        .map_err(|_| LoadError::Missing(name))
}

/// dlopen(path, RTLD_NOW | RTLD_GLOBAL), never unloaded. Err = dlerror text.
fn open_global(path: &str) -> Result<(), String> {
    // SAFETY: the firmware libraries' initialisers have no preconditions; they are never
    // unloaded.
    let lib = unsafe { Library::open(Some(path), RTLD_NOW | RTLD_GLOBAL) }.map_err(dl_error)?;
    let _handle = lib.into_raw();
    Ok(())
}

/// libyandex_evil.so needs libtflite.so and, on the Lite 2, libiconv.so from its own directory,
/// which is not on the loader's search path: both are loaded first (global, never unloaded), so
/// no LD_LIBRARY_PATH is needed. The C++ daemon found libiconv.so already loaded by GStreamer's
/// GLib (the player started before the microphone); the Midi's library does not need it and
/// its absence is not an error. Done once per process, like [`Lib::load`]. Err = dlerror text for
/// libtflite.so. The split libraries of [`Abi::Split`] need neither: nothing is loaded.
pub fn preload_deps() -> Result<(), String> {
    static DEPS: LazyLock<Result<(), String>> = LazyLock::new(|| {
        if detect_abi() == Abi::Split {
            return Ok(());
        }
        let _ = open_global(ICONV);
        open_global(TFLITE)
    });
    DEPS.clone()
}

impl Lib {
    /// dlopen of the firmware's libraries (RTLD_NOW | RTLD_GLOBAL) and every symbol the daemon
    /// uses, once per process (later calls return the same outcome); the libraries stay loaded for
    /// the life of the process, as in the C++ daemon.
    pub fn load() -> Result<Lib, LoadError> {
        static LIB: LazyLock<Result<Lib, LoadError>> = LazyLock::new(Lib::open);
        LIB.clone()
    }

    /// The generation of the libraries this Lib was loaded from.
    pub fn abi(&self) -> Abi {
        match self.sp {
            Spotters::Evil(_) => Abi::Evil,
            Spotters::Split(_) => Abi::Split,
        }
    }

    fn open() -> Result<Lib, LoadError> {
        match detect_abi() {
            Abi::Evil => {
                // SAFETY: the library's initialisers have no preconditions; it is never unloaded.
                let lib = unsafe { Library::open(Some(EVIL), RTLD_NOW | RTLD_GLOBAL) }
                    .map_err(|e| LoadError::Open(dl_error(e)))?;
                // SAFETY: `resolve_evil` reads symbols of the library just opened.
                let resolved = unsafe { Self::resolve_evil(&lib) };
                // Never unloaded, also after a missing symbol (as the C++ daemon).
                let _handle = lib.into_raw();
                resolved
            }
            Abi::Split => {
                // SAFETY: as above, for both libraries.
                let vqe = unsafe { Library::open(Some(SPLIT_VQE), RTLD_NOW | RTLD_GLOBAL) }
                    .map_err(|e| LoadError::Open(dl_error(e)))?;
                // SAFETY: as above.
                let spotter = unsafe { Library::open(Some(SPLIT_SPOTTER), RTLD_NOW | RTLD_GLOBAL) }
                    .map_err(|e| LoadError::Open(dl_error(e)))?;
                // SAFETY: `resolve_split` reads symbols of the libraries just opened.
                let resolved = unsafe { Self::resolve_split(&vqe, &spotter) };
                let _vqe = vqe.into_raw();
                let _spotter = spotter.into_raw();
                resolved
            }
        }
    }

    /// # Safety
    /// `lib` must be libyandex_evil.so and stay loaded for the life of the process.
    unsafe fn resolve_evil(lib: &Library) -> Result<Lib, LoadError> {
        // SAFETY: every type below is the prototype recovered from maind (module comment), with
        // size_t = usize, long = c_long, bool = C++ bool and the opaque handles as void*.
        unsafe {
            Ok(Lib {
                cfg_create: sym(lib, "YandexVqeC_Config_CreateFromPreset")?,
                cfg_free: sym(lib, "YandexVqeC_Config_Free")?,
                cfg_chunk: sym(lib, "YandexVqeC_Config_GetInputChunkSize")?,
                cfg_frame: sym(lib, "YandexVqeC_Config_GetProcessingFrame")?,
                vqe_create: VqeCreate::Models(sym(lib, "YandexVqeC_Create")?),
                vqe_free: sym(lib, "YandexVqeC_Free")?,
                vqe_process: sym(lib, "YandexVqeC_Process")?,
                vqe_out: sym(lib, "YandexVqeC_GetOutputChannelData")?,
                vqe_errmsg: sym(lib, "YandexVqeC_Error_GetMessage")?,
                vqe_errfree: sym(lib, "YandexVqeC_Error_Free")?,
                sp: Spotters::Evil(EvilSpotter {
                    conf_read: sym(lib, "spotter_conf_read")?,
                    conf_free: sym(lib, "spotter_conf_free")?,
                    conf_rate: sym(lib, "spotter_conf_native_sample_rate")?,
                    conf_chans: sym(lib, "spotter_conf_get_required_channel_count")?,
                    create: sym(lib, "spotter_create_multichannel")?,
                    destroy: sym(lib, "spotter_destroy")?,
                    submit: sym(lib, "spotter_submit_raw_data")?,
                    next: sym(lib, "spotter_get_next_event")?,
                    phrase: sym(lib, "spotter_get_current_phrase")?,
                    errmsg: sym(lib, "spotter_error_get_message")?,
                    errfree: sym(lib, "spotter_error_free")?,
                }),
                event_name: sym(lib, "event_id_to_str").ok(),
            })
        }
    }

    /// # Safety
    /// `vqe` must be libyandex_vqe.so and `spotter` libyandex_spotter.so; both stay loaded for the
    /// life of the process.
    unsafe fn resolve_split(vqe: &Library, spotter: &Library) -> Result<Lib, LoadError> {
        // SAFETY: as in `resolve_evil`; the prototypes of this generation are in the module
        // comment (maind's call sites on the Mini 3, firmware 3.21).
        unsafe {
            Ok(Lib {
                cfg_create: sym(vqe, "YandexVqeC_Config_CreateFromPreset")?,
                cfg_free: sym(vqe, "YandexVqeC_Config_Free")?,
                cfg_chunk: sym(vqe, "YandexVqeC_Config_GetInputChunkSize")?,
                cfg_frame: sym(vqe, "YandexVqeC_Config_GetProcessingFrame")?,
                vqe_create: VqeCreate::Builtin(sym(vqe, "YandexVqeC_Create")?),
                vqe_free: sym(vqe, "YandexVqeC_Free")?,
                vqe_process: sym(vqe, "YandexVqeC_Process")?,
                vqe_out: sym(vqe, "YandexVqeC_GetOutputChannelData")?,
                vqe_errmsg: sym(vqe, "YandexVqeC_Error_GetMessage")?,
                vqe_errfree: sym(vqe, "YandexVqeC_Error_Free")?,
                sp: Spotters::Split(SplitSpotter {
                    conf_read: sym(spotter, "spotter_conf_read")?,
                    conf_free: sym(spotter, "spotter_conf_free")?,
                    conf_rate: sym(spotter, "spotter_conf_native_sample_rate")?,
                    conf_chans: sym(spotter, "spotter_conf_get_required_channel_count")?,
                    create: sym(spotter, "spotter_create_multichannel")?,
                    destroy: sym(spotter, "spotter_destroy")?,
                    submit: sym(spotter, "spotter_submit_raw_data")?,
                    next: sym(spotter, "spotter_get_next_event")?,
                    phrase: sym(spotter, "spotter_get_current_phrase")?,
                }),
                event_name: sym(spotter, "event_id_to_str").ok(),
            })
        }
    }

    /// event_id_to_str(ev); None when the library does not export it.
    pub fn event_name(&self, ev: i32) -> Option<String> {
        let f = self.event_name?;
        // SAFETY: plain lookup of a static name for any value.
        let s = unsafe { f(ev) };
        // SAFETY: null or a static NUL-terminated string.
        Some(unsafe { text(s) })
    }

    /// Message of a VQE error (freed here); Ok for null.
    fn vqe_result(&self, err: P) -> Result<(), String> {
        if err.is_null() {
            return Ok(());
        }
        // SAFETY: `err` is a live YandexVqeC_Error returned by the library; its message is copied
        // before the error is freed, exactly once.
        let msg = unsafe { text((self.vqe_errmsg)(err)) };
        // SAFETY: as above.
        unsafe { (self.vqe_errfree)(err) };
        Err(msg)
    }

    /// `YandexVqeC_Create(cfg, models, 0, NULL, out)`; [`Abi::Split`] has no `models` argument.
    ///
    /// # Safety
    /// `cfg` is a live config, `models` a NUL-terminated directory, `out` valid for a write.
    unsafe fn vqe_create(&self, cfg: P, models: *const c_char, out: *mut P) -> P {
        // SAFETY: forwarded caller guarantee; no extra settings (0, NULL).
        unsafe {
            match self.vqe_create {
                VqeCreate::Models(f) => f(cfg, models, 0, ptr::null(), out),
                VqeCreate::Builtin(f) => f(cfg, 0, ptr::null(), out),
            }
        }
    }

    /// Message of a spotter error (freed here); None for null. `handle_null`: the call that set
    /// `err` returned null: in [`Abi::Split`] a message next to a handle is not an error.
    fn sp_error(&self, err: P, handle_null: bool) -> Option<String> {
        if err.is_null() {
            return None;
        }
        match self.sp {
            Spotters::Evil(t) => {
                // SAFETY: `err` is a live spotter_error set by the library; its message is copied
                // before the error is freed, exactly once.
                let msg = unsafe { text((t.errmsg)(err)) };
                // SAFETY: as above.
                unsafe { (t.errfree)(err) };
                Some(msg)
            }
            // SAFETY: the library stored a NUL-terminated message in the error cell; it is copied
            // here and left to the library (ownership is not known: a failure ends the daemon's
            // start-up anyway).
            Spotters::Split(_) => handle_null.then(|| unsafe { text(err.cast_const().cast()) }),
        }
    }

    /// spotter_conf_read.
    ///
    /// # Safety
    /// `dir` is NUL-terminated; `err` valid for a write.
    unsafe fn sp_conf_read(&self, dir: *const c_char, err: *mut P) -> P {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.conf_read)(dir, err),
                Spotters::Split(t) => (t.conf_read)(dir, err),
            }
        }
    }

    /// spotter_conf_free.
    ///
    /// # Safety
    /// `conf` is a live config, freed once; `err` valid for a write.
    unsafe fn sp_conf_free(&self, conf: P, err: *mut P) {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.conf_free)(conf, err),
                Spotters::Split(t) => (t.conf_free)(conf),
            }
        }
    }

    /// spotter_conf_native_sample_rate.
    ///
    /// # Safety
    /// `conf` is a live config; `err` valid for a write.
    unsafe fn sp_conf_rate(&self, conf: P, err: *mut P) -> c_int {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.conf_rate)(conf, err),
                Spotters::Split(t) => (t.conf_rate)(conf),
            }
        }
    }

    /// spotter_conf_get_required_channel_count.
    ///
    /// # Safety
    /// `conf` is a live config; `err` valid for a write.
    unsafe fn sp_conf_chans(&self, conf: P, ty: c_long, err: *mut P) -> c_long {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.conf_chans)(conf, ty, err),
                Spotters::Split(t) => (t.conf_chans)(conf, ty),
            }
        }
    }

    /// spotter_create_multichannel(conf, rate, n, types, ...).
    ///
    /// # Safety
    /// `conf` is a live config, `types` holds `n` ints; `err` valid for a write.
    unsafe fn sp_create(
        &self,
        conf: P,
        rate: c_int,
        n: c_int,
        types: *const c_int,
        err: *mut P,
    ) -> P {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.create)(conf, rate, n, types, false, err),
                Spotters::Split(t) => (t.create)(conf, rate, n, types, err, 0, 0),
            }
        }
    }

    /// spotter_destroy.
    ///
    /// # Safety
    /// `sp` is a live spotter, destroyed once; `err` valid for a write.
    unsafe fn sp_destroy(&self, sp: P, err: *mut P) {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.destroy)(sp, err),
                Spotters::Split(t) => (t.destroy)(sp),
            }
        }
    }

    /// spotter_submit_raw_data.
    ///
    /// # Safety
    /// `sp` is a live spotter, `data` holds `bytes` bytes; `err` valid for a write.
    unsafe fn sp_submit(&self, sp: P, data: *const c_void, bytes: usize, err: *mut P) {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.submit)(sp, data, bytes, err),
                Spotters::Split(t) => (t.submit)(sp, data, bytes),
            }
        }
    }

    /// spotter_get_next_event.
    ///
    /// # Safety
    /// `sp` is a live spotter; `err` valid for a write.
    unsafe fn sp_next(&self, sp: P, err: *mut P) -> c_int {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.next)(sp, err),
                Spotters::Split(t) => (t.next)(sp),
            }
        }
    }

    /// spotter_get_current_phrase.
    ///
    /// # Safety
    /// `sp` is a live spotter; `err` valid for a write.
    unsafe fn sp_phrase(&self, sp: P, err: *mut P) -> *const c_char {
        // SAFETY: forwarded caller guarantee.
        unsafe {
            match self.sp {
                Spotters::Evil(t) => (t.phrase)(sp, err),
                Spotters::Split(t) => (t.phrase)(sp),
            }
        }
    }
}

/// Copies a C string; "" for null.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string that stays valid during the call.
unsafe fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: forwarded caller guarantee.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn path_cstr(p: &Path) -> Result<CString, String> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("{}: NUL in path", p.display()))
}

/// The step of [`Vqe::new`] that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VqeStep {
    /// YandexVqeC_Config_CreateFromPreset
    Config,
    /// YandexVqeC_Create
    Create,
}

/// A VQE instance with its config; both freed on drop (instance first).
pub struct Vqe {
    lib: Lib,
    cfg: P,
    vqe: P,
    chunk: i32,
    frame: i32,
    /// Expected sample counts of one Process call: chunk × mic / speaker channels.
    mic_samples: usize,
    spk_samples: usize,
    /// Outputs exist only after a successful Process.
    processed: bool,
}

// SAFETY: the VQE is used by one thread at a time (Vqe is not Sync and every call that runs the
// engine takes &mut self or happens on the owning thread); the C++ daemon created it on the main
// thread and ran it on the processing thread the same way.
unsafe impl Send for Vqe {}

impl Vqe {
    /// YandexVqeC_Config_CreateFromPreset(preset) + YandexVqeC_Create(cfg, models, 0, NULL); the
    /// split libraries (Mini 3) have no `models` argument and ignore it.
    /// `nmic` / `nspk`: the preset's channel counts (the model's mic / speaker channels); every
    /// `process` call must bring exactly one chunk of them.
    pub fn new(
        lib: &Lib,
        preset: &str,
        models: &Path,
        nmic: usize,
        nspk: usize,
    ) -> Result<Vqe, (VqeStep, String)> {
        let preset = CString::new(preset)
            .map_err(|_| (VqeStep::Config, format!("{preset}: NUL in preset name")))?;
        let models = path_cstr(models).map_err(|e| (VqeStep::Create, e))?;
        if nmic == 0 || nspk == 0 {
            return Err((
                VqeStep::Config,
                format!("{nmic} mic, {nspk} speaker channels"),
            ));
        }
        let mut v = Vqe {
            lib: *lib,
            cfg: ptr::null_mut(),
            vqe: ptr::null_mut(),
            chunk: 0,
            frame: 0,
            mic_samples: 0,
            spk_samples: 0,
            processed: false,
        };
        // SAFETY: NUL-terminated preset, out-pointer to a field; a config set despite an error is
        // freed by Drop, as the C++ engine's destructor did.
        let err = unsafe { (lib.cfg_create)(preset.as_ptr(), &mut v.cfg) };
        lib.vqe_result(err).map_err(|e| (VqeStep::Config, e))?;
        if v.cfg.is_null() {
            return Err((VqeStep::Config, "no config".into()));
        }
        // SAFETY: valid config.
        v.chunk = unsafe { (lib.cfg_chunk)(v.cfg) };
        // SAFETY: valid config.
        v.frame = unsafe { (lib.cfg_frame)(v.cfg) };
        let chunk = usize::try_from(v.chunk).unwrap_or(0);
        v.mic_samples = chunk * nmic;
        v.spk_samples = chunk * nspk;
        // SAFETY: valid config, NUL-terminated models directory (unused by the split libraries),
        // out-pointer to a field.
        let err = unsafe { lib.vqe_create(v.cfg, models.as_ptr(), &mut v.vqe) };
        lib.vqe_result(err).map_err(|e| (VqeStep::Create, e))?;
        if v.vqe.is_null() {
            return Err((VqeStep::Create, "no instance".into()));
        }
        Ok(v)
    }

    /// YandexVqeC_Config_GetInputChunkSize: capture frames per Process call.
    pub fn chunk(&self) -> i32 {
        self.chunk
    }

    /// YandexVqeC_Config_GetProcessingFrame: output samples per channel and chunk.
    pub fn frame(&self) -> i32 {
        self.frame
    }

    /// One chunk: `mics` / `spk` frame-interleaved, `chunk()` frames each of the preset's
    /// channels; other lengths are refused without calling the library.
    pub fn process(&mut self, mics: &[f32], spk: &[f32]) -> Result<(), String> {
        if mics.len() != self.mic_samples || spk.len() != self.spk_samples || mics.is_empty() {
            return Err(format!(
                "{} mic / {} speaker samples, need {} / {}",
                mics.len(),
                spk.len(),
                self.mic_samples,
                self.spk_samples
            ));
        }
        // SAFETY: valid instance; both buffers are live for the call and hold exactly one chunk
        // of the preset's channels (checked above), the counts the library is given.
        let err = unsafe {
            (self.lib.vqe_process)(self.vqe, mics.as_ptr(), mics.len(), spk.as_ptr(), spk.len())
        };
        self.processed = false;
        self.lib.vqe_result(err)?;
        self.processed = true;
        Ok(())
    }

    /// Output channel `ty` (voicetech.AudioInputChannelType, 0..=16) of the last chunk; empty
    /// when the preset has no such output, for any other type and before the first `process`.
    /// Valid until the next `process`.
    pub fn output(&self, ty: i32) -> Result<&[f32], String> {
        if !self.processed || !(0..=MAX_CHANNEL_TYPE).contains(&ty) {
            return Ok(&[]);
        }
        let mut d: *const f32 = ptr::null();
        let mut n = 0usize;
        // SAFETY: valid instance, out-pointers to locals.
        let err = unsafe { (self.lib.vqe_out)(self.vqe, ty, 0, &mut d, &mut n) };
        self.lib.vqe_result(err)?;
        if d.is_null() || n == 0 {
            return Ok(&[]);
        }
        // SAFETY: the library hands out `n` floats of its output buffer for this channel; it is
        // rewritten only by the next YandexVqeC_Process, which needs &mut self, so the slice
        // cannot outlive it. Outputs of several channels are held at once, as the C++ engine did.
        Ok(unsafe { std::slice::from_raw_parts(d, n) })
    }
}

impl Drop for Vqe {
    fn drop(&mut self) {
        if !self.vqe.is_null() {
            // SAFETY: created by YandexVqeC_Create, freed once.
            unsafe { (self.lib.vqe_free)(self.vqe) };
        }
        if !self.cfg.is_null() {
            // SAFETY: created by YandexVqeC_Config_CreateFromPreset, freed once, after the
            // instance built from it.
            unsafe { (self.lib.cfg_free)(self.cfg) };
        }
    }
}

/// A spotter model config (spotter_conf_read); freed on drop.
pub struct SpotterConf {
    lib: Lib,
    conf: NonNull<c_void>,
}

// SAFETY: as Vqe: one thread at a time.
unsafe impl Send for SpotterConf {}

impl SpotterConf {
    /// spotter_conf_read(dir). Err(None): no config and no error message.
    pub fn read(lib: &Lib, dir: &Path) -> Result<SpotterConf, Option<String>> {
        let dir = path_cstr(dir).map_err(Some)?;
        let mut err = ptr::null_mut();
        // SAFETY: NUL-terminated directory, out-pointer to a local.
        let conf = unsafe { lib.sp_conf_read(dir.as_ptr(), &mut err) };
        if let Some(e) = lib.sp_error(err, conf.is_null()) {
            // A config returned together with an error is not trusted (nor freed: the C++
            // daemon did not touch it either).
            return Err(Some(e));
        }
        NonNull::new(conf)
            .map(|conf| SpotterConf { lib: *lib, conf })
            .ok_or(None)
    }

    /// spotter_conf_get_required_channel_count(conf, ty).
    pub fn channel_count(&self, ty: i64) -> Result<i64, String> {
        if !(0..SPOTTER_TYPES as i64).contains(&ty) {
            return Err(format!("type {ty} out of range"));
        }
        // Lossless: 0..SPOTTER_TYPES fits any C long.
        let ty = ty as c_long;
        let mut err = ptr::null_mut();
        // SAFETY: valid config, out-pointer to a local.
        let n = unsafe { self.lib.sp_conf_chans(self.conf.as_ptr(), ty, &mut err) };
        match self.lib.sp_error(err, true) {
            Some(e) => Err(e),
            None => Ok(from_long(n)),
        }
    }

    /// spotter_conf_native_sample_rate: the value and the error, if any (the C++ daemon logged
    /// both).
    pub fn native_rate(&self) -> (i32, Option<String>) {
        let mut err = ptr::null_mut();
        // SAFETY: valid config, out-pointer to a local.
        let rate = unsafe { self.lib.sp_conf_rate(self.conf.as_ptr(), &mut err) };
        (rate, self.lib.sp_error(err, true))
    }
}

impl Drop for SpotterConf {
    fn drop(&mut self) {
        let mut err = ptr::null_mut();
        // SAFETY: created by spotter_conf_read, freed once; out-pointer to a local.
        unsafe { self.lib.sp_conf_free(self.conf.as_ptr(), &mut err) };
        if let Some(e) = self.lib.sp_error(err, true) {
            log::error!("spotter_conf_free: {e}");
        }
    }
}

/// A multichannel phrase spotter; destroyed on drop.
pub struct Spotter {
    lib: Lib,
    sp: NonNull<c_void>,
}

// SAFETY: as Vqe: one thread at a time.
unsafe impl Send for Spotter {}

impl Spotter {
    /// spotter_create_multichannel(conf, rate, types.len(), types, ...): the spotter (if the
    /// library returned one) and the error (if it set one). Both can be present ([`Abi::Evil`]).
    pub fn create(
        conf: &SpotterConf,
        rate: i32,
        types: &[i32],
    ) -> (Option<Spotter>, Option<String>) {
        if let Some(t) = types
            .iter()
            .find(|&&t| !(0..SPOTTER_TYPES as i32).contains(&t))
        {
            return (None, Some(format!("type {t} out of range")));
        }
        let Ok(n) = c_int::try_from(types.len()) else {
            return (None, Some(format!("{} channels", types.len())));
        };
        let mut err = ptr::null_mut();
        // SAFETY: valid config; `types` holds `n` ints and outlives the call; out-pointer to a
        // local.
        let sp = unsafe {
            conf.lib
                .sp_create(conf.conf.as_ptr(), rate, n, types.as_ptr(), &mut err)
        };
        let lib = conf.lib;
        (
            NonNull::new(sp).map(|sp| Spotter { lib, sp }),
            lib.sp_error(err, sp.is_null()),
        )
    }

    /// spotter_submit_raw_data: s16 samples interleaved in the order of the creation types.
    pub fn submit(&mut self, pcm: &[i16]) -> Result<(), String> {
        let mut err = ptr::null_mut();
        // SAFETY: valid spotter; the byte count is that of `pcm`, live for the call; out-pointer
        // to a local.
        unsafe {
            self.lib.sp_submit(
                self.sp.as_ptr(),
                pcm.as_ptr().cast(),
                size_of_val(pcm),
                &mut err,
            )
        };
        self.lib.sp_error(err, true).map_or(Ok(()), Err)
    }

    /// spotter_get_next_event.
    pub fn next_event(&mut self) -> Result<i32, String> {
        let mut err = ptr::null_mut();
        // SAFETY: valid spotter, out-pointer to a local.
        let ev = unsafe { self.lib.sp_next(self.sp.as_ptr(), &mut err) };
        self.lib.sp_error(err, true).map_or(Ok(ev), Err)
    }

    /// spotter_get_current_phrase: the phrase (None for null) and the error, if any.
    pub fn phrase(&mut self) -> (Option<String>, Option<String>) {
        let mut err = ptr::null_mut();
        // SAFETY: valid spotter, out-pointer to a local.
        let p = unsafe { self.lib.sp_phrase(self.sp.as_ptr(), &mut err) };
        let phrase = if p.is_null() {
            None
        } else {
            // SAFETY: a NUL-terminated string owned by the spotter, copied before any other call
            // into it.
            Some(unsafe { text(p) })
        };
        (phrase, self.lib.sp_error(err, true))
    }
}

impl Drop for Spotter {
    fn drop(&mut self) {
        let mut err = ptr::null_mut();
        // SAFETY: created by spotter_create_multichannel, destroyed once; out-pointer to a local.
        unsafe { self.lib.sp_destroy(self.sp.as_ptr(), &mut err) };
        if let Some(e) = self.lib.sp_error(err, true) {
            log::error!("spotter_destroy: {e}");
        }
    }
}
