//! Per-model hardware description of the supported Yandex Stations, detected once from the
//! devicetree ("yandex,<codename>" in /proc/device-tree/compatible).

use std::sync::LazyLock;

#[derive(Debug, PartialEq, Eq)]
pub struct Model {
    /// Devicetree codename: "cucumber", "plum", "station-mini2".
    pub codename: &'static str,
    /// ESPHome node name prefix; the MAC suffix is appended.
    pub host_prefix: &'static str,
    /// Shown in Home Assistant / Music Assistant.
    pub friendly_name: &'static str,
    pub model_name: &'static str,

    // Capture: interleaved S16 frames from capture_pcm; VQE gets mic_channels / spk_channels.
    pub capture_pcm: &'static str,
    pub capture_rate: u32,
    pub capture_channels: u32,
    pub mic_channels: &'static [u32],
    pub spk_channels: &'static [u32],
    pub vqe_preset: &'static str,
    /// Pre-roll per wake word model: audio between the end of the wake word and its
    /// confirmation, sent ahead of the request. Ids not listed get none.
    pub wake_preroll: &'static [WakePreroll],

    /// Prefix of the per-channel sysfs LEDs "<led_prefix>:<color>:<ring|logo><index>".
    pub led_prefix: &'static str,
    pub ring: LedStrip,
    /// `logo.count == 0`: no logo.
    pub logo: LedStrip,

    // 4-digit 7-segment clock: "<clock_dir><segment><position>" sysfs LEDs.
    /// e.g. "/sys/class/leds/cucumber:white:clock-"
    pub clock_dir: &'static str,
    /// Name of segment a ('a' or 'A').
    pub clock_seg_base: char,
    /// Name of the leftmost digit position.
    pub clock_pos_base: i32,
    /// Colon LEDs.
    pub clock_colon: &'static [&'static str],
    /// LEDs kept dark.
    pub clock_aux: &'static [&'static str],
    /// LED at g height between digits 1 and 2 ("" = none).
    pub clock_center: &'static str,
    /// true: segments switch 0/255, dimming via the driver chips' "current" attribute;
    /// false: dimming via the per-segment brightness value itself.
    pub clock_chip_current: bool,
    /// false: the station may lack the clock panel; its HA entities then exist only when
    /// `pulsar_clock::panel_present` finds a segment at start, and a missing panel is logged at
    /// info.
    pub has_clock: bool,

    /// evdev device names; `mute_input` "" = no switch device (the Mini 2's mute is a GPIO).
    pub keys_input: &'static str,
    pub mute_input: &'static str,
    /// Mic mute on a sysfs GPIO instead of an evdev switch: global GPIO number, 0 = none; active
    /// low, exported if needed but never configured (used only when already an input).
    pub mute_gpio: u32,
    /// Earcons, relative to /system/vendor/quasar/sounds/.
    pub error_sound: &'static str,
    pub wake_sound: &'static str,
    pub cancel_sound: &'static str,
    pub timer_sound: &'static str,
    /// maind plays it (sound 45, SimpleVolumeTonePlayer) for every change except those from its
    /// muted sources "other" (app / remote) and "smart_tts" (smart volume); keys and voice words
    /// click.
    pub volume_sound: &'static str,

    /// ALSA playback PCM (dmix + the softvol "Master Playback Volume"): GStreamer's alsasink,
    /// the Sendspin sink, and the PCM the mixer opens to create the control.
    pub playback_pcm: &'static str,
    /// Stock volume curve: platform level -> raw value (0..255) of the softvol "Master Playback
    /// Volume" on `playback_pcm`, piecewise linear between the points; the first point is
    /// level 0 (raw 0), the last one the top level. quasar.cfg volume_capability
    /// .volume_manager_settings.platformToAlsaMap. One platform level is 5 % on the 20-level maps;
    /// maind's Alice volume (0..10, "громче" / "тише") is platform level / 2 rounded half up.
    /// quasar.cfg changeStepPercent is the step of maind's gradual volume change
    /// (start_volume_change), not of the keys or the voice words.
    pub volume_map: &'static [VolumePoint],
    /// true: any audible volume is at least the map's second point (the stock firmware never
    /// sets the raw values below it).
    pub volume_min_step: bool,
    /// false: no ambient light sensor (no Illuminance entity, the sensor is not opened).
    pub has_lux: bool,

    /// Zigbee radio: Telink co-processor on a UART; its reset and boot lines are GPIO value
    /// files (active low, 1 = asserted) exported by the kernel's zb_control driver.
    /// `zigbee_tty` empty: none.
    pub zigbee_tty: &'static str,
    pub zigbee_reset: &'static str,
    pub zigbee_boot: &'static str,
}

/// An Nx1 BGR framebuffer behind an ledfb trigger, with per-channel sysfs LEDs as fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedStrip {
    pub count: i32,
    /// sysfs numbering starts at 0 (Midi) or 1 (Lite 2, Mini 2).
    pub first_index: i32,
    /// "/dev/fb0"; "" = none: sysfs LEDs only.
    pub fb_dev: &'static str,
    /// Expected /sys/class/graphics/fbN/name.
    pub fb_name: &'static str,
    /// LED trigger that routes the framebuffer to the LEDs.
    pub trigger: &'static str,
    /// "ring" / "logo"
    pub sysfs_kind: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumePoint {
    pub level: i32,
    pub raw: i32,
}

/// Pre-roll of one wake word model (`spotter_models/activation/<id>`), in 32 ms VQE chunks
/// ending with the confirming chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakePreroll {
    pub id: &'static str,
    pub rule: PrerollRule,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrerollRule {
    /// (chunks since the 1st-stage hit) + `offset`, at least 0; `fallback` chunks when no
    /// 1st-stage hit preceded the confirmation.
    FirstStage { offset: i32, fallback: u32 },
    /// Always this many chunks.
    Fixed(u32),
}

// Midi, frontend_probe 2026-10-28, one talker at 1-2 m: "alisa" confirms with event 5 and the
// 1st-stage hit (event 3) always comes in the same chunk, so it anchors nothing; "yandex"
// confirms with event 2 and never reports event 3. Both get a fixed length. Words reach
// VQE_OMNI at -54..-63 dB, near the noise floor, so the end of an isolated word is often not
// found: confirmation 0..5 chunks after it, mostly 2..3 ("alisa", 10 words), 0..3 ("yandex",
// 11 words). Readings this close to the noise floor are unreliable, so the typical reading is
// used instead of the shortest; checked through HA (see the PR).
const CUCUMBER_PREROLL: &[WakePreroll] = &[
    WakePreroll {
        id: "alisa",
        rule: PrerollRule::Fixed(2),
    },
    WakePreroll {
        id: "yandex",
        rule: PrerollRule::Fixed(2),
    },
];
// Lite 2, frontend_probe 2026-10-28, one talker at 1-2 m: both models confirm with event 2
// (1st stage) and never report a 1st-stage hit (event 3), so the length is fixed. Chunks from
// the end of an isolated word (VQE_OMNI 20 dB below its peak) to the confirmation, over words
// louder than -55 dB (quieter ones have their noise floor within 20 dB): "alisa" 3..8,
// "yandex" 4..9. The shortest one is used, so the word does not reach STT. Through HA,
// "yandex" with no pre-roll lost the start of the command ("который час" -> "час").
// The Mini 2 ships the Lite 2's spotter models and uses these too (not measured on it).
const PLUM_PREROLL: &[WakePreroll] = &[
    WakePreroll {
        id: "alisa",
        rule: PrerollRule::Fixed(3),
    },
    WakePreroll {
        id: "yandex",
        rule: PrerollRule::Fixed(4),
    },
];

const NO_STRIP: LedStrip = LedStrip {
    count: 0,
    first_index: 0,
    fb_dev: "",
    fb_name: "",
    trigger: "",
    sysfs_kind: "",
};

// { "0": 0, "1": 49, "20": 255 }: softvol min_dB -60, so 2.6 dB per level, level 1 at -48.5 dB.
// The Lite 2's quasar.cfg has the same map.
const STOCK_VOLUME: &[VolumePoint] = &[
    VolumePoint { level: 0, raw: 0 },
    VolumePoint { level: 1, raw: 49 },
    VolumePoint {
        level: 20,
        raw: 255,
    },
];

/// Station Midi: aarch64, 4 mics + 4 speaker reference channels at 96 kHz, 24-LED ring + 6-LED
/// logo, clock on two aw21036/aw21012 chips.
static MIDI: Model = Model {
    codename: "cucumber",
    host_prefix: "yandex-midi-",
    friendly_name: "Яндекс Станция Миди",
    model_name: "Yandex Station Midi",
    capture_pcm: "hw:0,2",
    capture_rate: 96000,
    capture_channels: 8,
    mic_channels: &[0, 1, 2, 3],
    spk_channels: &[4, 5, 6, 7],
    vqe_preset: "cucumber",
    wake_preroll: CUCUMBER_PREROLL,
    led_prefix: "cucumber",
    ring: LedStrip {
        count: 24,
        first_index: 0,
        fb_dev: "/dev/fb0",
        fb_name: "ring",
        trigger: "ledfb-ring",
        sysfs_kind: "ring",
    },
    logo: LedStrip {
        count: 6,
        first_index: 0,
        fb_dev: "/dev/fb1",
        fb_name: "logo",
        trigger: "ledfb-logo",
        sysfs_kind: "logo",
    },
    clock_dir: "/sys/class/leds/cucumber:white:clock-",
    clock_seg_base: 'a',
    clock_pos_base: 1,
    // p3 + q3 form the colon; the rest are auxiliary dots/icons maind never lights for the time.
    clock_colon: &["p3", "q3"],
    clock_aux: &[
        "deg", "h1", "h2", "l1", "l2", "m1", "m2", "p1", "p2", "p4", "p5", "q1", "q2", "q4", "q5",
    ],
    clock_center: "",
    clock_chip_current: true,
    has_clock: true,
    keys_input: "awinic_sar",
    mute_input: "mute",
    mute_gpio: 0,
    error_sound: "ru-RU/vins_error.wav",
    wake_sound: "common/spotter_start.mp3",
    cancel_sound: "common/spotter_cancel.mp3",
    timer_sound: "common/timer.wav",
    volume_sound: "common/volume_sound.mp3",
    playback_pcm: "speakers",
    volume_map: STOCK_VOLUME,
    volume_min_step: true,
    has_lux: true,
    // Telink B91 (quasar.cfg zigbee.tty.port). Reset and boot are GPIOZ_11 / GPIOZ_9
    // (devicetree node "zigbee", driver zb_control); maind drives them through these files when
    // quasar.cfg zigbee.zbControl is set, as it is on this firmware.
    zigbee_tty: "/dev/ttyS2",
    zigbee_reset: "/sys/devices/platform/zigbee/zb-reset/value",
    zigbee_boot: "/sys/devices/platform/zigbee/zb-boot/value",
};

/// Station Lite 2: 32-bit userland, 2 PDM mics (capture channels 0 and 2) + speaker feedback
/// (channel 6) at 48 kHz on hw:0,1 (layout of multi_capture_raw in /etc/asound.conf), 3-LED
/// ring, clock LEDs "plum:white:clock<A..H><0..3>" + DH/DL (colon), DM, DEGREE (quasar.cfg
/// /leds/clock).
static LITE2: Model = Model {
    codename: "plum",
    host_prefix: "yandex-lite2-",
    friendly_name: "Яндекс Станция Лайт 2",
    model_name: "Yandex Station Lite 2",
    capture_pcm: "hw:0,1",
    capture_rate: 48000,
    capture_channels: 8,
    mic_channels: &[0, 2],
    spk_channels: &[6],
    vqe_preset: "plum",
    wake_preroll: PLUM_PREROLL,
    led_prefix: "plum",
    ring: LedStrip {
        count: 3,
        first_index: 1,
        fb_dev: "/dev/fb0",
        fb_name: "YNDX LED ring",
        trigger: "led_ring_fb",
        sysfs_kind: "ring",
    },
    logo: NO_STRIP,
    clock_dir: "/sys/class/leds/plum:white:clock",
    clock_seg_base: 'A',
    clock_pos_base: 0,
    clock_colon: &["DH", "DL"],
    clock_aux: &["H0", "H1", "H2", "H3", "DEGREE"],
    // Centre dot of the stock voice patterns (their extra LED 35).
    clock_center: "DM",
    clock_chip_current: false,
    has_clock: true,
    keys_input: "aw9610x",
    mute_input: "gpio-keys",
    mute_gpio: 0,
    error_sound: "ru-RU/playback_error.mp3",
    wake_sound: "common/spotter_start.mp3",
    cancel_sound: "common/spotter_cancel.mp3",
    timer_sound: "common/timer.wav",
    volume_sound: "common/volume_sound.mp3",
    playback_pcm: "speakers",
    volume_map: STOCK_VOLUME,
    volume_min_step: true,
    has_lux: true,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

// set_volume.sh on the Mini 2 is `amixer set 'Master' <percent>%`: raw = percent of 255, no dB
// offset and no lowest step.
const MINI2_VOLUME: &[VolumePoint] = &[
    VolumePoint { level: 0, raw: 0 },
    VolumePoint {
        level: 20,
        raw: 255,
    },
];

/// Station Mini 2: Amlogic AXG, 32-bit armhf userland on an aarch64 kernel. Capture is the
/// 6-channel "default" multi device of /etc/asound.conf: 4 PDM mics on channels 0..3 (hw:0,1
/// channels 0/2/4/6) and the 2 TDM amplifier loopback channels on 4/5 (hw:0,0). Playback goes to
/// "default" (softvol "Master" over dmix_spk_plug); there is no "speakers" PCM. The ring is 6 RGB
/// LEDs of an IS31FL3236 without an ledfb trigger (sysfs brightness only). The clock is wired as
/// "mini2:white:clock<A..G><1..4>" + PLUS1/PLUS2 (colon), DEGREE/MINUS (quasar.cfg /leds/clock),
/// but its IS31FL3236 (i2c 0-003f) does not probe on firmware 0.369. No light sensor. Mic mute
/// has no evdev switch: the button is momentary and the mic circuit latches the mute on GPIOZ_3.
static MINI2: Model = Model {
    codename: "station-mini2",
    host_prefix: "yandex-mini2-",
    friendly_name: "Яндекс Станция Мини 2",
    model_name: "Yandex Station Mini 2",
    capture_pcm: "default",
    capture_rate: 48000,
    capture_channels: 6,
    mic_channels: &[0, 1, 2, 3],
    spk_channels: &[4, 5],
    vqe_preset: "yandexmini_2",
    wake_preroll: PLUM_PREROLL,
    led_prefix: "mini2",
    ring: LedStrip {
        count: 6,
        first_index: 1,
        fb_dev: "",
        fb_name: "",
        trigger: "",
        sysfs_kind: "ring",
    },
    logo: NO_STRIP,
    clock_dir: "/sys/class/leds/mini2:white:clock",
    clock_seg_base: 'A',
    clock_pos_base: 1,
    clock_colon: &["PLUS1", "PLUS2"],
    clock_aux: &["DEGREE", "MINUS"],
    clock_center: "",
    clock_chip_current: false,
    has_clock: false,
    keys_input: "sar0_pad0",
    mute_input: "",
    // GPIOZ_3: pull-up, held low by the mic circuit while the microphones are cut (maind polls
    // it); GPIOZ_8 only pulses during a press.
    mute_gpio: 414,
    // This firmware's GStreamer has no MP3 decoder; every earcon ships as .opus.
    error_sound: "ru-RU/playback_error.opus",
    wake_sound: "common/spotter_start.opus",
    cancel_sound: "common/spotter_cancel.opus",
    timer_sound: "common/timer.opus",
    volume_sound: "common/volume_sound.opus",
    playback_pcm: "default",
    volume_map: MINI2_VOLUME,
    volume_min_step: false,
    has_lux: false,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

/// The model of this device, read once; None (logged) on unsupported hardware.
pub fn detect_model() -> Option<&'static Model> {
    static MODEL: LazyLock<Option<&'static Model>> = LazyLock::new(|| {
        let raw = std::fs::read("/proc/device-tree/compatible").unwrap_or_default();
        let model = model_from_compatible(&raw);
        if model.is_none() {
            let shown = String::from_utf8_lossy(&raw).replace('\0', " ");
            log::error!("unsupported device (devicetree compatible: {shown})");
        }
        model
    });
    *MODEL
}

/// The model of the first "yandex,cucumber" / "yandex,plum" / "yandex,station-mini2" entry of a
/// NUL-separated compatible list, e.g. "yandex,cucumber-dvt2\0yandex,cucumber\0amlogic, a5\0".
pub fn model_from_compatible(raw: &[u8]) -> Option<&'static Model> {
    raw.split(|&b| b == 0).find_map(|entry| match entry {
        b"yandex,cucumber" => Some(&MIDI),
        b"yandex,plum" => Some(&LITE2),
        b"yandex,station-mini2" => Some(&MINI2),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cucumber_is_midi() {
        let m =
            model_from_compatible(b"yandex,cucumber-dvt2\0yandex,cucumber\0amlogic, a5\0").unwrap();
        assert_eq!(m.codename, "cucumber");
        assert_eq!(m.capture_rate, 96000);
        assert_eq!(m.logo.count, 6);
        assert_eq!(m.zigbee_tty, "/dev/ttyS2");
    }

    #[test]
    fn plum_is_lite2() {
        let m = model_from_compatible(b"yandex,plum\0amlogic, axg\0").unwrap();
        assert_eq!(m.codename, "plum");
        assert_eq!(m.ring.first_index, 1);
        assert_eq!(m.logo.count, 0);
        assert_eq!(m.zigbee_tty, "");
        // No trailing NUL: the last entry still counts.
        assert_eq!(
            model_from_compatible(b"amlogic, axg\0yandex,plum")
                .unwrap()
                .codename,
            "plum"
        );
    }

    #[test]
    fn station_mini2_is_mini2() {
        let m = model_from_compatible(b"yandex,station-mini2\0amlogic, axg\0").unwrap();
        assert_eq!(m.codename, "station-mini2");
        assert_eq!(m.playback_pcm, "default");
        assert_eq!(m.mute_input, "");
        assert_ne!(m.mute_gpio, 0);
        assert!(!m.has_clock && !m.has_lux && !m.volume_min_step);
    }

    #[test]
    fn unknown_hardware() {
        assert!(model_from_compatible(b"").is_none());
        assert!(model_from_compatible(b"yandex,cucumber-dvt2\0amlogic, a5\0").is_none());
        assert!(model_from_compatible(b"yandex,yandexmini\0").is_none());
        assert!(model_from_compatible(b"xyandex,plum\0").is_none());
        // The old installer pattern cut "yandex,station-mini2" at the hyphen.
        assert!(model_from_compatible(b"yandex,station\0").is_none());
    }

    #[test]
    fn first_known_entry_wins() {
        assert_eq!(
            model_from_compatible(b"yandex,plum\0yandex,cucumber\0")
                .unwrap()
                .codename,
            "plum"
        );
        assert_eq!(
            model_from_compatible(b"yandex,cucumber\0yandex,plum\0")
                .unwrap()
                .codename,
            "cucumber"
        );
    }
}
