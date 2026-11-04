//! Per-model hardware description of the supported Yandex Stations, detected once from the
//! devicetree ("yandex,<codename>" in /proc/device-tree/compatible, with the Mini's
//! /proc/device-tree/amlogic-dt-id fallback).

use std::sync::LazyLock;

/// Раскладка светодиодов модели: задаёт эффекты лампы, их геометрию и имя сущности в HA.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LedLayout {
    #[default]
    Ring,
    Station2Panel,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Model {
    /// Кодовое имя из devicetree: "cucumber", "plum", "prune", "station-mini2", "bergamot",
    /// "pickle", "yandexmini", "station-midi".
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
    /// Физическая раскладка светодиодов; определяет тип эффектов и геометрию панели.
    pub led_layout: LedLayout,

    /// Префикс поканальных sysfs-светодиодов "<led_prefix>:<color>:<ring|logo><index>"; пусто — их
    /// нет (Мини, Станция 2).
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
    /// PCM воспроизведения ALSA (dmix, обычно за softvol «Master Playback Volume» — см.
    /// `master_pcm`): alsasink GStreamer, вывод объявлений и Sendspin.
    pub playback_pcm: &'static str,
    /// PCM со softvol «Master Playback Volume» (на hw:0): однократное открытие создаёт регулятор,
    /// если его ещё нет (softvol — пользовательский контрол alsa-lib). Обычно совпадает с
    /// `playback_pcm`; пусто — у моделей с программной громкостью, где регулятора нет.
    pub master_pcm: &'static str,
    /// ALSA config selected through ALSA_CONFIG_PATH before any thread starts (the Mini's dmix);
    /// empty: the firmware's /etc/asound.conf.
    pub alsa_config: &'static str,
    /// Absolute libasound path for `pulsar-alsa` (the Mini keeps it outside the loader path);
    /// empty: the usual "libasound.so.2" lookup.
    pub alsa_lib: &'static str,
    pub software_volume: bool,
    /// Sysfs-затворы усилителя: в каждый пишется "1" до и после открытия PCM воспроизведения
    /// (`pulsar_alsa::enable_output`). Пусто: затвора нет.
    pub amp_enable_paths: &'static [&'static str],

    /// Штатная кривая громкости: уровень платформы -> сырое значение (0..255) softvol «Master
    /// Playback Volume» (`master_pcm`), кусочно-линейная между точками; первая точка — уровень 0
    /// (raw 0), последняя — верхний уровень. В quasar.cfg — volume_capability
    /// .volume_manager_settings.platformToAlsaMap. На 20-уровневых картах один уровень платформы —
    /// 5 %; громкость Алисы в maind (0..10, «громче» / «тише») — уровень платформы / 2 с
    /// округлением половины вверх. changeStepPercent в quasar.cfg — шаг плавного изменения
    /// громкости в maind (start_volume_change), а не кнопок и голосовых команд.
    pub volume_map: &'static [VolumePoint],
    /// true: any audible volume is at least the map's second point (the stock firmware never
    /// sets the raw values below it).
    pub volume_min_step: bool,
    /// false: the station may lack the ambient light sensor; the Illuminance entity then exists
    /// only when the sensor opens at start, and a missing sensor is logged at info.
    pub has_lux: bool,
    /// Проверено на этой модели: падение в чужой libtflite зациклит перезапуски демона.
    pub speaker_id: bool,

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

/// Лента светодиодов: фреймбуфер (24 bpp B, G, R за ledfb-триггером; 32 bpp R, G, B, W панели
/// Станции 2 без триггера) с поканальными sysfs-светодиодами как фолбэком, если они есть, либо
/// I²C-контроллер IS31FL323x (18 каналов = 6 RGB-пикселей).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedStrip {
    pub count: i32,
    /// Нумерация sysfs начинается с 0 (Миди, Мини 3, Мини 3 Про) или с 1 (Лайт 2, Мини 2).
    pub first_index: i32,
    /// "/dev/fb0"; "" = none: sysfs LEDs only.
    pub fb_dev: &'static str,
    /// Expected /sys/class/graphics/fbN/name.
    pub fb_name: &'static str,
    /// LED-триггер, направляющий фреймбуфер на светодиоды; "" — триггера нет (панель Станции 2).
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

// { "0": 0, "1": 49, "20": 255 }: softvol min_dB -60, значит 2,6 дБ на уровень, уровень 1 — на
// -48,5 дБ. Та же карта в quasar.cfg Лайт 2 на плате plum; prune: см. docs/lite2.md, «Не проверено».
const STOCK_VOLUME: &[VolumePoint] = &[
    VolumePoint { level: 0, raw: 0 },
    VolumePoint { level: 1, raw: 49 },
    VolumePoint {
        level: 20,
        raw: 255,
    },
];

/// Станция Миди: см. docs/midi.md.
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
    led_layout: LedLayout::Ring,
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
    master_pcm: "speakers",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    amp_enable_paths: &[],
    volume_map: STOCK_VOLUME,
    volume_min_step: true,
    has_lux: true,
    speaker_id: true,
    // Telink B91 (quasar.cfg zigbee.tty.port). Reset and boot are GPIOZ_11 / GPIOZ_9
    // (devicetree node "zigbee", driver zb_control); maind drives them through these files when
    // quasar.cfg zigbee.zbControl is set, as it is on this firmware.
    zigbee_tty: "/dev/ttyS2",
    zigbee_reset: "/sys/devices/platform/zigbee/zb-reset/value",
    zigbee_boot: "/sys/devices/platform/zigbee/zb-boot/value",
};

/// Станция Лайт 2: см. docs/lite2.md.
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
    led_layout: LedLayout::Ring,
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
    master_pcm: "speakers",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    amp_enable_paths: &[],
    volume_map: STOCK_VOLUME,
    volume_min_step: true,
    has_lux: true,
    speaker_id: false,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

/// Станция Лайт 2 на плате prune: см. docs/lite2.md.
static PRUNE: Model = Model {
    codename: "prune",
    clock_dir: "",
    has_clock: false,
    keys_input: "aw933xx",
    has_lux: false,
    ..LITE2
};

// set_volume.sh у Мини 2 и (по вики) у Мини 3 Про и Станции 2 — `amixer set 'Master' <percent>%`:
// raw — процент от 255, без сдвига в дБ и без нижней ступени.
const PERCENT_VOLUME: &[VolumePoint] = &[
    VolumePoint { level: 0, raw: 0 },
    VolumePoint {
        level: 20,
        raw: 255,
    },
];

/// Станция Мини 2: см. docs/mini2.md.
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
    led_layout: LedLayout::Ring,
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
    // GPIOZ_3: см. docs/mini2.md, «Кнопки и микрофоны».
    mute_gpio: 414,
    action_key: 582,
    mute_key: 0,
    // Earcons в .opus: см. docs/mini2.md, «Звук».
    error_sound: "ru-RU/playback_error.opus",
    wake_sound: "common/spotter_start.opus",
    cancel_sound: "common/spotter_cancel.opus",
    timer_sound: "common/timer.opus",
    volume_sound: "common/volume_sound.opus",
    playback_pcm: "default",
    master_pcm: "default",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    amp_enable_paths: &[],
    volume_map: PERCENT_VOLUME,
    volume_min_step: false,
    // Датчик есть не на всех станциях: см. docs/mini2.md, «Светодиоды и часы».
    has_lux: false,
    speaker_id: false,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

/// Станция Мини (первое поколение): см. docs/mini.md.
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
    led_layout: LedLayout::Ring,
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
    master_pcm: "",
    alsa_config: "/data/pulsar/asound.conf",
    alsa_lib: "/system/vendor/quasar/libs/libasound.so.2",
    // No softvol "speakers" PCM and the hardware "Master" control does not attenuate the output;
    // Controller applies the volume to the GStreamer player channels instead.
    software_volume: true,
    // см. docs/mini.md, «Звук»
    amp_enable_paths: &["/sys/class/tdm_state/amp_table"],
    volume_map: STOCK_VOLUME,
    volume_min_step: true,
    has_lux: false,
    speaker_id: false,
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

/// Станция Мини 3: см. docs/mini3.md.
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
    led_layout: LedLayout::Ring,
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
    master_pcm: "speakers",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    amp_enable_paths: &[],
    volume_map: MINI3_VOLUME,
    volume_min_step: true,
    has_lux: true,
    speaker_id: false,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

/// Станция Мини 3 Про: см. docs/mini3pro.md.
static MINI3PRO: Model = Model {
    codename: "pickle",
    host_prefix: "yandex-mini3pro-",
    friendly_name: "Яндекс Станция Мини 3 Про",
    model_name: "Yandex Station Mini 3 Pro",
    capture_pcm: "hw:0,2",
    capture_rate: 48000,
    capture_channels: 4,
    capture_format: CaptureFormat::S16,
    // Раскладка со слов контрибьютора: см. docs/mini3pro.md, «Не проверено».
    mic_channels: &[0, 1, 2],
    spk_channels: &[3],
    vqe_preset: "pickle",
    led_layout: LedLayout::Ring,
    led_prefix: "pickle",
    ring: LedStrip {
        count: 16,
        first_index: 0,
        fb_dev: "/dev/fb1",
        fb_name: "ring",
        trigger: "ledfb-ring",
        sysfs_kind: "ring",
        i2c_dev: "",
        i2c_addr: 0,
    },
    logo: LedStrip {
        count: 6,
        first_index: 0,
        fb_dev: "/dev/fb0",
        fb_name: "logo",
        trigger: "ledfb-logo",
        sysfs_kind: "logo",
        i2c_dev: "",
        i2c_addr: 0,
    },
    // Раскладка часов Лайт 2 на плате plum: см. docs/mini3pro.md, «Светодиоды и часы».
    clock_dir: "/sys/class/leds/pickle:white:clock",
    clock_seg_base: 'A',
    clock_pos_base: 0,
    clock_colon: &["DH", "DL"],
    clock_aux: &["H0", "H1", "H2", "H3", "DEGREE"],
    clock_center: "DM",
    clock_chip_current: false,
    has_clock: true,
    keys_input: "aw933xx",
    mute_input: "mute",
    mute_gpio: 0,
    action_key: 582,
    mute_key: 0,
    error_sound: "ru-RU/playback_error.mp3",
    wake_sound: "common/spotter_start.mp3",
    cancel_sound: "common/spotter_cancel.mp3",
    timer_sound: "common/timer.wav",
    volume_sound: "common/volume_sound.mp3",
    playback_pcm: "speakers",
    master_pcm: "speakers",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    amp_enable_paths: &[],
    volume_map: PERCENT_VOLUME,
    volume_min_step: false,
    has_lux: true,
    speaker_id: false,
    zigbee_tty: "",
    zigbee_reset: "",
    zigbee_boot: "",
};

/// Станция 2: см. docs/station2.md.
static STATION2: Model = Model {
    codename: "station-midi",
    host_prefix: "yandex-station2-",
    friendly_name: "Яндекс Станция 2",
    model_name: "Yandex Station 2",
    capture_pcm: "hw:0,2",
    capture_rate: 48000,
    capture_channels: 16,
    capture_format: CaptureFormat::S16,
    mic_channels: &[0, 2, 4, 7],
    spk_channels: &[8, 9],
    vqe_preset: "yandexmidi",
    led_layout: LedLayout::Station2Panel,
    led_prefix: "",
    // Панель Gowin: 88 светодиодов на /dev/fb0, 89-й пиксель не горит —
    // https://hackalice.me/devices/station-2/#led.
    ring: LedStrip {
        count: 88,
        first_index: 0,
        fb_dev: "/dev/fb0",
        fb_name: "YNDX LED screen",
        trigger: "",
        sysfs_kind: "ring",
        i2c_dev: "",
        i2c_addr: 0,
    },
    logo: NO_STRIP,
    clock_dir: "",
    clock_seg_base: 'a',
    clock_pos_base: 0,
    clock_colon: &[],
    clock_aux: &[],
    clock_center: "",
    clock_chip_current: false,
    has_clock: false,
    keys_input: "midi_touch",
    mute_input: "gpio-keys",
    mute_gpio: 0,
    action_key: 582,
    mute_key: 0,
    error_sound: "ru-RU/playback_error.mp3",
    wake_sound: "common/spotter_start.mp3",
    cancel_sound: "common/spotter_cancel.mp3",
    timer_sound: "common/timer.wav",
    volume_sound: "common/volume_sound.mp3",
    // Штатный маршрут без дополнительного softvol: см. docs/station2.md, «Звук».
    playback_pcm: "default",
    // Master создаёт softvol soft_volume, а не default:
    // https://hackalice.me/devices/station-2/#audio.
    master_pcm: "soft_volume",
    alsa_config: "",
    alsa_lib: "",
    software_volume: false,
    amp_enable_paths: &[],
    // Линейная кривая, как у Мини 2: см. docs/station2.md, «Звук».
    volume_map: PERCENT_VOLUME,
    volume_min_step: false,
    has_lux: true,
    speaker_id: false,
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
        b"yandex,prune" => Some(&PRUNE),
        b"yandex,station-mini2" => Some(&MINI2),
        b"yandex,bergamot" => Some(&MINI3),
        b"yandex,pickle" => Some(&MINI3PRO),
        b"yandex,station-midi" => Some(&STATION2),
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
        assert!(m.speaker_id);
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
    fn prune_is_lite2_without_clock() {
        // devicetree станции из #57.
        let m = model_from_compatible(b"yandex,prune-evt2\0yandex,prune\0amlogic, axg\0").unwrap();
        assert_eq!(m.codename, "prune");
        assert_eq!(m.keys_input, "aw933xx");
        assert!(!m.has_clock && !m.has_lux);
        assert_eq!(m.clock_dir, "");
        // Звуковой тракт и светодиоды — как у plum.
        assert_eq!(m.vqe_preset, "plum");
        assert_eq!(m.mic_channels, &[0, 2]);
        assert_eq!(m.spk_channels, &[6]);
        assert_eq!(m.led_prefix, "plum");
        assert_eq!(m.ring.trigger, "led_ring_fb");
        assert_eq!(m.host_prefix, "yandex-lite2-");
        assert!(model_from_compatible(b"yandex,prune-evt2\0amlogic, axg\0").is_none());
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
    fn station_midi_is_station2() {
        let m = model_from_compatible(b"yandex,station-midi\0amlogic, axg\0").unwrap();
        assert_eq!(m.codename, "station-midi");
        assert_eq!(m.ring.count, 88);
        assert_eq!(m.capture_rate, 48_000);
        assert_eq!(m.playback_pcm, "default");
        assert_eq!(m.master_pcm, "soft_volume");
        assert_eq!(m.led_layout, LedLayout::Station2Panel);
        assert!(!m.has_clock);
        assert!(m.has_lux);
        assert_eq!(m.capture_channels, 16);
        assert_eq!(m.vqe_preset, "yandexmidi");
    }

    #[test]
    fn master_pcm_exactly_where_volume_is_hardware() {
        // Без master_pcm у модели с аппаратной громкостью Mixer не создаст регулятор Master.
        for m in [
            &MIDI, &LITE2, &PRUNE, &MINI2, &MINI, &MINI3, &MINI3PRO, &STATION2,
        ] {
            assert_eq!(m.master_pcm.is_empty(), m.software_volume, "{}", m.codename);
        }
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
    fn pickle_is_mini3pro() {
        // compatible по вики: вариант платы, затем модель.
        let m = model_from_compatible(b"yandex,pickle-evt\0yandex,pickle\0amlogic, a4\0").unwrap();
        assert_eq!(m.codename, "pickle");
        assert_eq!((m.capture_pcm, m.capture_channels), ("hw:0,2", 4));
        assert_eq!(m.mic_channels, &[0, 1, 2]);
        assert_eq!(m.spk_channels, &[3]);
        assert_eq!((m.ring.count, m.logo.count), (16, 6));
        assert!(m.has_clock && m.has_lux && !m.volume_min_step);
        assert_eq!(m.zigbee_tty, "");
        // Один вариант платы — не модель.
        assert!(model_from_compatible(b"yandex,pickle-evt\0amlogic, a4\0").is_none());
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
