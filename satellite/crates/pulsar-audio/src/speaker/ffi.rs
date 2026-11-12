//! C API libtflite.so из прошивки через dlopen: символы резолвятся один раз в [`TfLite`], обёртка
//! [`Interp`] — единственный вызывающий этих указателей.

use std::ffi::{CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::LazyLock;

use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW, Symbol};

const TFLITE: &str = "/system/vendor/quasar/libs/libtflite.so";
// "custom-opresolver-tag" в конфиге модели. В этой сборке библиотеки у TfLiteInterpreterCreate
// нулевой резолвер (падает на создании), операторы даёт только резолвер из её реестра по тегу.
const RESOLVER_TAG: &std::ffi::CStr = c"next";

type P = *mut c_void;

#[derive(Clone, Copy)]
struct TfLite {
    model_create: unsafe extern "C" fn(*const c_char) -> P,
    model_delete: unsafe extern "C" fn(P),
    opts_create: unsafe extern "C" fn() -> P,
    opts_threads: unsafe extern "C" fn(P, i32),
    opts_delete: unsafe extern "C" fn(P),
    resolver_create: unsafe extern "C" fn(*const c_char) -> P,
    resolver_delete: unsafe extern "C" fn(P),
    interp_create: unsafe extern "C" fn(P, P, P) -> P,
    interp_delete: unsafe extern "C" fn(P),
    resize: unsafe extern "C" fn(P, i32, *const c_int, i32) -> c_int,
    allocate: unsafe extern "C" fn(P) -> c_int,
    invoke: unsafe extern "C" fn(P) -> c_int,
    input: unsafe extern "C" fn(P, i32) -> P,
    output: unsafe extern "C" fn(P, i32) -> P,
    copy_from: unsafe extern "C" fn(P, *const c_void, usize) -> c_int,
    copy_to: unsafe extern "C" fn(P, *mut c_void, usize) -> c_int,
}

/// # Safety
/// `T` — точный прототип символа `name`, а `lib` не выгружается до конца процесса.
unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
    // SAFETY: условие функции.
    let s: Symbol<T> = unsafe { lib.get(name.as_bytes()) }.map_err(|e| format!("{name}: {e}"))?;
    Ok(*s)
}

fn load() -> Result<TfLite, String> {
    static LIB: LazyLock<Result<TfLite, String>> = LazyLock::new(|| {
        // SAFETY: инициализаторы библиотеки не имеют предусловий; она не выгружается.
        let lib = unsafe { Library::open(Some(TFLITE), RTLD_NOW | RTLD_GLOBAL) }
            .map_err(|e| e.to_string())?;
        // SAFETY: типы — C API TensorFlow Lite (tensorflow/lite/c/c_api.h) и, для резолвера,
        // прототипы по дизассемблеру библиотеки: Create(tag), Delete(resolver),
        // CreateWithCustomResolver(model, resolver, options); lib не выгружается: into_raw ниже.
        let api = unsafe {
            TfLite {
                model_create: sym(&lib, "TfLiteModelCreateFromFile")?,
                model_delete: sym(&lib, "TfLiteModelDelete")?,
                opts_create: sym(&lib, "TfLiteInterpreterOptionsCreate")?,
                opts_threads: sym(&lib, "TfLiteInterpreterOptionsSetNumThreads")?,
                opts_delete: sym(&lib, "TfLiteInterpreterOptionsDelete")?,
                resolver_create: sym(&lib, "TfLiteCustomOpResolverCreate")?,
                resolver_delete: sym(&lib, "TfLiteCustomOpResolverDelete")?,
                interp_create: sym(&lib, "TfLiteInterpreterCreateWithCustomResolver")?,
                interp_delete: sym(&lib, "TfLiteInterpreterDelete")?,
                resize: sym(&lib, "TfLiteInterpreterResizeInputTensor")?,
                allocate: sym(&lib, "TfLiteInterpreterAllocateTensors")?,
                invoke: sym(&lib, "TfLiteInterpreterInvoke")?,
                input: sym(&lib, "TfLiteInterpreterGetInputTensor")?,
                output: sym(&lib, "TfLiteInterpreterGetOutputTensor")?,
                copy_from: sym(&lib, "TfLiteTensorCopyFromBuffer")?,
                copy_to: sym(&lib, "TfLiteTensorCopyToBuffer")?,
            }
        };
        let _handle = lib.into_raw();
        Ok(api)
    });
    LIB.clone()
}

/// Модель и интерпретатор с одним float-входом и одним float-выходом; живут на одном потоке.
pub(super) struct Interp {
    api: TfLite,
    model: NonNull<c_void>,
    // Живёт дольше интерпретатора: тот хранит указатели на его операторы.
    resolver: NonNull<c_void>,
    interp: NonNull<c_void>,
}

impl Interp {
    pub(super) fn open(path: &Path) -> Result<Interp, String> {
        let api = load()?;
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: c_path — строка с NUL, живёт весь вызов.
        let model = NonNull::new(unsafe { (api.model_create)(c_path.as_ptr()) })
            .ok_or_else(|| format!("cannot load {}", path.display()))?;
        // SAFETY: тег — статическая строка с NUL.
        let Some(resolver) = NonNull::new(unsafe { (api.resolver_create)(RESOLVER_TAG.as_ptr()) })
        else {
            // SAFETY: создан выше, удаляется один раз.
            unsafe { (api.model_delete)(model.as_ptr()) };
            return Err("no TFLite op resolver".into());
        };
        // SAFETY: без предусловий; null проверяется ниже.
        let opts = unsafe { (api.opts_create)() };
        if !opts.is_null() {
            // Как в конфиге модели прошивки: "tflite-num-threads": 1.
            // SAFETY: opts — живой объект из opts_create.
            unsafe { (api.opts_threads)(opts, 1) };
        }
        // SAFETY: model и resolver живы; opts жив или null, что API допускает; опции копируются.
        let interp =
            NonNull::new(unsafe { (api.interp_create)(model.as_ptr(), resolver.as_ptr(), opts) });
        if !opts.is_null() {
            // SAFETY: создан выше, удаляется один раз.
            unsafe { (api.opts_delete)(opts) };
        }
        let Some(interp) = interp else {
            // SAFETY: созданы выше, удаляются один раз.
            unsafe {
                (api.resolver_delete)(resolver.as_ptr());
                (api.model_delete)(model.as_ptr());
            }
            return Err(format!("no interpreter for {}", path.display()));
        };
        Ok(Interp {
            api,
            model,
            resolver,
            interp,
        })
    }

    /// Прогоняет `input` (форма `dims`) и пишет выход в `out`. Размеры проверяет сам TFLite:
    /// копирование с неверным байтовым размером возвращает ошибку.
    pub(super) fn run(
        &mut self,
        dims: &[i32],
        input: &[f32],
        out: &mut [f32],
    ) -> Result<(), String> {
        let n_dims = i32::try_from(dims.len()).map_err(|e| e.to_string())?;
        let it = self.interp.as_ptr();
        let ok = |step: &str, status: c_int| {
            if status == 0 {
                Ok(())
            } else {
                Err(format!("{step}: status {status}"))
            }
        };
        // SAFETY: it жив; dims и буферы живут весь вызов, их длины в байтах переданы вместе с
        // указателями; указатели тензоров действительны до следующего resize/allocate.
        unsafe {
            ok("resize", (self.api.resize)(it, 0, dims.as_ptr(), n_dims))?;
            ok("allocate", (self.api.allocate)(it))?;
            let t_in = (self.api.input)(it, 0);
            if t_in.is_null() {
                return Err("no input tensor".into());
            }
            ok(
                "input",
                (self.api.copy_from)(t_in, input.as_ptr().cast(), size_of_val(input)),
            )?;
            ok("invoke", (self.api.invoke)(it))?;
            let t_out = (self.api.output)(it, 0);
            if t_out.is_null() {
                return Err("no output tensor".into());
            }
            ok(
                "output",
                (self.api.copy_to)(t_out, out.as_mut_ptr().cast(), size_of_val(out)),
            )
        }
    }
}

impl Drop for Interp {
    fn drop(&mut self) {
        // SAFETY: объекты созданы в open и удаляются один раз, интерпретатор — первым.
        unsafe {
            (self.api.interp_delete)(self.interp.as_ptr());
            (self.api.resolver_delete)(self.resolver.as_ptr());
            (self.api.model_delete)(self.model.as_ptr());
        }
    }
}
