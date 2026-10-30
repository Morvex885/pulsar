//! libasound loaded at runtime (the firmware's copy; nothing is linked at build time).
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[allow(unsafe_code)]
mod ffi;

use ffi::{HwParams, MixerValue, RawCtl, RawPcm, SwParams};
use pulsar_core::model::Model;

/// Applies the model's process-wide ALSA settings before any thread or audio library starts:
/// ALSA_CONFIG_PATH and the absolute libasound path (the Mini's firmware keeps it outside the
/// loader's search path). With such a path the library is loaded here, before GStreamer: dlopen
/// with RTLD_GLOBAL makes libquasar_gstreamer's NEEDED `libasound.so.2` resolve to this copy by
/// SONAME whether or not that library carries a RUNPATH (not checked on a Mini). The other models
/// keep the lazy SONAME lookup: an early `libasound.so.2` dlopen from the executable (no RUNPATH)
/// could pick another copy than GStreamer's RUNPATH does. Must be called before the first [`load`].
pub fn init(model: &Model) {
    model.apply_env();
    ffi::set_library_path(model.alsa_lib);
    if !model.alsa_lib.is_empty() {
        // A failed dlopen is logged by ffi::lib.
        let _ = ffi::lib();
    }
}

/// Loads libasound.so.2 once; every other call loads it on demand. Err = dlerror text.
pub fn load() -> Result<(), String> {
    ffi::lib().map(|_| ()).map_err(str::to_owned)
}

/// Whether the library exports the `__snd_pcm_hw_params_*` aliases (versioned alsa-lib).
pub fn has_modern_aliases() -> bool {
    ffi::lib().is_ok_and(ffi::Lib::modern_aliases)
}

/// snd_strerror; without libasound, the C library's text for the errno.
pub fn strerror(err: i32) -> String {
    match ffi::lib() {
        Ok(lib) => lib.strerror(err),
        Err(_) => {
            let s = std::io::Error::from_raw_os_error(err.saturating_neg()).to_string();
            match s.find(" (os error") {
                Some(i) => s[..i].to_owned(),
                None => s,
            }
        }
    }
}

/// Process-wide: alsa-lib's own diagnostics go to the debug log (without their message text).
pub fn quiet_error_handler() {
    if let Ok(lib) = ffi::lib() {
        lib.set_quiet_error_handler();
    }
}

#[derive(Clone, Copy)]
pub enum Stream {
    Playback,
    Capture,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    S16Le,
    S24_3Le,
    S32Le,
}

impl Format {
    fn bytes(self) -> usize {
        match self {
            Format::S16Le => 2,
            Format::S24_3Le => 3,
            Format::S32Le => 4,
        }
    }

    fn raw(self) -> i32 {
        match self {
            Format::S16Le => ffi::FORMAT_S16_LE,
            Format::S24_3Le => ffi::FORMAT_S24_3LE,
            Format::S32Le => ffi::FORMAT_S32_LE,
        }
    }
}

pub struct HwRequest {
    pub format: Format,
    pub channels: u32,
    pub rate: u32,
    /// true: snd_pcm_hw_params_set_rate (fails unless the rate is supported as is);
    /// false: set_rate_near, the chosen rate is reported back.
    pub rate_exact: bool,
    pub period_frames: Option<u64>,
    pub buffer_frames: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HwResult {
    pub rate: u32,
    pub period_frames: u64,
    pub buffer_frames: u64,
}

/// Every value of one [`Pcm::set_hw_params_traced`] run, for alsa_abi_probe.
#[derive(Clone, Copy, Debug)]
pub struct HwTrace {
    /// The request, updated by each set_*_near call that ran (unrequested sizes stay 0).
    pub near: HwResult,
    /// Period and buffer read back after the parameters were applied (0 until then); `rate` is
    /// the applied one.
    pub actual: HwResult,
    /// The failing step ("any", "access", "format", "channels", "rate", "period", "buffer",
    /// "apply", "get period", "get buffer") and its error.
    pub failed: Option<(&'static str, i32)>,
}

pub struct Pcm {
    raw: RawPcm,
    frame_bytes: usize,
}

fn ok(rc: i32) -> Result<(), i32> {
    if rc < 0 { Err(rc) } else { Ok(()) }
}

impl Pcm {
    /// Err = negative ALSA errno.
    pub fn open(name: &str, stream: Stream, nonblock: bool) -> Result<Pcm, i32> {
        let lib = ffi::lib().map_err(|_| -libc::ENOENT)?;
        let name = ffi::c_name(name)?;
        let stream = match stream {
            Stream::Playback => ffi::STREAM_PLAYBACK,
            Stream::Capture => ffi::STREAM_CAPTURE,
        };
        let mode = if nonblock { ffi::PCM_NONBLOCK } else { 0 };
        let raw = lib.pcm_open(&name, stream, mode)?;
        Ok(Pcm {
            raw,
            frame_bytes: 0,
        })
    }

    /// Access RW_INTERLEAVED, then format, channels, rate, period, buffer, apply; returns the
    /// rate and the period / buffer read back from the device.
    pub fn set_hw_params(&mut self, req: &HwRequest) -> Result<HwResult, i32> {
        let t = self.set_hw_params_traced(req);
        match t.failed {
            Some((_, err)) => Err(err),
            None => Ok(t.actual),
        }
    }

    /// [`Pcm::set_hw_params`] with the values each step produced.
    pub fn set_hw_params_traced(&mut self, req: &HwRequest) -> HwTrace {
        let mut t = HwTrace {
            near: HwResult {
                rate: req.rate,
                period_frames: req.period_frames.unwrap_or(0),
                buffer_frames: req.buffer_frames.unwrap_or(0),
            },
            actual: HwResult::default(),
            failed: None,
        };
        if let Err(f) = self.apply_hw(req, &mut t) {
            t.failed = Some(f);
        }
        t
    }

    fn apply_hw(&mut self, req: &HwRequest, t: &mut HwTrace) -> Result<(), (&'static str, i32)> {
        let step = |what: &'static str, rc: i32| ok(rc).map_err(|e| (what, e));
        self.frame_bytes = 0;
        let mut hw = HwParams::new(self.raw.lib()).map_err(|e| ("any", e))?;
        let pcm = &mut self.raw;
        step("any", pcm.hw_any(&mut hw))?;
        step(
            "access",
            pcm.hw_set_access(&mut hw, ffi::ACCESS_RW_INTERLEAVED),
        )?;
        step("format", pcm.hw_set_format(&mut hw, req.format.raw()))?;
        step("channels", pcm.hw_set_channels(&mut hw, req.channels))?;
        if req.rate_exact {
            step("rate", pcm.hw_set_rate(&mut hw, req.rate))?;
        } else {
            step("rate", pcm.hw_set_rate_near(&mut hw, &mut t.near.rate))?;
        }
        if req.period_frames.is_some() {
            step(
                "period",
                pcm.hw_set_period_size_near(&mut hw, &mut t.near.period_frames),
            )?;
        }
        if req.buffer_frames.is_some() {
            step(
                "buffer",
                pcm.hw_set_buffer_size_near(&mut hw, &mut t.near.buffer_frames),
            )?;
        }
        step("apply", pcm.hw_apply(&mut hw))?;
        t.actual.rate = t.near.rate;
        step(
            "get period",
            hw.get_period_size(&mut t.actual.period_frames),
        )?;
        step(
            "get buffer",
            hw.get_buffer_size(&mut t.actual.buffer_frames),
        )?;
        self.frame_bytes = req.format.bytes() * req.channels as usize;
        Ok(())
    }

    pub fn set_sw_params(&mut self, start_threshold: u64, avail_min: u64) -> Result<(), i32> {
        let mut sw = SwParams::new(self.raw.lib())?;
        ok(self.raw.sw_current(&mut sw))?;
        ok(self.raw.sw_set_start_threshold(&mut sw, start_threshold))?;
        ok(self.raw.sw_set_avail_min(&mut sw, avail_min))?;
        ok(self.raw.sw_apply(&mut sw))
    }

    pub fn prepare(&mut self) -> Result<(), i32> {
        ok(self.raw.prepare())
    }

    /// snd_pcm_drop.
    pub fn drop_frames(&mut self) -> Result<(), i32> {
        ok(self.raw.drop_frames())
    }

    pub fn resume(&mut self) -> Result<(), i32> {
        ok(self.raw.resume())
    }

    /// `buf` must hold `frames` frames of the configured S16 format (else -EINVAL).
    pub fn readi(&mut self, buf: &mut [i16], frames: usize) -> Result<usize, i32> {
        self.raw.readi(buf, frames, self.frame_bytes)
    }

    /// `buf` must hold `frames` frames of the configured S32 format (else -EINVAL).
    pub fn readi_s32(&mut self, buf: &mut [i32], frames: usize) -> Result<usize, i32> {
        self.raw.readi_s32(buf, frames, self.frame_bytes)
    }

    /// `buf` must hold `frames` frames of the configured format (else -EINVAL).
    pub fn writei(&mut self, buf: &[u8], frames: usize) -> Result<usize, i32> {
        self.raw.writei(buf, frames, self.frame_bytes)
    }

    /// true = ready, false = timeout.
    pub fn wait(&mut self, timeout_ms: i32) -> Result<bool, i32> {
        let rc = self.raw.wait(timeout_ms);
        ok(rc).map(|()| rc > 0)
    }

    pub fn avail_update(&mut self) -> Result<usize, i32> {
        self.raw.avail_update()
    }

    pub fn delay(&mut self) -> Result<i64, i32> {
        self.raw.delay()
    }

    pub fn recover(&mut self, err: i32, silent: bool) -> Result<(), i32> {
        ok(self.raw.recover(err, silent))
    }
}

/// Writes an integer mixer element on card 0, e.g. the Mini's "SPK mute" switch (1 = on).
/// Opens the control for every call, as the stock firmware does before each playback start.
pub fn set_mixer_integer(name: &str, values: &[i64]) -> Result<(), i32> {
    let ctl = Ctl::open("hw:0")?;
    ctl.write_integers(name, values)
}

/// Switches the model's output on: the sysfs amplifier gates and the ALSA "SPK mute" mixer.
/// Opening a PCM or dmix dropping its last client may switch the gates off again, so this runs
/// both before and after a device is opened.
pub fn enable_output(model: &Model) {
    for path in model.amp_enable_paths {
        if !pulsar_core::sysfs::write_file(path, "1\n") {
            log::warn!("amp: cannot write '{path}'");
        }
    }
    if !model.spk_mute_ctl.is_empty()
        && let Err(e) = set_mixer_integer(model.spk_mute_ctl, &[1])
    {
        log::warn!(
            "spk: cannot unmute '{}': {}",
            model.spk_mute_ctl,
            strerror(e)
        );
    }
}

/// Opens the model's playback PCM with the output gates on. Gates are enabled before the open (a
/// device must never start muted) and again afterwards: dmix turns the amplifier off when its
/// last client closes, so another channel may have closed it between our two calls. All PCM
/// openers in the daemon use this; the MUSIC pipeline, whose alsasink opens the device inside
/// GStreamer, instead calls [`enable_output`] before it starts the pipeline and again when the
/// pipeline reports PLAYING (the device is open by then).
pub fn open_playback(model: &Model) -> Result<Pcm, i32> {
    enable_output(model);
    let pcm = Pcm::open(model.playback_pcm, Stream::Playback, false)?;
    enable_output(model);
    Ok(pcm)
}

pub struct Ctl {
    raw: RawCtl,
}

impl Ctl {
    /// `name`: a card, e.g. "hw:0".
    pub fn open(name: &str) -> Result<Ctl, i32> {
        let lib = ffi::lib().map_err(|_| -libc::ENOENT)?;
        let name = ffi::c_name(name)?;
        Ok(Ctl {
            raw: lib.ctl_open(&name)?,
        })
    }

    /// Whether a mixer element of that name exists.
    pub fn mixer_exists(&self, name: &str) -> bool {
        ffi::c_name(name).is_ok_and(|name| self.raw.mixer_info(&name) == 0)
    }

    /// Integer `index` of mixer element `name`.
    pub fn read_integer(&self, name: &str, index: u32) -> Result<i64, i32> {
        let index = index as usize;
        if index >= ffi::CTL_MAX_INTEGERS {
            return Err(-libc::EINVAL);
        }
        let mut v = MixerValue::new(self.raw.lib(), &ffi::c_name(name)?)?;
        ok(self.raw.elem_read(&mut v))?;
        Ok(v.integer(index))
    }

    /// Writes `values` as integers 0.. of mixer element `name`.
    pub fn write_integers(&self, name: &str, values: &[i64]) -> Result<(), i32> {
        if values.len() > ffi::CTL_MAX_INTEGERS {
            return Err(-libc::EINVAL);
        }
        let mut v = MixerValue::new(self.raw.lib(), &ffi::c_name(name)?)?;
        for (i, &x) in values.iter().enumerate() {
            v.set_integer(i, x);
        }
        ok(self.raw.elem_write(&mut v))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn load_from_missing_library_reports_dlerror() {
        let err = match super::ffi::load_from("libasound-does-not-exist.so.2") {
            Ok(_) => panic!("a missing library loaded"),
            Err(e) => e,
        };
        assert!(err.contains("libasound-does-not-exist.so.2"), "{err}");
        assert!(err.contains("cannot open shared object file"), "{err}");
    }
}
