//! Per-model hardware description of the supported Yandex Stations, detected once from the
//! devicetree ("yandex,<codename>" in /proc/device-tree/compatible, with the Mini's
//! /proc/device-tree/amlogic-dt-id fallback).

use std::sync::LazyLock;

#[derive(Debug, PartialEq, Eq)]
pub struct Model {
    /// Devicetree codename: "cucumber", "plum", "station-mini2", "bergamot", "yandexmini".
    pub codename: &'static str,
    /// ESPHome node name prefix; the MAC suffix is appended.
    pub host_prefix: &'static str,
    /// Shown in Home Assistant / Music Assistant.
    pub friendly_name: &'static str,
    pub model_name: &'static str,

    // Capture: interleaved samples from capture_pcm, capture_format per channel (S16_LE or S32_LE);
    // VQE gets mic_channels / spk_channels.
    pub capture_pcm: &'static str,
    pub capture_rate: u32,
    pub capture_channels: u32,
    pub capture_format: CaptureFormat,
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

    /// evdev device names; `mute_input` "" = no switch device.
    pub keys_input: &'static str,
    pub mute_input: &'static str,
    /// Mic mute on a sysfs GPIO instead of an evdev switch: global GPIO number, 0 = none; active
    /// low, exported if needed but never configured (used only when already an input).
    pub mute_gpio: u32,
    /// Key code of the action/TALK button (KEY_VOICECOMMAND 582); 0: no action button.
    pub action_key: u16,
    /// Key code of a momentary microphone-mute button on `keys_input` that toggles the software
    /// mute (the Mini's KEY_MUTE 113); 0: none.
    pub mute_key: u16,
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
    /// ALSA config selected through ALSA_CONFIG_PATH before any thread starts (the Mini's dmix);
    /// empty: the firmware's /etc/asound.conf.
    pub alsa_config: &'static str,
    /// Absolute libasound path for `pulsar-alsa` (the Mini keeps it outside the loader path);
    /// empty: the usual "libasound.so.2" lookup.
    pub alsa_lib: &'static str,
    pub software_volume: bool,
    /// ALSA playback switch (on hw:0) that must be 1 (on) for the amplifier to pass audio; empty:
    /// no such control.
    pub spk_mute_ctl: &'static str,
    /// Sysfs switches that gate the speaker amplifier; written to "1" before a playback PCM is
    /// opened. Empty: no gate.
    pub amp_enable_paths: &'static [&'static str],

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
    /// false: the station may lack the ambient light sensor; the Illuminance entity then exists
    /// only when the sensor opens at start, and a missing sensor is logged at info.
    pub has_lux: bool,

    /// Zigbee radio: Telink co-processor on a UART; its reset and boot lines are GPIO value
    /// files (active low, 1 = asserted) exported by the kernel's zb_control driver.
    /// `zigbee_tty` empty: none.
    pub zigbee_tty: &'static str,
    pub zigbee_reset: &'static str,
    pub zigbee_boot: &'static str,
}

/// Sample format of the capture PCM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureFormat {
    S16,
    /// S32_LE with right-justified 24 significant bits; `shift` low bits are discarded to get
    /// the VQE's 16-bit input (8 on the Mini). A full 32-bit sample would use 16.
    S32 {
        shift: u32,
    },
}

impl CaptureFormat {
    pub fn bits(self) -> u32 {
        match self {
            Self::S16 => 16,
            Self::S32 { .. } => 32,
        }
    }
}

/// An Nx1 BGR framebuffer behind an ledfb trigger, with per-channel sysfs LEDs as fallback, or an
/// IS31FL323x I2C controller (18 channels = 6 RGB pixels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedStrip {
    pub count: i32,
    /// sysfs numbering starts at 0 (Midi, Mini 3) or 1 (Lite 2, Mini 2).
    pub first_index: i32,
    /// "/dev/fb0"; "" = none: sysfs LEDs only.
    pub fb_dev: &'static str,
    /// Expected /sys/class/graphics/fbN/name.
    pub fb_name: &'static str,
    /// LED trigger that routes the framebuffer to the LEDs.
    pub trigger: &'static str,
    /// "ring" / "logo"
    pub sysfs_kind: &'static str,
    /// "/dev/i2c-0"; empty: framebuffer/sysfs.
    pub i2c_dev: &'static str,
    pub i2c_addr: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumePoint {
    pub level: i32,
    pub raw: i32,
}

/// Pre-roll of one wake word model (`spotter_models/activation/<id>`), in VQE chunks (32 ms;
/// 16 ms on the Mini 3 and Mini) ending with the confirming chunk.
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
// Mini 3: not measured. Its VQE chunk is 16 ms, so these are the Lite 2's lengths in time
// (96 ms "alisa", 128 ms "yandex").
const BERGAMOT_PREROLL: &[WakePreroll] = &[
    WakePreroll {
        id: "alisa",
        rule: PrerollRule::Fixed(6),
    },
    WakePreroll {
        id: "yandex",
        rule: PrerollRule::Fixed(8),
    },
];

const NO_STRIP: LedStrip = LedStrip {
    count: 0,
    first_index: 0,
    fb_dev: "",
    fb_name: "",
    trigger: "",
    sysfs_kind: "",
    i2c_dev: "",
    i2c_addr: 0,
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
    capture_format: CaptureFormat::S16,
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
        i2c_dev: "",
        i2c_addr: 0,
    },
    logo: LedStrip {
        count: 6,
        first_index: 0,
        fb_dev: "/dev/fb1",
        fb_name: "logo",
        trigger: "ledfb-logo",
        sysfs_kind: "logo",
        i2c_dev: "",
        i2c_addr: 0,
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
    action_key: 582,
    mute_key: 0,
    error_sound: "ru-RU/vins_error.wav",
    wake_sound: "common/spotter_start.mp3",
    cancel_sound: "common/spotter_cancel.mp3",
    timer_sound: "common/timer.wav",
    volume_sound: "common/volume_sound.mp3",
    playback_pcm: "speakers",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    spk_mute_ctl: "",
    amp_enable_paths: &[],
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
    capture_format: CaptureFormat::S16,
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
        i2c_dev: "",
        i2c_addr: 0,
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
    action_key: 582,
    mute_key: 0,
    error_sound: "ru-RU/playback_error.mp3",
    wake_sound: "common/spotter_start.mp3",
    cancel_sound: "common/spotter_cancel.mp3",
    timer_sound: "common/timer.wav",
    volume_sound: "common/volume_sound.mp3",
    playback_pcm: "speakers",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    spk_mute_ctl: "",
    amp_enable_paths: &[],
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
    capture_format: CaptureFormat::S16,
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
        i2c_dev: "",
        i2c_addr: 0,
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
    action_key: 582,
    mute_key: 0,
    // This firmware's GStreamer has no MP3 decoder; every earcon ships as .opus.
    error_sound: "ru-RU/playback_error.opus",
    wake_sound: "common/spotter_start.opus",
    cancel_sound: "common/spotter_cancel.opus",
    timer_sound: "common/timer.opus",
    volume_sound: "common/volume_sound.opus",
    playback_pcm: "default",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    spk_mute_ctl: "",
    amp_enable_paths: &[],
    volume_map: MINI2_VOLUME,
    volume_min_step: false,
    // ltr308als01 seen on the #38 station (firmware 0.372), absent on the #9 one (0.369).
    has_lux: false,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

/// Station Mini (first generation): 32-bit userland, 4 PDM mics (capture channels 0..3) and two
/// speaker references (channels 6 and 7) at 48 kHz on hw:0,2. Channels 4/5 carry a large DC offset
/// (the "micUnknownChannels" of quasar.cfg) and are not fed to the VQE. Playback is hw:0,0: it is
/// the only output with the speaker loopback on channels 6/7. The ring is six RGB LEDs behind an
/// IS31FL3236-compatible controller at i2c-0/0x3c (no framebuffer, no clock).
static MINI: Model = Model {
    codename: "yandexmini",
    host_prefix: "yandex-mini-",
    friendly_name: "Яндекс Станция Мини",
    model_name: "Yandex Station Mini",
    capture_pcm: "hw:0,2",
    capture_rate: 48000,
    capture_channels: 8,
    // Right-justified 24-bit samples: keep the top 16 bits as int16.
    capture_format: CaptureFormat::S32 { shift: 8 },
    mic_channels: &[0, 1, 2, 3],
    spk_channels: &[6, 7],
    vqe_preset: "yandexmini",
    // No pre-roll measurements on the Mini: an id without a rule gives an empty pre-roll.
    wake_preroll: &[],
    led_prefix: "",
    ring: LedStrip {
        count: 6,
        first_index: 0,
        fb_dev: "",
        fb_name: "",
        trigger: "",
        sysfs_kind: "",
        i2c_dev: "/dev/i2c-0",
        i2c_addr: 0x3c,
    },
    logo: NO_STRIP,
    clock_dir: "",
    clock_seg_base: 'a',
    clock_pos_base: 1,
    clock_colon: &[],
    clock_aux: &[],
    clock_center: "",
    clock_chip_current: false,
    has_clock: false,
    keys_input: "adc_keypad",
    mute_input: "",
    mute_gpio: 0,
    // The retail Mini has no TALK pad and no volume keys: adc_keypad sends only KEY_MUTE.
    action_key: 0,
    mute_key: 113,
    error_sound: "ru-RU/playback_error.mp3",
    wake_sound: "common/spotter_start.mp3",
    cancel_sound: "common/spotter_cancel.mp3",
    timer_sound: "common/timer.wav",
    volume_sound: "common/volume_sound.mp3",
    // dmix over hw:0,0; defined in /data/pulsar/asound.conf and selected via ALSA_CONFIG_PATH.
    playback_pcm: "mini",
    alsa_config: "/data/pulsar/asound.conf",
    alsa_lib: "/system/vendor/quasar/libs/libasound.so.2",
    // No softvol "speakers" PCM and the hardware "Master" control does not attenuate the output;
    // Controller applies the volume to the GStreamer player channels instead.
    software_volume: true,
    spk_mute_ctl: "SPK mute",
    // The stock maind opens the tdm_state amplifier gates; without them the ALSA sink runs but the
    // speaker stays silent (SPK mute cannot be cleared while amp_table is 0). The driver clears
    // amp_table when the playback PCM closes (observed idle state), and both sinks close it when
    // playback ends, so only the next start has to set the gates again.
    amp_enable_paths: &[
        "/sys/class/tdm_state/amp_table",
        "/sys/class/tdm_state/aux_amp",
    ],
    volume_map: STOCK_VOLUME,
    volume_min_step: true,
    has_lux: false,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

// The Mini 3's quasar.cfg platformToAlsaMap: { "0": 0, "1": 33, "20": 255 }.
const MINI3_VOLUME: &[VolumePoint] = &[
    VolumePoint { level: 0, raw: 0 },
    VolumePoint { level: 1, raw: 33 },
    VolumePoint {
        level: 20,
        raw: 255,
    },
];

/// Station Mini 3: 32-bit armhf userland on an aarch64 kernel, hw:0,1 physical channels
/// (asound.conf multi_capture): PDM mics 0-2, ch 6 = the amplifier feedback (TDMB_IN0), the VQE
/// speaker reference. 6-zone ring, no logo.
static MINI3: Model = Model {
    codename: "bergamot",
    host_prefix: "yandex-mini3-",
    friendly_name: "Яндекс Станция Мини 3",
    model_name: "Yandex Station Mini 3",
    capture_pcm: "hw:0,1",
    capture_rate: 48000,
    capture_channels: 8,
    capture_format: CaptureFormat::S16,
    mic_channels: &[0, 1, 2],
    spk_channels: &[6],
    vqe_preset: "bergamot",
    wake_preroll: BERGAMOT_PREROLL,
    led_prefix: "bergamot",
    // The driver calls the 6-zone ring "logo" (/dev/fb0 6×1, AW21018); there is no separate logo.
    ring: LedStrip {
        count: 6,
        first_index: 0,
        fb_dev: "/dev/fb0",
        fb_name: "logo",
        trigger: "ledfb-logo",
        sysfs_kind: "logo",
        i2c_dev: "",
        i2c_addr: 0,
    },
    logo: NO_STRIP,
    clock_dir: "/sys/class/leds/bergamot:white:clock",
    clock_seg_base: 'A',
    clock_pos_base: 0,
    clock_colon: &["DH", "DL"],
    clock_aux: &["DEGREE"],
    clock_center: "DM",
    clock_chip_current: false,
    // Sold with and without the LED clock (AW21036): segments are looked up at start.
    has_clock: false,
    keys_input: "aw933xx",
    mute_input: "gpio-keys",
    mute_gpio: 0,
    action_key: 582,
    mute_key: 0,
    error_sound: "ru-RU/playback_error.mp3",
    wake_sound: "common/spotter_start.mp3",
    cancel_sound: "common/spotter_cancel.mp3",
    timer_sound: "common/timer.wav",
    volume_sound: "common/volume_sound.mp3",
    playback_pcm: "speakers",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    spk_mute_ctl: "",
    amp_enable_paths: &[],
    volume_map: MINI3_VOLUME,
    volume_min_step: true,
    has_lux: true,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

impl Model {
    /// Process-wide ALSA setup, before any thread or audio library is started. Only the Mini sets
    /// ALSA_CONFIG_PATH: its dmix PCM lives in /data/pulsar/asound.conf (put there by
    /// pulsar-install). A missing file leaves the firmware's config, where that PCM does not exist.
    pub fn apply_env(&self) {
        if self.alsa_config.is_empty() {
            return;
        }
        if std::path::Path::new(self.alsa_config).is_file() {
            crate::sys::set_var("ALSA_CONFIG_PATH", self.alsa_config);
        } else {
            log::error!(
                "alsa: {} missing, ALSA_CONFIG_PATH not set: '{}' cannot open until pulsar-install \
                 puts it back",
                self.alsa_config,
                self.playback_pcm
            );
        }
    }

    /// Whether the keys device feeds the «Button» event entity (action, action_long, volume_up,
    /// volume_down): every model with the action button also has the volume keys. The Mini's
    /// keypad has only the mute button, which toggles the Microphones switch instead.
    pub fn has_button_events(&self) -> bool {
        !self.keys_input.is_empty() && self.action_key != 0
    }
}

/// The `deviceType` value of quasar.cfg (e.g. "yandexmini"), the firmware's own model marker.
pub fn quasar_device_type(cfg: &str) -> Option<&str> {
    const KEY: &str = "\"deviceType\"";
    let after = cfg.find(KEY)? + KEY.len();
    let rest = cfg[after..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// The Mini model from its board id, but only when quasar.cfg confirms a Yandex firmware:
/// `axg_s420_v03` is an Amlogic S420 reference board id, not a Yandex-specific one.
pub fn mini_from_board_and_cfg(dt_id: &[u8], quasar_cfg: &str) -> Option<&'static Model> {
    (quasar_device_type(quasar_cfg) == Some("yandexmini"))
        .then(|| model_from_dt_id(dt_id))
        .flatten()
}
/// The model of this device, read once; None (logged) on unsupported hardware.
pub fn detect_model() -> Option<&'static Model> {
    static MODEL: LazyLock<Option<&'static Model>> = LazyLock::new(|| {
        let raw = std::fs::read("/proc/device-tree/compatible").unwrap_or_default();
        let model = model_from_compatible(&raw).or_else(|| {
            let dt = std::fs::read("/proc/device-tree/amlogic-dt-id").unwrap_or_default();
            let cfg =
                std::fs::read_to_string("/system/vendor/quasar/quasar.cfg").unwrap_or_default();
            mini_from_board_and_cfg(&dt, &cfg)
        });
        if model.is_none() {
            let shown = String::from_utf8_lossy(&raw).replace('\0', " ");
            let dt = std::fs::read("/proc/device-tree/amlogic-dt-id").unwrap_or_default();
            let dt = String::from_utf8_lossy(&dt);
            log::error!(
                "unsupported device (devicetree compatible: {shown}, amlogic-dt-id: '{}')",
                dt.trim_end_matches('\0')
            );
        }
        model
    });
    *MODEL
}

/// The model of the first exact "yandex,<codename>" entry of a NUL-separated compatible list,
/// e.g. "yandex,cucumber-dvt2\0yandex,cucumber\0amlogic, a5\0"; a board variant alone
/// ("yandex,bergamot-evt1") is no model.
pub fn model_from_compatible(raw: &[u8]) -> Option<&'static Model> {
    raw.split(|&b| b == 0).find_map(|entry| match entry {
        b"yandex,cucumber" => Some(&MIDI),
        b"yandex,plum" => Some(&LITE2),
        b"yandex,station-mini2" => Some(&MINI2),
        b"yandex,bergamot" => Some(&MINI3),
        _ => None,
    })
}

/// The Mini's devicetree has no yandex,* entry. The board id alone is not a Yandex marker; use
/// [`mini_from_board_and_cfg`] unless testing the id parser.
pub fn model_from_dt_id(raw: &[u8]) -> Option<&'static Model> {
    let id = raw.split(|&b| b == 0).next().unwrap_or_default();
    (id == b"axg_s420_v03").then_some(&MINI)
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
        assert_eq!(m.capture_format, CaptureFormat::S16);
        assert_eq!(m.action_key, 582);
        assert_eq!(m.mute_key, 0);
        assert!(m.has_button_events());
    }

    #[test]
    fn mini_is_detected_by_board_id() {
        let m = model_from_dt_id(b"axg_s420_v03\0").unwrap();
        assert_eq!(m.codename, "yandexmini");
        assert_eq!(m.capture_rate, 48000);
        assert_eq!(m.capture_format, CaptureFormat::S32 { shift: 8 });
        assert_eq!(m.ring.i2c_addr, 0x3c);
        assert_eq!(m.ring.count, 6);
        assert_eq!(m.logo.count, 0);
        assert!(m.software_volume);
        assert!(!m.has_clock && !m.has_lux);
        // Only KEY_MUTE on the keypad: it toggles the mute, and there is no «Button» entity.
        assert_eq!((m.action_key, m.mute_key), (0, 113));
        assert!(!m.has_button_events());
        // No trailing NUL still counts.
        assert_eq!(
            model_from_dt_id(b"axg_s420_v03").unwrap().codename,
            "yandexmini"
        );
        assert!(model_from_dt_id(b"axg_s420_v02\0").is_none());
        // axg_s420_v03 is an Amlogic reference-board id: only a Yandex firmware makes it a Mini.
        let cfg = r#"{"deviceType" : "yandexmini", "x": 1}"#;
        assert_eq!(
            mini_from_board_and_cfg(b"axg_s420_v03\0", cfg)
                .unwrap()
                .codename,
            "yandexmini"
        );
        assert!(mini_from_board_and_cfg(b"axg_s420_v03\0", r#"{"deviceType":"other"}"#).is_none());
        assert!(mini_from_board_and_cfg(b"axg_s420_v02\0", cfg).is_none());
        assert_eq!(quasar_device_type(cfg), Some("yandexmini"));
        assert_eq!(quasar_device_type("{}"), None);
        // The installer and boot.sh reject these too.
        assert_eq!(quasar_device_type(r#"{"deviceType" "yandexmini"}"#), None);
        assert_eq!(quasar_device_type(r#"{"deviceType": yandexmini}"#), None);
    }

    #[test]
    fn bergamot_is_mini3() {
        let m = model_from_compatible(b"yandex,bergamot-evt1\0yandex,bergamot\0amlogic, axg\0")
            .unwrap();
        assert_eq!(m.codename, "bergamot");
        assert_eq!(m.vqe_preset, "bergamot");
        assert_eq!(m.mic_channels, &[0, 1, 2]);
        assert_eq!(m.spk_channels, &[6]);
        assert_eq!(m.ring.count, 6);
        assert_eq!(m.ring.fb_name, "logo");
        assert_eq!(m.logo.count, 0);
        assert!(!m.has_clock && m.has_lux);
    }

    #[test]
    fn bergamot_requires_exact_compatible() {
        // A board variant alone, also next to a non-yandex "bergamot".
        for raw in [
            b"yandex,bergamot-evt1\0amlogic, axg\0".as_slice(),
            b"yandex,bergamot-evt1\0amlogic,bergamot\0".as_slice(),
        ] {
            assert!(model_from_compatible(raw).is_none(), "{raw:?}");
        }
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
