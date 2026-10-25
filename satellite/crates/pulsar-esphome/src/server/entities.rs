//! Entities: DeviceInfo, ListEntities, the cached states (replayed on SubscribeStates, pushed to
//! subscribers on every publish) and the 60 s diagnostics.

use std::net::Ipv4Addr;

use tokio::time::Instant;

use super::{ESPHOME_VERSION, PROJECT_NAME, Server, msg};
use crate::proto::Writer;
use crate::types::{
    BinarySensorId, EventId, LightState, MediaState, NumberId, SelectId, SensorId, SwitchId,
    TextId, TextSensorId, UpdateState,
};

/// Entity keys (fixed32), stable across restarts: HA keys its entities by them.
pub(super) mod key {
    pub const MEDIA: u32 = 1;
    pub const SW_MICROPHONES: u32 = 2;
    pub const SW_CLOCK: u32 = 3;
    pub const BS_HW_MIC_MUTE: u32 = 4;
    pub const SENSOR_LUX: u32 = 5;
    pub const NUMBER_CLOCK_BRI: u32 = 6;
    pub const LIGHT_RING: u32 = 7;
    pub const EVENT_BUTTON: u32 = 8;
    pub const BUTTON_RESTART: u32 = 9;
    pub const SENSOR_RSSI: u32 = 10;
    pub const SENSOR_UPTIME: u32 = 11;
    pub const TEXT_IP: u32 = 12;
    pub const TEXT_VERSION: u32 = 13;
    pub const TEXT_CLOCK: u32 = 14;
    pub const SELECT_CLOCK_FACE: u32 = 15;
    pub const NUMBER_CLOCK_SCROLL: u32 = 16;
    pub const ACTION_CLOCK_TEXT: u32 = 17;
    pub const ACTION_CLOCK_FACE: u32 = 18;
    pub const UPDATE: u32 = 19;
    pub const SW_AUTO_UPDATE: u32 = 20;
    pub const SELECT_ZIGBEE_MODE: u32 = 21;
    pub const BUTTON_ZIGBEE_JOIN: u32 = 22;
    pub const BUTTON_ZIGBEE_LEAVE: u32 = 23;
    pub const TEXT_ZIGBEE: u32 = 24;
    pub const SW_NEAREST_ANSWERS: u32 = 25;
    pub const SELECT_MUSIC_VIZ: u32 = 26;
    pub const NUMBER_LED_SPEED: u32 = 27;
    pub const SW_VOICE_INDICATION: u32 = 28;
    pub const SELECT_CASE_COLOR: u32 = 29;
    pub const ACTION_LED_SHOW: u32 = 30;
    pub const SELECT_CLOCK_MUSIC: u32 = 31;
}

/// aioesphomeapi VoiceAssistantFeature: voice assistant, API audio, timers, announce, start
/// conversation.
pub(super) const VOICE_FEATURES: u32 = 1 | 4 | 8 | 16 | 32;
/// HA MediaPlayerEntityFeature: pause, volume set / mute / step, play media, stop, play,
/// media announce.
const MEDIA_FEATURES: u32 = 1 | 4 | 8 | 512 | 1024 | 4096 | 16384 | 1_048_576;
const ENTITY_CATEGORY_CONFIG: u32 = 1;
const ENTITY_CATEGORY_DIAGNOSTIC: u32 = 2;
const COLOR_MODE_RGB: u32 = 35;
const STATE_CLASS_MEASUREMENT: u32 = 1;
const STATE_CLASS_TOTAL_INCREASING: u32 = 2;
const NUMBER_MODE_SLIDER: u32 = 2;
const FORMAT_PURPOSE_DEFAULT: u32 = 0;
const FORMAT_PURPOSE_ANNOUNCEMENT: u32 = 1;
/// api.proto ServiceArgType.
const SERVICE_ARG_INT: u32 = 1;
const SERVICE_ARG_STRING: u32 = 3;
/// Text «Clock text» max_length, UTF-8 characters.
pub(super) const CLOCK_TEXT_MAX: usize = 64;
/// Number «LED speed».
pub(super) const LED_SPEED_MIN: f32 = 0.25;
pub(super) const LED_SPEED_MAX: f32 = 4.0;
/// Wi-Fi signal, uptime and IP refresh.
pub(super) const DIAG_PERIOD_MS: u64 = 60_000;

/// Clamps to 0..1; NaN passes through, as in the C++ server.
pub(super) fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// A publish_* call of the handle, applied by the actor.
pub(crate) enum Publish {
    Media(MediaState, f32, bool),
    Switch(SwitchId, bool),
    BinarySensor(BinarySensorId, bool),
    Sensor(SensorId, f32),
    Number(NumberId, f32),
    Text(TextId, String),
    Select(SelectId, String),
    TextSensor(TextSensorId, String),
    Light(LightState),
    Event(EventId, String),
    Update(Box<UpdateState>),
}

#[derive(Clone, Copy, Default)]
pub(super) struct BoolState {
    has: bool,
    v: bool,
}

#[derive(Clone, Copy, Default)]
pub(super) struct FloatState {
    has: bool,
    v: f32,
}

#[derive(Clone, Default)]
pub(super) struct StrState {
    has: bool,
    v: String,
}

/// Entity states as last published (replayed on SubscribeStatesRequest).
pub(super) struct Cache {
    media_state: MediaState,
    media_volume: f32,
    media_muted: bool,
    sw_microphones: BoolState,
    sw_clock: BoolState,
    sw_auto_update: BoolState,
    sw_nearest_answers: BoolState,
    sw_voice_indication: BoolState,
    bs_hw_mute: BoolState,
    update: UpdateState,
    update_has: bool,
    lux: FloatState,
    clock_bri: FloatState,
    clock_scroll: FloatState,
    led_speed: FloatState,
    clock_text: StrState,
    clock_face: StrState,
    zigbee_mode: StrState,
    zigbee_status: StrState,
    music_viz: StrState,
    case_color: StrState,
    clock_music: StrState,
    pub(super) light: LightState,
    rssi: FloatState,
    ip: Option<Ipv4Addr>,
    started: Instant,
}

impl Cache {
    pub(super) fn new() -> Self {
        Self {
            media_state: MediaState::Idle,
            media_volume: 0.0,
            media_muted: false,
            sw_microphones: BoolState::default(),
            sw_clock: BoolState::default(),
            sw_auto_update: BoolState::default(),
            sw_nearest_answers: BoolState::default(),
            sw_voice_indication: BoolState::default(),
            bs_hw_mute: BoolState::default(),
            update: UpdateState::default(),
            update_has: false,
            lux: FloatState::default(),
            clock_bri: FloatState::default(),
            clock_scroll: FloatState::default(),
            led_speed: FloatState::default(),
            clock_text: StrState::default(),
            clock_face: StrState::default(),
            zigbee_mode: StrState::default(),
            zigbee_status: StrState::default(),
            music_viz: StrState::default(),
            case_color: StrState::default(),
            clock_music: StrState::default(),
            light: LightState::default(),
            rssi: FloatState::default(),
            ip: None,
            started: Instant::now(),
        }
    }
}

/// wlan0 signal level in dBm from /proc/net/wireless ("wlan0: 0000   58.  -52.  -256 ...").
pub(super) fn parse_wireless(text: &str) -> Option<f32> {
    text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        if w.next()? != "wlan0:" {
            return None;
        }
        u32::from_str_radix(w.next()?, 16).ok()?;
        w.next()?.parse::<f32>().ok()?;
        w.next()?.parse::<f32>().ok()
    })
}

fn entity_head(w: &mut Writer, object_id: &str, key: u32, name: &str) {
    w.str(1, object_id, true);
    w.fixed32(2, key, true);
    w.str(3, name, true);
}

/// `None`: every state subscriber; `Some(id)`: that client only (initial states).
type To = Option<u64>;

impl Server {
    fn states_on(&self) -> bool {
        self.clients.iter().any(|c| c.hello && c.states)
    }

    /// State updates go to every client that subscribed to states.
    pub(super) fn broadcast(&mut self, msg_type: u32, w: &Writer) {
        self.for_each_client(|s, id| {
            if s.client(id).is_some_and(|c| c.hello && c.states) {
                s.send_msg(id, msg_type, w);
            }
        });
    }

    fn emit_state(&mut self, to: To, msg_type: u32, w: &Writer) {
        match to {
            Some(id) => self.send_msg(id, msg_type, w),
            None => self.broadcast(msg_type, w),
        }
    }

    pub(super) fn send_device_info(&mut self, id: u64) {
        let supported = self.cfg.key_path.is_some();
        let mut w = Writer::new();
        w.str(2, &self.id.name, true);
        w.str(3, &self.id.mac, true);
        w.str(4, ESPHOME_VERSION, true);
        w.str(5, &self.id.compilation_time, true);
        w.str(6, &self.id.model, true);
        w.str(8, PROJECT_NAME, true);
        w.str(9, &self.id.project_version, true);
        w.str(12, &self.id.manufacturer, true);
        w.str(13, &self.id.friendly_name, true);
        w.u32(17, VOICE_FEATURES, false);
        w.boolean(19, supported, false); // api_encryption_supported
        w.boolean(26, supported && !self.key.has_key, false); // api_encryption_provisionable
        self.send_msg(id, msg::DEVICE_INFO_RESP, &w);
    }

    pub(super) fn send_device_caps(&mut self, id: u64) {
        let mut va = Writer::new();
        va.u32(1, VOICE_FEATURES, false);
        let mut w = Writer::new();
        w.msg(2, &va);
        self.send_msg(id, msg::DEVICE_CAPS_RESP, &w);
    }

    /// The entity list in the C++ order (HA keeps it per device; keys and object_ids must not
    /// move).
    pub(super) fn list_entities(&mut self, id: u64) {
        let mut out: Vec<(u32, Writer)> = Vec::with_capacity(40);
        let mut w = Writer::new();
        entity_head(&mut w, "media_player", key::MEDIA, "Media Player");
        w.boolean(8, true, false); // supports_pause (deprecated, for older clients)
        for (channels, purpose) in [
            (2, FORMAT_PURPOSE_DEFAULT),
            (1, FORMAT_PURPOSE_ANNOUNCEMENT),
        ] {
            let mut s = Writer::new();
            s.str(1, "flac", false);
            s.u32(2, 48000, false);
            s.u32(3, channels, false);
            s.u32(4, purpose, false);
            s.u32(5, 2, false); // 16-bit
            w.msg(9, &s);
        }
        w.u32(11, MEDIA_FEATURES, false);
        out.push((msg::LIST_MEDIA_PLAYER, w));
        let switch = |oid, key, name, icon, category: Option<u32>| {
            let mut w = Writer::new();
            entity_head(&mut w, oid, key, name);
            w.str(5, icon, false);
            if let Some(c) = category {
                w.u32(8, c, false);
            }
            (msg::LIST_SWITCH, w)
        };
        out.push(switch(
            "microphones",
            key::SW_MICROPHONES,
            "Microphones",
            "mdi:microphone",
            None,
        ));
        out.push(switch(
            "clock_display",
            key::SW_CLOCK,
            "Clock display",
            "mdi:clock-digital",
            None,
        ));
        let cfg = Some(ENTITY_CATEGORY_CONFIG);
        out.push(switch(
            "auto_update",
            key::SW_AUTO_UPDATE,
            "Auto update",
            "mdi:update",
            cfg,
        ));
        out.push(switch(
            "nearest_station_answers",
            key::SW_NEAREST_ANSWERS,
            "Nearest station answers",
            "mdi:speaker-multiple",
            cfg,
        ));
        out.push(switch(
            "voice_indication",
            key::SW_VOICE_INDICATION,
            "Voice indication",
            "mdi:led-on",
            cfg,
        ));
        let number = |oid, key, name, icon, (min, max, step): (f32, f32, f32), unit| {
            let mut w = Writer::new();
            entity_head(&mut w, oid, key, name);
            w.str(5, icon, false);
            w.flt(6, min, false);
            w.flt(7, max, false);
            w.flt(8, step, false);
            w.u32(10, ENTITY_CATEGORY_CONFIG, false);
            w.str(11, unit, false);
            w.u32(12, NUMBER_MODE_SLIDER, false);
            (msg::LIST_NUMBER, w)
        };
        out.push(number(
            "led_speed",
            key::NUMBER_LED_SPEED,
            "LED speed",
            "mdi:speedometer",
            (LED_SPEED_MIN, LED_SPEED_MAX, 0.05),
            "×",
        ));
        let select = |oid, key, name, icon, options: &[String], category: Option<u32>| {
            let mut w = Writer::new();
            entity_head(&mut w, oid, key, name);
            w.str(5, icon, false);
            for o in options {
                w.str(6, o, true);
            }
            if let Some(c) = category {
                w.u32(8, c, false);
            }
            (msg::LIST_SELECT, w)
        };
        out.push(select(
            "music_visualization",
            key::SELECT_MUSIC_VIZ,
            "Music visualization",
            "mdi:equalizer",
            &self.cfg.music_styles,
            cfg,
        ));
        out.push(select(
            "case_color",
            key::SELECT_CASE_COLOR,
            "Case color",
            "mdi:palette",
            &self.cfg.case_colors,
            cfg,
        ));
        out.push(select(
            "clock_music",
            key::SELECT_CLOCK_MUSIC,
            "Clock music",
            "mdi:equalizer-outline",
            &self.cfg.clock_music_styles,
            cfg,
        ));
        let mut w = Writer::new();
        entity_head(&mut w, "firmware", key::UPDATE, "Firmware");
        w.u32(7, ENTITY_CATEGORY_CONFIG, false);
        w.str(8, "firmware", false);
        out.push((msg::LIST_UPDATE, w));
        let mut w = Writer::new();
        entity_head(
            &mut w,
            "hardware_mic_mute",
            key::BS_HW_MIC_MUTE,
            "Hardware mic mute",
        );
        w.str(8, "mdi:microphone-off", false);
        w.u32(9, ENTITY_CATEGORY_DIAGNOSTIC, false);
        out.push((msg::LIST_BINARY_SENSOR, w));
        let mut w = Writer::new();
        entity_head(&mut w, "illuminance", key::SENSOR_LUX, "Illuminance");
        w.str(6, "lx", false);
        w.i32(7, 0, false);
        w.str(9, "illuminance", false);
        w.u32(10, STATE_CLASS_MEASUREMENT, false);
        out.push((msg::LIST_SENSOR, w));
        out.push(number(
            "clock_brightness",
            key::NUMBER_CLOCK_BRI,
            "Clock brightness",
            "mdi:brightness-6",
            (0.0, 100.0, 1.0),
            "%",
        ));
        let mut w = Writer::new();
        entity_head(&mut w, "ring", key::LIGHT_RING, "Ring");
        w.str(11, "None", true);
        for e in &self.cfg.light_effects {
            w.str(11, e, true);
        }
        w.packed_u32(12, &[COLOR_MODE_RGB]);
        w.str(14, "mdi:led-strip-variant", false);
        out.push((msg::LIST_LIGHT, w));
        let mut w = Writer::new();
        entity_head(&mut w, "button", key::EVENT_BUTTON, "Button");
        w.str(5, "mdi:gesture-tap-button", false);
        w.str(8, "button", false);
        for t in ["action", "action_long", "volume_up", "volume_down"] {
            w.str(9, t, true);
        }
        out.push((msg::LIST_EVENT, w));
        let mut w = Writer::new();
        entity_head(&mut w, "restart", key::BUTTON_RESTART, "Restart");
        w.u32(7, ENTITY_CATEGORY_CONFIG, false);
        w.str(8, "restart", false);
        out.push((msg::LIST_BUTTON, w));
        let sensor = |oid, key, name, unit, class, state_class| {
            let mut w = Writer::new();
            entity_head(&mut w, oid, key, name);
            w.str(6, unit, false);
            w.str(9, class, false);
            w.u32(10, state_class, false);
            w.u32(13, ENTITY_CATEGORY_DIAGNOSTIC, false);
            (msg::LIST_SENSOR, w)
        };
        out.push(sensor(
            "wifi_signal",
            key::SENSOR_RSSI,
            "Wi-Fi signal",
            "dBm",
            "signal_strength",
            STATE_CLASS_MEASUREMENT,
        ));
        out.push(sensor(
            "uptime",
            key::SENSOR_UPTIME,
            "Uptime",
            "s",
            "duration",
            STATE_CLASS_TOTAL_INCREASING,
        ));
        let text = |oid, key, name, icon| {
            let mut w = Writer::new();
            entity_head(&mut w, oid, key, name);
            w.str(5, icon, false);
            w.u32(7, ENTITY_CATEGORY_DIAGNOSTIC, false);
            (msg::LIST_TEXT_SENSOR, w)
        };
        out.push(text(
            "ip_address",
            key::TEXT_IP,
            "IP address",
            "mdi:ip-network",
        ));
        out.push(text("version", key::TEXT_VERSION, "Version", "mdi:new-box"));
        out.push(number(
            "clock_text_speed",
            key::NUMBER_CLOCK_SCROLL,
            "Clock text speed",
            "mdi:speedometer",
            (100.0, 2000.0, 50.0),
            "ms",
        ));
        let mut w = Writer::new();
        entity_head(&mut w, "clock_text", key::TEXT_CLOCK, "Clock text");
        w.str(5, "mdi:form-textbox", false);
        w.u32(9, CLOCK_TEXT_MAX as u32, false); // max_length
        out.push((msg::LIST_TEXT, w));
        out.push(select(
            "clock_face",
            key::SELECT_CLOCK_FACE,
            "Clock face",
            "mdi:emoticon-outline",
            &self.cfg.clock_faces,
            None,
        ));
        if !self.cfg.zigbee_modes.is_empty() {
            out.push(select(
                "zigbee_role",
                key::SELECT_ZIGBEE_MODE,
                "Zigbee role",
                "mdi:zigbee",
                &self.cfg.zigbee_modes,
                cfg,
            ));
            let button = |oid, key, name, icon| {
                let mut w = Writer::new();
                entity_head(&mut w, oid, key, name);
                w.str(5, icon, false);
                w.u32(7, ENTITY_CATEGORY_CONFIG, false);
                (msg::LIST_BUTTON, w)
            };
            out.push(button(
                "zigbee_join",
                key::BUTTON_ZIGBEE_JOIN,
                "Zigbee join network",
                "mdi:link-variant",
            ));
            out.push(button(
                "zigbee_leave",
                key::BUTTON_ZIGBEE_LEAVE,
                "Zigbee leave network",
                "mdi:link-variant-off",
            ));
            out.push(text("zigbee", key::TEXT_ZIGBEE, "Zigbee", "mdi:zigbee"));
        }
        // User-defined actions (HA registers esphome.<device>_<name>; every argument is required).
        let action = |name, key, args: &[(&str, u32)]| {
            let mut w = Writer::new();
            w.str(1, name, true);
            w.fixed32(2, key, true);
            for &(arg, ty) in args {
                let mut a = Writer::new();
                a.str(1, arg, true);
                a.u32(2, ty, false);
                w.msg(3, &a);
            }
            (msg::LIST_SERVICES, w)
        };
        out.push(action(
            "clock_show_text",
            key::ACTION_CLOCK_TEXT,
            &[("text", SERVICE_ARG_STRING), ("seconds", SERVICE_ARG_INT)],
        ));
        out.push(action(
            "clock_show_face",
            key::ACTION_CLOCK_FACE,
            &[("face", SERVICE_ARG_STRING), ("seconds", SERVICE_ARG_INT)],
        ));
        out.push(action(
            "led_show",
            key::ACTION_LED_SHOW,
            &[
                ("effect", SERVICE_ARG_STRING),
                ("color", SERVICE_ARG_STRING),
                ("seconds", SERVICE_ARG_INT),
            ],
        ));
        out.push((msg::LIST_DONE, Writer::new()));
        for (t, w) in &out {
            self.send_msg(id, *t, w);
        }
    }

    pub(super) fn send_va_config(&mut self, id: u64) {
        let mut w = Writer::new();
        for ww in &self.cfg.wake_words {
            let mut s = Writer::new();
            s.str(1, &ww.id, false);
            s.str(2, &ww.phrase, false);
            for l in &ww.languages {
                s.str(3, l, true);
            }
            w.msg(1, &s);
        }
        for a in &self.cfg.active_wake_words {
            w.str(2, a, true);
        }
        w.u32(3, 1, false); // max_active_wake_words
        self.send_msg(id, msg::VA_CONFIG_RESP, &w);
    }

    // ------------------------------------------------------------ states

    fn send_media(&mut self, to: To) {
        let c = &self.cache;
        let mut w = Writer::new();
        w.fixed32(1, key::MEDIA, true);
        w.u32(2, c.media_state as u32, false);
        w.flt(3, c.media_volume, false);
        w.boolean(4, c.media_muted, false);
        self.emit_state(to, msg::MEDIA_PLAYER_STATE, &w);
    }

    /// SwitchStateResponse and BinarySensorStateResponse differ in the missing_state field.
    fn send_bool(&mut self, msg_type: u32, key: u32, s: BoolState, to: To) {
        let mut w = Writer::new();
        w.fixed32(1, key, true);
        w.boolean(2, s.v, false);
        let missing = if msg_type == msg::SWITCH_STATE { 4 } else { 3 };
        w.boolean(missing, !s.has, false);
        self.emit_state(to, msg_type, &w);
    }

    fn send_float(&mut self, msg_type: u32, key: u32, s: FloatState, to: To) {
        let mut w = Writer::new();
        w.fixed32(1, key, true);
        w.flt(2, s.v, false);
        w.boolean(3, !s.has, false);
        self.emit_state(to, msg_type, &w);
    }

    /// TextSensorState, TextState and SelectState share the layout key / state / missing_state.
    fn send_str(&mut self, msg_type: u32, key: u32, s: &str, has: bool, to: To) {
        let mut w = Writer::new();
        w.fixed32(1, key, true);
        w.str(2, s, false);
        w.boolean(3, !has, false);
        self.emit_state(to, msg_type, &w);
    }

    fn send_cached_str(&mut self, msg_type: u32, key: u32, get: fn(&Cache) -> &StrState, to: To) {
        let s = get(&self.cache);
        let (v, has) = (s.v.clone(), s.has);
        self.send_str(msg_type, key, &v, has, to);
    }

    pub(super) fn send_light(&mut self, to: To) {
        // HA expects rgb normalised to max 1 with the scale carried in color_brightness.
        let l = &self.cache.light;
        let m = l.r.max(l.g).max(l.b);
        let norm = |v: f32| if m > 0.0 { v / m } else { 1.0 };
        let mut w = Writer::new();
        w.fixed32(1, key::LIGHT_RING, true);
        w.boolean(2, l.on, false);
        w.flt(3, l.brightness, false);
        w.flt(10, if m > 0.0 { m } else { 1.0 }, false);
        w.u32(11, COLOR_MODE_RGB, false);
        w.flt(4, norm(l.r), false);
        w.flt(5, norm(l.g), false);
        w.flt(6, norm(l.b), false);
        w.str(
            9,
            if l.effect.is_empty() {
                "None"
            } else {
                &l.effect
            },
            false,
        );
        self.emit_state(to, msg::LIGHT_STATE, &w);
    }

    fn send_update(&mut self, to: To) {
        let (u, has) = (&self.cache.update, self.cache.update_has);
        let mut w = Writer::new();
        w.fixed32(1, key::UPDATE, true);
        w.boolean(2, !has, false); // missing_state
        w.boolean(3, u.in_progress, false);
        w.boolean(4, u.has_progress, false);
        w.flt(5, u.progress, false);
        w.str(6, &u.current_version, false);
        w.str(7, &u.latest_version, false);
        w.str(8, &u.title, false);
        w.str(10, &u.release_url, false);
        self.emit_state(to, msg::UPDATE_STATE, &w);
    }

    fn send_ip(&mut self, to: To) {
        let ip = self.cache.ip;
        let s = ip.map(|a| a.to_string()).unwrap_or_default();
        self.send_str(msg::TEXT_SENSOR_STATE, key::TEXT_IP, &s, ip.is_some(), to);
    }

    fn send_uptime(&mut self, to: To) {
        let secs = self.cache.started.elapsed().as_secs();
        let s = FloatState {
            has: true,
            v: secs as f32,
        };
        self.send_float(msg::SENSOR_STATE, key::SENSOR_UPTIME, s, to);
    }

    pub(super) fn send_all_states(&mut self, id: u64) {
        let to = Some(id);
        let c = &self.cache;
        let (mic, clock, auto, nearest, voice, hw) = (
            c.sw_microphones,
            c.sw_clock,
            c.sw_auto_update,
            c.sw_nearest_answers,
            c.sw_voice_indication,
            c.bs_hw_mute,
        );
        let (lux, bri, scroll, speed) = (c.lux, c.clock_bri, c.clock_scroll, c.led_speed);
        self.send_media(to);
        self.send_bool(msg::SWITCH_STATE, key::SW_MICROPHONES, mic, to);
        self.send_bool(msg::SWITCH_STATE, key::SW_CLOCK, clock, to);
        self.send_bool(msg::SWITCH_STATE, key::SW_AUTO_UPDATE, auto, to);
        self.send_bool(msg::SWITCH_STATE, key::SW_NEAREST_ANSWERS, nearest, to);
        self.send_bool(msg::SWITCH_STATE, key::SW_VOICE_INDICATION, voice, to);
        self.send_update(to);
        self.send_bool(msg::BINARY_SENSOR_STATE, key::BS_HW_MIC_MUTE, hw, to);
        self.send_float(msg::SENSOR_STATE, key::SENSOR_LUX, lux, to);
        self.send_float(msg::NUMBER_STATE, key::NUMBER_CLOCK_BRI, bri, to);
        self.send_light(to);
        let rssi = self.cache.rssi;
        self.send_float(msg::SENSOR_STATE, key::SENSOR_RSSI, rssi, to);
        self.send_uptime(to);
        self.send_ip(to);
        let version = self.id.project_version.clone();
        self.send_str(
            msg::TEXT_SENSOR_STATE,
            key::TEXT_VERSION,
            &version,
            true,
            to,
        );
        self.send_float(msg::NUMBER_STATE, key::NUMBER_CLOCK_SCROLL, scroll, to);
        self.send_cached_str(msg::TEXT_STATE, key::TEXT_CLOCK, |c| &c.clock_text, to);
        self.send_cached_str(
            msg::SELECT_STATE,
            key::SELECT_CLOCK_FACE,
            |c| &c.clock_face,
            to,
        );
        self.send_float(msg::NUMBER_STATE, key::NUMBER_LED_SPEED, speed, to);
        self.send_cached_str(
            msg::SELECT_STATE,
            key::SELECT_MUSIC_VIZ,
            |c| &c.music_viz,
            to,
        );
        self.send_cached_str(
            msg::SELECT_STATE,
            key::SELECT_CASE_COLOR,
            |c| &c.case_color,
            to,
        );
        self.send_cached_str(
            msg::SELECT_STATE,
            key::SELECT_CLOCK_MUSIC,
            |c| &c.clock_music,
            to,
        );
        if !self.cfg.zigbee_modes.is_empty() {
            self.send_cached_str(
                msg::SELECT_STATE,
                key::SELECT_ZIGBEE_MODE,
                |c| &c.zigbee_mode,
                to,
            );
            self.send_cached_str(
                msg::TEXT_SENSOR_STATE,
                key::TEXT_ZIGBEE,
                |c| &c.zigbee_status,
                to,
            );
        }
    }

    /// Reads RSSI and the IP into the cache (once at start, then on the 60 s timer only: a
    /// client flooding SubscribeStates must not cost getifaddrs and /proc reads each time; the
    /// C++ server refreshed on every subscription). RSSI and uptime go out every period, the IP
    /// when it changed; `publish` false only fills the cache.
    pub(super) fn refresh_diagnostics(&mut self, publish: bool) {
        let dbm = std::fs::read_to_string("/proc/net/wireless")
            .ok()
            .and_then(|t| parse_wireless(&t));
        self.cache.rssi = FloatState {
            has: dbm.is_some(),
            v: dbm.unwrap_or(self.cache.rssi.v),
        };
        // getifaddrs failing keeps the address we had.
        let ip = match pulsar_core::net::best_interface_checked() {
            Ok(iface) => iface.map(|i| i.addr),
            Err(()) => self.cache.ip,
        };
        let ip_changed = ip != self.cache.ip;
        self.cache.ip = ip;
        if !publish || !self.states_on() {
            return;
        }
        let rssi = self.cache.rssi;
        self.send_float(msg::SENSOR_STATE, key::SENSOR_RSSI, rssi, None);
        self.send_uptime(None);
        if ip_changed {
            self.send_ip(None);
        }
    }

    // ------------------------------------------------------------ publish (handle commands)

    pub(super) fn publish(&mut self, p: Publish) {
        let on = self.states_on();
        let c = &mut self.cache;
        match p {
            Publish::Media(st, volume, muted) => {
                c.media_state = st;
                c.media_volume = clamp01(volume);
                c.media_muted = muted;
                if on {
                    self.send_media(None);
                }
            }
            Publish::Switch(id, v) => {
                let (s, key) = match id {
                    SwitchId::Microphones => (&mut c.sw_microphones, key::SW_MICROPHONES),
                    SwitchId::Clock => (&mut c.sw_clock, key::SW_CLOCK),
                    SwitchId::AutoUpdate => (&mut c.sw_auto_update, key::SW_AUTO_UPDATE),
                    SwitchId::NearestAnswers => {
                        (&mut c.sw_nearest_answers, key::SW_NEAREST_ANSWERS)
                    }
                    SwitchId::VoiceIndication => {
                        (&mut c.sw_voice_indication, key::SW_VOICE_INDICATION)
                    }
                };
                *s = BoolState { has: true, v };
                let s = *s;
                if on {
                    self.send_bool(msg::SWITCH_STATE, key, s, None);
                }
            }
            Publish::BinarySensor(BinarySensorId::HwMicMute, v) => {
                c.bs_hw_mute = BoolState { has: true, v };
                let s = c.bs_hw_mute;
                if on {
                    self.send_bool(msg::BINARY_SENSOR_STATE, key::BS_HW_MIC_MUTE, s, None);
                }
            }
            Publish::Sensor(SensorId::Illuminance, v) => {
                c.lux = FloatState { has: true, v };
                let s = c.lux;
                if on {
                    self.send_float(msg::SENSOR_STATE, key::SENSOR_LUX, s, None);
                }
            }
            Publish::Number(id, v) => {
                let (s, key) = match id {
                    NumberId::ClockBrightness => (&mut c.clock_bri, key::NUMBER_CLOCK_BRI),
                    NumberId::ClockScroll => (&mut c.clock_scroll, key::NUMBER_CLOCK_SCROLL),
                    NumberId::LedSpeed => (&mut c.led_speed, key::NUMBER_LED_SPEED),
                };
                *s = FloatState { has: true, v };
                let s = *s;
                if on {
                    self.send_float(msg::NUMBER_STATE, key, s, None);
                }
            }
            Publish::Text(TextId::ClockText, v) => {
                if on {
                    self.send_str(msg::TEXT_STATE, key::TEXT_CLOCK, &v, true, None);
                }
                self.cache.clock_text = StrState { has: true, v };
            }
            Publish::Select(id, v) => {
                let key = match id {
                    SelectId::ClockFace => key::SELECT_CLOCK_FACE,
                    SelectId::ZigbeeMode => key::SELECT_ZIGBEE_MODE,
                    SelectId::MusicViz => key::SELECT_MUSIC_VIZ,
                    SelectId::CaseColor => key::SELECT_CASE_COLOR,
                    SelectId::ClockMusic => key::SELECT_CLOCK_MUSIC,
                };
                if on {
                    self.send_str(msg::SELECT_STATE, key, &v, true, None);
                }
                let c = &mut self.cache;
                let s = match id {
                    SelectId::ClockFace => &mut c.clock_face,
                    SelectId::ZigbeeMode => &mut c.zigbee_mode,
                    SelectId::MusicViz => &mut c.music_viz,
                    SelectId::CaseColor => &mut c.case_color,
                    SelectId::ClockMusic => &mut c.clock_music,
                };
                *s = StrState { has: true, v };
            }
            Publish::TextSensor(TextSensorId::Zigbee, v) => {
                if on && !self.cfg.zigbee_modes.is_empty() {
                    self.send_str(msg::TEXT_SENSOR_STATE, key::TEXT_ZIGBEE, &v, true, None);
                }
                self.cache.zigbee_status = StrState { has: true, v };
            }
            Publish::Light(st) => {
                c.light = st;
                if on {
                    self.send_light(None);
                }
            }
            Publish::Event(EventId::Button, event_type) => {
                if !on {
                    return;
                }
                let mut w = Writer::new();
                w.fixed32(1, key::EVENT_BUTTON, true);
                w.str(2, &event_type, true);
                self.broadcast(msg::EVENT_RESP, &w);
            }
            Publish::Update(st) => {
                c.update = *st;
                c.update_has = true;
                if on {
                    self.send_update(None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wireless_level() {
        let text = "Inter-| sta-|   Quality        |   Discarded packets\n \
                    face | tus | link level noise |  nwid  crypt\n \
                    wlan0: 0000   58.  -52.  -256        0      0\n";
        assert_eq!(parse_wireless(text), Some(-52.0));
        assert_eq!(parse_wireless("  eth0: 0000 1. -3. -256\n"), None);
        assert_eq!(parse_wireless("  wlan0: zz 1. -3.\n"), None);
        assert_eq!(parse_wireless(""), None);
    }

    #[test]
    fn clamp01_keeps_nan() {
        assert_eq!(clamp01(-1.0), 0.0);
        assert_eq!(clamp01(2.0), 1.0);
        assert_eq!(clamp01(0.25), 0.25);
        assert!(clamp01(f32::NAN).is_nan());
    }
}
