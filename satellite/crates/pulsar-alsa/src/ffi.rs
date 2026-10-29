//! libasound through dlopen: every symbol is resolved once into [`Lib`]; the safe wrappers below
//! are the only callers of those pointers.

use std::error::Error as _;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};

pub(crate) const SONAME: &str = "libasound.so.2";

pub(crate) const STREAM_PLAYBACK: c_int = 0;
pub(crate) const STREAM_CAPTURE: c_int = 1;
pub(crate) const PCM_NONBLOCK: c_int = 0x1;
pub(crate) const ACCESS_RW_INTERLEAVED: c_int = 3;
pub(crate) const FORMAT_S16_LE: c_int = 2;
pub(crate) const FORMAT_S32_LE: c_int = 10;
pub(crate) const FORMAT_S24_3LE: c_int = 32;
const CTL_ELEM_IFACE_MIXER: c_int = 2;
// snd_ctl_elem_value_t holds 128 integers; alsa-lib asserts (aborts) on a larger index.
pub(crate) const CTL_MAX_INTEGERS: usize = 128;

type Pcm = c_void;
type HwP = c_void;
type SwP = c_void;
type Ctl = c_void;
type ElemId = c_void;
type ElemInfo = c_void;
type ElemValue = c_void;
type Uframes = c_ulong;
type Sframes = c_long;

// alsa-lib calls the handler as `(file, line, function, err, fmt, ...)`. It is declared here
// without the variadic tail: on the AAPCS (arm, aarch64 Linux) and SysV x86_64 ABIs the named
// arguments of a variadic call are passed exactly like those of a plain call, and the handler
// never reads the tail.
type ErrorHandler = unsafe extern "C" fn(*const c_char, c_int, *const c_char, c_int, *const c_char);

pub(crate) struct Lib {
    _lib: Library,
    modern_aliases: bool,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    lib_error_set_handler: unsafe extern "C" fn(Option<ErrorHandler>) -> c_int,

    pcm_open: unsafe extern "C" fn(*mut *mut Pcm, *const c_char, c_int, c_int) -> c_int,
    pcm_close: unsafe extern "C" fn(*mut Pcm) -> c_int,
    pcm_prepare: unsafe extern "C" fn(*mut Pcm) -> c_int,
    pcm_drop: unsafe extern "C" fn(*mut Pcm) -> c_int,
    pcm_resume: unsafe extern "C" fn(*mut Pcm) -> c_int,
    pcm_readi: unsafe extern "C" fn(*mut Pcm, *mut c_void, Uframes) -> Sframes,
    pcm_writei: unsafe extern "C" fn(*mut Pcm, *const c_void, Uframes) -> Sframes,
    pcm_wait: unsafe extern "C" fn(*mut Pcm, c_int) -> c_int,
    pcm_avail_update: unsafe extern "C" fn(*mut Pcm) -> Sframes,
    pcm_delay: unsafe extern "C" fn(*mut Pcm, *mut Sframes) -> c_int,
    pcm_recover: unsafe extern "C" fn(*mut Pcm, c_int, c_int) -> c_int,

    hw_malloc: unsafe extern "C" fn(*mut *mut HwP) -> c_int,
    hw_free: unsafe extern "C" fn(*mut HwP),
    hw_any: unsafe extern "C" fn(*mut Pcm, *mut HwP) -> c_int,
    hw_set_access: unsafe extern "C" fn(*mut Pcm, *mut HwP, c_int) -> c_int,
    hw_set_format: unsafe extern "C" fn(*mut Pcm, *mut HwP, c_int) -> c_int,
    hw_set_channels: unsafe extern "C" fn(*mut Pcm, *mut HwP, c_uint) -> c_int,
    hw_set_rate: unsafe extern "C" fn(*mut Pcm, *mut HwP, c_uint, c_int) -> c_int,
    hw_set_rate_near: unsafe extern "C" fn(*mut Pcm, *mut HwP, *mut c_uint, *mut c_int) -> c_int,
    hw_set_period_size_near:
        unsafe extern "C" fn(*mut Pcm, *mut HwP, *mut Uframes, *mut c_int) -> c_int,
    hw_set_buffer_size_near: unsafe extern "C" fn(*mut Pcm, *mut HwP, *mut Uframes) -> c_int,
    hw_apply: unsafe extern "C" fn(*mut Pcm, *mut HwP) -> c_int,
    hw_get_period_size: unsafe extern "C" fn(*const HwP, *mut Uframes, *mut c_int) -> c_int,
    hw_get_buffer_size: unsafe extern "C" fn(*const HwP, *mut Uframes) -> c_int,

    sw_malloc: unsafe extern "C" fn(*mut *mut SwP) -> c_int,
    sw_free: unsafe extern "C" fn(*mut SwP),
    sw_current: unsafe extern "C" fn(*mut Pcm, *mut SwP) -> c_int,
    sw_set_start_threshold: unsafe extern "C" fn(*mut Pcm, *mut SwP, Uframes) -> c_int,
    sw_set_avail_min: unsafe extern "C" fn(*mut Pcm, *mut SwP, Uframes) -> c_int,
    sw_apply: unsafe extern "C" fn(*mut Pcm, *mut SwP) -> c_int,

    ctl_open: unsafe extern "C" fn(*mut *mut Ctl, *const c_char, c_int) -> c_int,
    ctl_close: unsafe extern "C" fn(*mut Ctl) -> c_int,
    ctl_elem_info: unsafe extern "C" fn(*mut Ctl, *mut ElemInfo) -> c_int,
    ctl_elem_read: unsafe extern "C" fn(*mut Ctl, *mut ElemValue) -> c_int,
    ctl_elem_write: unsafe extern "C" fn(*mut Ctl, *mut ElemValue) -> c_int,
    id_malloc: unsafe extern "C" fn(*mut *mut ElemId) -> c_int,
    id_free: unsafe extern "C" fn(*mut ElemId),
    id_set_interface: unsafe extern "C" fn(*mut ElemId, c_int),
    id_set_name: unsafe extern "C" fn(*mut ElemId, *const c_char),
    info_malloc: unsafe extern "C" fn(*mut *mut ElemInfo) -> c_int,
    info_free: unsafe extern "C" fn(*mut ElemInfo),
    info_set_id: unsafe extern "C" fn(*mut ElemInfo, *const ElemId),
    value_malloc: unsafe extern "C" fn(*mut *mut ElemValue) -> c_int,
    value_free: unsafe extern "C" fn(*mut ElemValue),
    value_set_interface: unsafe extern "C" fn(*mut ElemValue, c_int),
    value_set_name: unsafe extern "C" fn(*mut ElemValue, *const c_char),
    value_get_integer: unsafe extern "C" fn(*const ElemValue, c_uint) -> c_long,
    value_set_integer: unsafe extern "C" fn(*mut ElemValue, c_uint, c_long),
}

fn dl_error(e: libloading::Error) -> String {
    // The source carries dlerror()'s text ("<file>: cannot open shared object file: ...").
    match e.source() {
        Some(src) => src.to_string(),
        None => e.to_string(),
    }
}

/// Resolves `name` (NUL-terminated) as a function pointer of type `T`.
///
/// # Safety
/// `T` must be the function pointer type matching the C prototype of `name`.
unsafe fn sym<T: Copy>(lib: &Library, name: &CStr) -> Result<T, String> {
    // SAFETY: the caller guarantees T matches the symbol's prototype; the pointer is only used
    // while `lib` stays loaded (Lib keeps it for the life of the process).
    unsafe { lib.get::<T>(name).map(|s| *s).map_err(dl_error) }
}

/// The current hw_params ABI: the `__snd_*` alias where the library exports one, else the plain
/// name.
///
/// # Safety
/// As [`sym`], for both names.
unsafe fn modern<T: Copy>(lib: &Library, alias: &CStr, plain: &CStr) -> Result<T, String> {
    // SAFETY: forwarded caller guarantee; alias and plain name share the prototype.
    unsafe { sym(lib, alias).or_else(|_| sym(lib, plain)) }
}

pub(crate) fn load_from(soname: &str) -> Result<Lib, String> {
    // Midi firmware images with a 32-bit userland ship a versioned libasound (ALSA_0.9 +
    // ALSA_0.9.0rc4). There the plain names of the hw_params get / set_*_near functions can bind
    // to the ALSA_0.9 compat entries, which take values instead of pointers (period / buffer
    // read back as 0, wake word dead). The __snd_* aliases are the current ABI; a library without
    // them (the Lite 2's unversioned one) has only that ABI under the plain name.
    //
    // RTLD_GLOBAL: libasound is shared with the GStreamer alsasink of the same process, as the
    // directly linked C++ daemon did.
    //
    // SAFETY: libasound's initialisers have no preconditions; it is never unloaded.
    let lib = unsafe { Library::open(Some(soname), RTLD_NOW | RTLD_GLOBAL) }.map_err(dl_error)?;
    // SAFETY: every `sym`/`modern` type below is the prototype from alsa/pcm.h, alsa/control.h
    // and alsa/error.h, with snd_pcm_uframes_t = unsigned long, snd_pcm_sframes_t = long and the
    // enums as int.
    unsafe {
        let modern_aliases =
            sym::<unsafe extern "C" fn()>(&lib, c"__snd_pcm_hw_params_set_rate_near").is_ok();
        Ok(Lib {
            modern_aliases,
            strerror: sym(&lib, c"snd_strerror")?,
            lib_error_set_handler: sym(&lib, c"snd_lib_error_set_handler")?,
            pcm_open: sym(&lib, c"snd_pcm_open")?,
            pcm_close: sym(&lib, c"snd_pcm_close")?,
            pcm_prepare: sym(&lib, c"snd_pcm_prepare")?,
            pcm_drop: sym(&lib, c"snd_pcm_drop")?,
            pcm_resume: sym(&lib, c"snd_pcm_resume")?,
            pcm_readi: sym(&lib, c"snd_pcm_readi")?,
            pcm_writei: sym(&lib, c"snd_pcm_writei")?,
            pcm_wait: sym(&lib, c"snd_pcm_wait")?,
            pcm_avail_update: sym(&lib, c"snd_pcm_avail_update")?,
            pcm_delay: sym(&lib, c"snd_pcm_delay")?,
            pcm_recover: sym(&lib, c"snd_pcm_recover")?,
            hw_malloc: sym(&lib, c"snd_pcm_hw_params_malloc")?,
            hw_free: sym(&lib, c"snd_pcm_hw_params_free")?,
            hw_any: sym(&lib, c"snd_pcm_hw_params_any")?,
            hw_set_access: sym(&lib, c"snd_pcm_hw_params_set_access")?,
            hw_set_format: sym(&lib, c"snd_pcm_hw_params_set_format")?,
            hw_set_channels: sym(&lib, c"snd_pcm_hw_params_set_channels")?,
            hw_set_rate: sym(&lib, c"snd_pcm_hw_params_set_rate")?,
            hw_set_rate_near: modern(
                &lib,
                c"__snd_pcm_hw_params_set_rate_near",
                c"snd_pcm_hw_params_set_rate_near",
            )?,
            hw_set_period_size_near: modern(
                &lib,
                c"__snd_pcm_hw_params_set_period_size_near",
                c"snd_pcm_hw_params_set_period_size_near",
            )?,
            hw_set_buffer_size_near: modern(
                &lib,
                c"__snd_pcm_hw_params_set_buffer_size_near",
                c"snd_pcm_hw_params_set_buffer_size_near",
            )?,
            hw_apply: sym(&lib, c"snd_pcm_hw_params")?,
            hw_get_period_size: modern(
                &lib,
                c"__snd_pcm_hw_params_get_period_size",
                c"snd_pcm_hw_params_get_period_size",
            )?,
            hw_get_buffer_size: modern(
                &lib,
                c"__snd_pcm_hw_params_get_buffer_size",
                c"snd_pcm_hw_params_get_buffer_size",
            )?,
            sw_malloc: sym(&lib, c"snd_pcm_sw_params_malloc")?,
            sw_free: sym(&lib, c"snd_pcm_sw_params_free")?,
            sw_current: sym(&lib, c"snd_pcm_sw_params_current")?,
            sw_set_start_threshold: sym(&lib, c"snd_pcm_sw_params_set_start_threshold")?,
            sw_set_avail_min: sym(&lib, c"snd_pcm_sw_params_set_avail_min")?,
            sw_apply: sym(&lib, c"snd_pcm_sw_params")?,
            ctl_open: sym(&lib, c"snd_ctl_open")?,
            ctl_close: sym(&lib, c"snd_ctl_close")?,
            ctl_elem_info: sym(&lib, c"snd_ctl_elem_info")?,
            ctl_elem_read: sym(&lib, c"snd_ctl_elem_read")?,
            ctl_elem_write: sym(&lib, c"snd_ctl_elem_write")?,
            id_malloc: sym(&lib, c"snd_ctl_elem_id_malloc")?,
            id_free: sym(&lib, c"snd_ctl_elem_id_free")?,
            id_set_interface: sym(&lib, c"snd_ctl_elem_id_set_interface")?,
            id_set_name: sym(&lib, c"snd_ctl_elem_id_set_name")?,
            info_malloc: sym(&lib, c"snd_ctl_elem_info_malloc")?,
            info_free: sym(&lib, c"snd_ctl_elem_info_free")?,
            info_set_id: sym(&lib, c"snd_ctl_elem_info_set_id")?,
            value_malloc: sym(&lib, c"snd_ctl_elem_value_malloc")?,
            value_free: sym(&lib, c"snd_ctl_elem_value_free")?,
            value_set_interface: sym(&lib, c"snd_ctl_elem_value_set_interface")?,
            value_set_name: sym(&lib, c"snd_ctl_elem_value_set_name")?,
            value_get_integer: sym(&lib, c"snd_ctl_elem_value_get_integer")?,
            value_set_integer: sym(&lib, c"snd_ctl_elem_value_set_integer")?,
            _lib: lib,
        })
    }
}

/// Set by pulsar_alsa::init before the first load; empty = the loader's soname lookup.
static LIB_PATH: OnceLock<&'static str> = OnceLock::new();

pub(crate) fn set_library_path(path: &'static str) {
    // First setting wins: a later call would be too late for the already-loaded library.
    let _ = LIB_PATH.set(path);
}

// Logged once: later calls only see -ENOENT.
static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();

pub(crate) fn lib() -> Result<&'static Lib, &'static str> {
    LIB.get_or_init(|| {
        let path = LIB_PATH.get().copied().unwrap_or_default();
        let path = if path.is_empty() { SONAME } else { path };
        load_from(path).inspect_err(|e| log::error!("alsa: {e}"))
    })
    .as_ref()
    .map_err(String::as_str)
}

fn uframes(v: u64) -> Uframes {
    Uframes::try_from(v).unwrap_or(Uframes::MAX)
}

#[allow(
    clippy::useless_conversion,
    reason = "unsigned long is 32-bit on armhf, 64-bit on aarch64"
)]
fn from_uframes(v: Uframes) -> u64 {
    u64::from(v)
}

#[allow(
    clippy::useless_conversion,
    reason = "long is 32-bit on armhf, 64-bit on aarch64"
)]
fn from_long(v: c_long) -> i64 {
    i64::from(v)
}

impl Lib {
    pub(crate) fn modern_aliases(&self) -> bool {
        self.modern_aliases
    }

    pub(crate) fn strerror(&self, err: i32) -> String {
        // SAFETY: snd_strerror returns a static NUL-terminated string for any value.
        let s = unsafe { (self.strerror)(err) };
        if s.is_null() {
            return format!("error {err}");
        }
        // SAFETY: non-null, NUL-terminated, static.
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    }

    pub(crate) fn set_quiet_error_handler(&'static self) {
        // SAFETY: the handler matches the named arguments of snd_lib_error_handler_t (see
        // ErrorHandler) and stays valid for the life of the process.
        unsafe { (self.lib_error_set_handler)(Some(quiet_handler)) };
    }

    pub(crate) fn pcm_open(
        &'static self,
        name: &CStr,
        stream: c_int,
        mode: c_int,
    ) -> Result<RawPcm, i32> {
        let mut pcm = ptr::null_mut();
        // SAFETY: out-pointer to a local, NUL-terminated name.
        let rc = unsafe { (self.pcm_open)(&mut pcm, name.as_ptr(), stream, mode) };
        match NonNull::new(pcm) {
            Some(pcm) if rc >= 0 => Ok(RawPcm { lib: self, pcm }),
            _ => Err(if rc < 0 { rc } else { -libc::EINVAL }),
        }
    }

    pub(crate) fn ctl_open(&'static self, name: &CStr) -> Result<RawCtl, i32> {
        let mut ctl = ptr::null_mut();
        // SAFETY: out-pointer to a local, NUL-terminated name.
        let rc = unsafe { (self.ctl_open)(&mut ctl, name.as_ptr(), 0) };
        match NonNull::new(ctl) {
            Some(ctl) if rc >= 0 => Ok(RawCtl { lib: self, ctl }),
            _ => Err(if rc < 0 { rc } else { -libc::EINVAL }),
        }
    }
}

unsafe extern "C" fn quiet_handler(
    file: *const c_char,
    line: c_int,
    function: *const c_char,
    err: c_int,
    _fmt: *const c_char,
) {
    // alsa-lib diagnostics (alsasink's IEC958 probe "Unknown PCM speakers:{AES0 ...}", "Cannot
    // read Channel Map ctl") are noise on this device; real failures surface as errors of the
    // calls themselves.
    // Only when stderr itself logs at debug level (as the C++ daemon); otherwise nothing reaches
    // the sink.
    if pulsar_core::stderr_level() < log::LevelFilter::Debug {
        return;
    }
    let text = |p: *const c_char| {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: alsa-lib passes NUL-terminated strings (__FILE__, __func__).
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    };
    let (file, function) = (text(file), text(function));
    match (err != 0).then(lib).and_then(Result::ok) {
        Some(lib) => log::debug!("alsa: {file}:{line} {function}: {}", lib.strerror(err)),
        None => log::debug!("alsa: {file}:{line} {function}"),
    }
}

/// An open snd_pcm_t; closed on drop.
pub(crate) struct RawPcm {
    lib: &'static Lib,
    pcm: NonNull<Pcm>,
}

// SAFETY: a snd_pcm_t may be used from any thread, one at a time; RawPcm is not Sync and every
// call takes &mut self or happens on its owner's thread.
unsafe impl Send for RawPcm {}

impl Drop for RawPcm {
    fn drop(&mut self) {
        // SAFETY: opened by snd_pcm_open, closed exactly once.
        unsafe { (self.lib.pcm_close)(self.pcm.as_ptr()) };
    }
}

/// snd_pcm_hw_params_t from snd_pcm_hw_params_malloc.
pub(crate) struct HwParams {
    lib: &'static Lib,
    p: NonNull<HwP>,
}

impl HwParams {
    pub(crate) fn new(lib: &'static Lib) -> Result<Self, i32> {
        let mut p = ptr::null_mut();
        // SAFETY: out-pointer to a local.
        let rc = unsafe { (lib.hw_malloc)(&mut p) };
        NonNull::new(p)
            .filter(|_| rc >= 0)
            .map(|p| Self { lib, p })
            .ok_or(if rc < 0 { rc } else { -libc::ENOMEM })
    }

    pub(crate) fn get_period_size(&self, frames: &mut u64) -> i32 {
        let mut v: Uframes = 0;
        // SAFETY: valid params, out-pointers to locals (current ABI, see load_from).
        let rc = unsafe { (self.lib.hw_get_period_size)(self.p.as_ptr(), &mut v, ptr::null_mut()) };
        *frames = from_uframes(v);
        rc
    }

    pub(crate) fn get_buffer_size(&self, frames: &mut u64) -> i32 {
        let mut v: Uframes = 0;
        // SAFETY: valid params, out-pointer to a local (current ABI, see load_from).
        let rc = unsafe { (self.lib.hw_get_buffer_size)(self.p.as_ptr(), &mut v) };
        *frames = from_uframes(v);
        rc
    }
}

impl Drop for HwParams {
    fn drop(&mut self) {
        // SAFETY: allocated by snd_pcm_hw_params_malloc, freed once.
        unsafe { (self.lib.hw_free)(self.p.as_ptr()) };
    }
}

/// snd_pcm_sw_params_t from snd_pcm_sw_params_malloc.
pub(crate) struct SwParams {
    lib: &'static Lib,
    p: NonNull<SwP>,
}

impl SwParams {
    pub(crate) fn new(lib: &'static Lib) -> Result<Self, i32> {
        let mut p = ptr::null_mut();
        // SAFETY: out-pointer to a local.
        let rc = unsafe { (lib.sw_malloc)(&mut p) };
        NonNull::new(p)
            .filter(|_| rc >= 0)
            .map(|p| Self { lib, p })
            .ok_or(if rc < 0 { rc } else { -libc::ENOMEM })
    }
}

impl Drop for SwParams {
    fn drop(&mut self) {
        // SAFETY: allocated by snd_pcm_sw_params_malloc, freed once.
        unsafe { (self.lib.sw_free)(self.p.as_ptr()) };
    }
}

// SAFETY (all RawPcm methods): `self.pcm` is an open PCM, params objects are live allocations of
// the same library, out-pointers point to locals; return codes are passed through unchanged.
impl RawPcm {
    pub(crate) fn lib(&self) -> &'static Lib {
        self.lib
    }

    pub(crate) fn hw_any(&mut self, hw: &mut HwParams) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.hw_any)(self.pcm.as_ptr(), hw.p.as_ptr()) }
    }

    pub(crate) fn hw_set_access(&mut self, hw: &mut HwParams, access: c_int) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.hw_set_access)(self.pcm.as_ptr(), hw.p.as_ptr(), access) }
    }

    pub(crate) fn hw_set_format(&mut self, hw: &mut HwParams, format: c_int) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.hw_set_format)(self.pcm.as_ptr(), hw.p.as_ptr(), format) }
    }

    pub(crate) fn hw_set_channels(&mut self, hw: &mut HwParams, channels: u32) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.hw_set_channels)(self.pcm.as_ptr(), hw.p.as_ptr(), channels) }
    }

    pub(crate) fn hw_set_rate(&mut self, hw: &mut HwParams, rate: u32) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.hw_set_rate)(self.pcm.as_ptr(), hw.p.as_ptr(), rate, 0) }
    }

    pub(crate) fn hw_set_rate_near(&mut self, hw: &mut HwParams, rate: &mut u32) -> i32 {
        // SAFETY: see impl comment; current ABI (pointer argument), see load_from.
        unsafe {
            (self.lib.hw_set_rate_near)(self.pcm.as_ptr(), hw.p.as_ptr(), rate, ptr::null_mut())
        }
    }

    pub(crate) fn hw_set_period_size_near(&mut self, hw: &mut HwParams, frames: &mut u64) -> i32 {
        let mut v = uframes(*frames);
        // SAFETY: see impl comment; current ABI (pointer argument), see load_from.
        let rc = unsafe {
            (self.lib.hw_set_period_size_near)(
                self.pcm.as_ptr(),
                hw.p.as_ptr(),
                &mut v,
                ptr::null_mut(),
            )
        };
        *frames = from_uframes(v);
        rc
    }

    pub(crate) fn hw_set_buffer_size_near(&mut self, hw: &mut HwParams, frames: &mut u64) -> i32 {
        let mut v = uframes(*frames);
        // SAFETY: see impl comment; current ABI (pointer argument), see load_from.
        let rc =
            unsafe { (self.lib.hw_set_buffer_size_near)(self.pcm.as_ptr(), hw.p.as_ptr(), &mut v) };
        *frames = from_uframes(v);
        rc
    }

    pub(crate) fn hw_apply(&mut self, hw: &mut HwParams) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.hw_apply)(self.pcm.as_ptr(), hw.p.as_ptr()) }
    }

    pub(crate) fn sw_current(&mut self, sw: &mut SwParams) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.sw_current)(self.pcm.as_ptr(), sw.p.as_ptr()) }
    }

    pub(crate) fn sw_set_start_threshold(&mut self, sw: &mut SwParams, frames: u64) -> i32 {
        // SAFETY: see impl comment.
        unsafe {
            (self.lib.sw_set_start_threshold)(self.pcm.as_ptr(), sw.p.as_ptr(), uframes(frames))
        }
    }

    pub(crate) fn sw_set_avail_min(&mut self, sw: &mut SwParams, frames: u64) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.sw_set_avail_min)(self.pcm.as_ptr(), sw.p.as_ptr(), uframes(frames)) }
    }

    pub(crate) fn sw_apply(&mut self, sw: &mut SwParams) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.sw_apply)(self.pcm.as_ptr(), sw.p.as_ptr()) }
    }

    pub(crate) fn prepare(&mut self) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.pcm_prepare)(self.pcm.as_ptr()) }
    }

    pub(crate) fn drop_frames(&mut self) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.pcm_drop)(self.pcm.as_ptr()) }
    }

    pub(crate) fn resume(&mut self) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.pcm_resume)(self.pcm.as_ptr()) }
    }

    /// `frame_bytes`: bytes per frame of the configured format and channel count (0 = not
    /// configured).
    pub(crate) fn readi(
        &mut self,
        buf: &mut [i16],
        frames: usize,
        frame_bytes: usize,
    ) -> Result<usize, i32> {
        if frame_bytes == 0
            || frames
                .checked_mul(frame_bytes)
                .is_none_or(|n| n > size_of_val(buf))
        {
            return Err(-libc::EINVAL);
        }
        // SAFETY: see impl comment; `buf` holds at least `frames` frames (checked above).
        let n = unsafe {
            (self.lib.pcm_readi)(
                self.pcm.as_ptr(),
                buf.as_mut_ptr().cast(),
                uframes(frames as u64),
            )
        };
        sframes(n)
    }

    /// [`RawPcm::readi`] for an S32_LE PCM; the caller converts the samples to S16.
    pub(crate) fn readi_s32(
        &mut self,
        buf: &mut [i32],
        frames: usize,
        frame_bytes: usize,
    ) -> Result<usize, i32> {
        if frame_bytes == 0
            || frames
                .checked_mul(frame_bytes)
                .is_none_or(|n| n > size_of_val(buf))
        {
            return Err(-libc::EINVAL);
        }
        // SAFETY: see impl comment; `buf` holds at least `frames` frames (checked above).
        let n = unsafe {
            (self.lib.pcm_readi)(
                self.pcm.as_ptr(),
                buf.as_mut_ptr().cast(),
                uframes(frames as u64),
            )
        };
        sframes(n)
    }

    pub(crate) fn writei(
        &mut self,
        buf: &[u8],
        frames: usize,
        frame_bytes: usize,
    ) -> Result<usize, i32> {
        if frame_bytes == 0
            || frames
                .checked_mul(frame_bytes)
                .is_none_or(|n| n > buf.len())
        {
            return Err(-libc::EINVAL);
        }
        // SAFETY: see impl comment; `buf` holds at least `frames` frames (checked above).
        let n = unsafe {
            (self.lib.pcm_writei)(
                self.pcm.as_ptr(),
                buf.as_ptr().cast(),
                uframes(frames as u64),
            )
        };
        sframes(n)
    }

    pub(crate) fn wait(&mut self, timeout_ms: i32) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.pcm_wait)(self.pcm.as_ptr(), timeout_ms) }
    }

    pub(crate) fn avail_update(&mut self) -> Result<usize, i32> {
        // SAFETY: see impl comment.
        sframes(unsafe { (self.lib.pcm_avail_update)(self.pcm.as_ptr()) })
    }

    pub(crate) fn delay(&mut self) -> Result<i64, i32> {
        let mut d: Sframes = 0;
        // SAFETY: see impl comment.
        let rc = unsafe { (self.lib.pcm_delay)(self.pcm.as_ptr(), &mut d) };
        if rc < 0 { Err(rc) } else { Ok(from_long(d)) }
    }

    pub(crate) fn recover(&mut self, err: i32, silent: bool) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.pcm_recover)(self.pcm.as_ptr(), err, c_int::from(silent)) }
    }
}

#[allow(
    clippy::useless_conversion,
    reason = "long is 32-bit on armhf, 64-bit on aarch64"
)]
fn sframes(n: Sframes) -> Result<usize, i32> {
    if n < 0 {
        Err(i32::try_from(n).unwrap_or(-libc::EIO))
    } else {
        Ok(n as usize)
    }
}

/// An open snd_ctl_t; closed on drop.
pub(crate) struct RawCtl {
    lib: &'static Lib,
    ctl: NonNull<Ctl>,
}

// SAFETY: a snd_ctl_t may be used from any thread, one at a time; RawCtl is not Sync.
unsafe impl Send for RawCtl {}

impl Drop for RawCtl {
    fn drop(&mut self) {
        // SAFETY: opened by snd_ctl_open, closed exactly once.
        unsafe { (self.lib.ctl_close)(self.ctl.as_ptr()) };
    }
}

/// A snd_ctl_elem_value_t naming a mixer element; freed on drop.
pub(crate) struct MixerValue {
    lib: &'static Lib,
    v: NonNull<ElemValue>,
}

impl MixerValue {
    pub(crate) fn new(lib: &'static Lib, name: &CStr) -> Result<Self, i32> {
        let mut v = ptr::null_mut();
        // SAFETY: out-pointer to a local.
        let rc = unsafe { (lib.value_malloc)(&mut v) };
        let v =
            NonNull::new(v)
                .filter(|_| rc >= 0)
                .ok_or(if rc < 0 { rc } else { -libc::ENOMEM })?;
        // SAFETY: freshly allocated value, NUL-terminated name (copied by alsa-lib).
        unsafe {
            (lib.value_set_interface)(v.as_ptr(), CTL_ELEM_IFACE_MIXER);
            (lib.value_set_name)(v.as_ptr(), name.as_ptr());
        }
        Ok(Self { lib, v })
    }

    /// `index` < [`CTL_MAX_INTEGERS`] (checked by the caller; alsa-lib aborts otherwise).
    pub(crate) fn integer(&self, index: usize) -> i64 {
        debug_assert!(index < CTL_MAX_INTEGERS);
        // SAFETY: live value; index within the value's 128 integers.
        from_long(unsafe { (self.lib.value_get_integer)(self.v.as_ptr(), index as c_uint) })
    }

    /// `index` < [`CTL_MAX_INTEGERS`] (checked by the caller; alsa-lib aborts otherwise).
    pub(crate) fn set_integer(&mut self, index: usize, v: i64) {
        debug_assert!(index < CTL_MAX_INTEGERS);
        let v = c_long::try_from(v).unwrap_or(if v < 0 { c_long::MIN } else { c_long::MAX });
        // SAFETY: live value; index within the value's 128 integers.
        unsafe { (self.lib.value_set_integer)(self.v.as_ptr(), index as c_uint, v) };
    }
}

impl Drop for MixerValue {
    fn drop(&mut self) {
        // SAFETY: allocated by snd_ctl_elem_value_malloc, freed once.
        unsafe { (self.lib.value_free)(self.v.as_ptr()) };
    }
}

// SAFETY (all RawCtl methods): `self.ctl` is an open control handle, the element objects are
// live allocations of the same library and are freed before returning.
impl RawCtl {
    pub(crate) fn lib(&self) -> &'static Lib {
        self.lib
    }

    /// snd_ctl_elem_info of the mixer element `name` (0 = it exists).
    pub(crate) fn mixer_info(&self, name: &CStr) -> i32 {
        let lib = self.lib;
        let (mut id, mut info) = (ptr::null_mut(), ptr::null_mut());
        // SAFETY: see impl comment; both objects are freed on every path below.
        unsafe {
            let rc_id = (lib.id_malloc)(&mut id);
            let rc_info = (lib.info_malloc)(&mut info);
            let rc = if rc_id < 0 || id.is_null() {
                rc_id.min(-libc::ENOMEM)
            } else if rc_info < 0 || info.is_null() {
                rc_info.min(-libc::ENOMEM)
            } else {
                (lib.id_set_interface)(id, CTL_ELEM_IFACE_MIXER);
                (lib.id_set_name)(id, name.as_ptr());
                (lib.info_set_id)(info, id);
                (lib.ctl_elem_info)(self.ctl.as_ptr(), info)
            };
            if !info.is_null() {
                (lib.info_free)(info);
            }
            if !id.is_null() {
                (lib.id_free)(id);
            }
            rc
        }
    }

    pub(crate) fn elem_read(&self, v: &mut MixerValue) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.ctl_elem_read)(self.ctl.as_ptr(), v.v.as_ptr()) }
    }

    pub(crate) fn elem_write(&self, v: &mut MixerValue) -> i32 {
        // SAFETY: see impl comment.
        unsafe { (self.lib.ctl_elem_write)(self.ctl.as_ptr(), v.v.as_ptr()) }
    }
}

/// NUL-terminated copy of a device / element name (Err for an interior NUL).
pub(crate) fn c_name(name: &str) -> Result<CString, i32> {
    CString::new(name).map_err(|_| -libc::EINVAL)
}
