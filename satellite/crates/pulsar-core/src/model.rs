//! Per-model hardware description of the supported Yandex Stations, detected once from the
//! devicetree ("yandex,<codename>" in /proc/device-tree/compatible).

use std::sync::LazyLock;

#[derive(Debug, PartialEq, Eq)]
pub struct Model {
    /// Devicetree codename: "cucumber", "plum".
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

    /// evdev device names.
    pub keys_input: &'static str,
    pub mute_input: &'static str,
    /// Relative to /system/vendor/quasar/sounds/.
    pub error_sound: &'static str,

    /// Stock volume curve: platform level -> raw value (0..255) of the softvol "Master Playback
    /// Volume" on the speakers PCM, piecewise linear between the points; the first point is
    /// level 0 (raw 0), the last one the top level. quasar.cfg volume_capability
    /// .volume_manager_settings.platformToAlsaMap; the platform level steps by 5 %
    /// (changeStepPercent).
    pub volume_map: &'static [VolumePoint],

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
    /// sysfs numbering starts at 0 (Midi) or 1 (Lite 2).
    pub first_index: i32,
    /// "/dev/fb0"
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
    keys_input: "awinic_sar",
    mute_input: "mute",
    error_sound: "ru-RU/vins_error.wav",
    volume_map: STOCK_VOLUME,
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
    keys_input: "aw9610x",
    mute_input: "gpio-keys",
    error_sound: "ru-RU/playback_error.mp3",
    volume_map: STOCK_VOLUME,
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

/// The model of the first "yandex,cucumber" / "yandex,plum" entry of a NUL-separated
/// compatible list, e.g. "yandex,cucumber-dvt2\0yandex,cucumber\0amlogic, a5\0".
pub fn model_from_compatible(raw: &[u8]) -> Option<&'static Model> {
    raw.split(|&b| b == 0).find_map(|entry| match entry {
        b"yandex,cucumber" => Some(&MIDI),
        b"yandex,plum" => Some(&LITE2),
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
    fn unknown_hardware() {
        assert!(model_from_compatible(b"").is_none());
        assert!(model_from_compatible(b"yandex,cucumber-dvt2\0amlogic, a5\0").is_none());
        assert!(model_from_compatible(b"yandex,yandexmini\0").is_none());
        assert!(model_from_compatible(b"xyandex,plum\0").is_none());
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
