//! Messages of the entities and the voice assistant: commands from HA, validated as the C++
//! server did, become `ApiEvent`s.

use std::collections::HashMap;

use log::{debug, info, warn};

use super::entities::{CLOCK_TEXT_MAX, LED_SPEED_MAX, LED_SPEED_MIN, clamp01, key};
use super::{Server, msg};
use crate::proto::{Reader, WireType, zigzag32};
use crate::types::{
    ActionId, AnnounceRequest, ApiEvent, ButtonId, LightState, MediaCommand, MediaCommandRequest,
    NumberId, SelectId, SwitchId, TextId, TimerEvent, TimerInfo, UpdateCommand, VaEvent,
};

/// LightCommandRequest without a transition: ESPHome's default_transition_length. Longer ones
/// are cut to a minute (frames run for the whole fade).
const DEFAULT_TRANSITION_MS: u32 = 1000;
const MAX_TRANSITION_MS: u32 = 60_000;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Text for the clock: at most CLOCK_TEXT_MAX characters of what the controller will get (the
/// lossy decoding, where each invalid sequence is one U+FFFD). For valid UTF-8 this is the
/// C++ count; for invalid input C++ counted bytes that are not continuation bytes, which let
/// e.g. "a" + 64 stray continuation bytes through as one character.
fn clock_text(b: &[u8]) -> Option<String> {
    let s = lossy(b);
    let n = s.chars().count();
    if n > CLOCK_TEXT_MAX {
        warn!("api: clock text too long ({n} chars)");
        return None;
    }
    Some(s)
}

fn va_event(v: u32) -> VaEvent {
    let e = VaEvent::from_code(v);
    if let VaEvent::Unknown(_) = e {
        debug!("api: unknown voice event type {v}");
    }
    e
}

fn media_command(v: u32) -> Option<MediaCommand> {
    use MediaCommand::*;
    Some(match v {
        0 => Play,
        1 => Pause,
        2 => Stop,
        3 => Mute,
        4 => Unmute,
        5 => Toggle,
        6 => VolumeUp,
        7 => VolumeDown,
        8 => Enqueue,
        9 => RepeatOne,
        10 => RepeatOff,
        11 => ClearPlaylist,
        12 => TurnOn,
        13 => TurnOff,
        _ => return None,
    })
}

fn timer_event(v: u32) -> Option<TimerEvent> {
    Some(match v {
        0 => TimerEvent::Started,
        1 => TimerEvent::Updated,
        2 => TimerEvent::Cancelled,
        3 => TimerEvent::Finished,
        _ => return None,
    })
}

impl Server {
    /// Messages past the transport's own; false: return at once (the C++ server could be gone
    /// after the restart hook).
    pub(super) fn handle_entities(
        &mut self,
        id: u64,
        msg_type: u32,
        r: &mut Reader<'_>,
        len: usize,
    ) -> bool {
        match msg_type {
            msg::DEVICE_INFO_REQ => self.send_device_info(id),
            msg::DEVICE_CAPS_REQ => self.send_device_caps(id),
            msg::LIST_ENTITIES_REQ => self.list_entities(id),
            msg::SUBSCRIBE_STATES_REQ => {
                if let Some(c) = self.client(id) {
                    c.states = true;
                }
                self.send_all_states(id);
            }
            msg::SUBSCRIBE_HA_SERVICES_REQ | msg::SUBSCRIBE_HA_STATES_REQ => {}
            msg::MEDIA_PLAYER_CMD => self.on_media_cmd(r),
            msg::BUTTON_CMD => return self.on_button_cmd(id, r),
            msg::SWITCH_CMD => self.on_switch_cmd(r),
            msg::UPDATE_CMD => {
                let (mut key, mut cmd) = (0, 0);
                while let Some(f) = r.next() {
                    match f.num {
                        1 => key = f.u32(),
                        2 => cmd = f.u32(),
                        _ => {}
                    }
                }
                let cmd = match cmd {
                    0 => Some(UpdateCommand::None),
                    1 => Some(UpdateCommand::Install),
                    2 => Some(UpdateCommand::Check),
                    _ => None,
                };
                if key == key::UPDATE
                    && let Some(cmd) = cmd
                {
                    self.emit(ApiEvent::Update(cmd));
                }
            }
            msg::NUMBER_CMD => self.on_number_cmd(r),
            msg::TEXT_CMD => self.on_text_cmd(r),
            msg::SELECT_CMD => self.on_select_cmd(r),
            msg::EXECUTE_SERVICE_REQ => self.on_execute_service(r),
            msg::LIGHT_CMD => self.on_light_cmd(r),
            msg::VA_EVENT_RESP => self.on_voice_event(id, r),
            msg::VA_TIMER_EVENT_RESP => self.on_timer_event(r),
            msg::VA_ANNOUNCE_REQ => {
                let mut a = AnnounceRequest::default();
                while let Some(f) = r.next() {
                    match f.num {
                        1 => a.media_id = f.str(),
                        2 => a.text = f.str(),
                        3 => a.preannounce_media_id = f.str(),
                        4 => a.start_conversation = f.boolean(),
                        _ => {}
                    }
                }
                info!("api: announce '{}' media={}", a.text, a.media_id);
                self.emit(ApiEvent::Announce(a));
            }
            msg::VA_CONFIG_REQ => self.send_va_config(id),
            msg::VA_SET_CONFIG => self.on_va_set_config(r),
            // HA -> device TTS audio over the API; SPEAKER is not advertised.
            msg::VA_AUDIO => {}
            _ => debug!("api: ignoring message type {msg_type} ({len} bytes)"),
        }
        true
    }

    fn on_button_cmd(&mut self, id: u64, r: &mut Reader<'_>) -> bool {
        let mut k = 0;
        while let Some(f) = r.next() {
            if f.num == 1 {
                k = f.u32();
            }
        }
        let peer = self.client(id).map(|c| c.peer.clone()).unwrap_or_default();
        if k == key::BUTTON_ZIGBEE_JOIN || k == key::BUTTON_ZIGBEE_LEAVE {
            if self.cfg.zigbee_modes.is_empty() {
                return true;
            }
            info!("api: Zigbee button {k} pressed by {peer}");
            let b = if k == key::BUTTON_ZIGBEE_JOIN {
                ButtonId::ZigbeeJoin
            } else {
                ButtonId::ZigbeeLeave
            };
            self.emit(ApiEvent::Button(b));
            return true;
        }
        if k != key::BUTTON_RESTART {
            return true;
        }
        warn!("api: restart requested by {peer}");
        self.emit(ApiEvent::Restart);
        false
    }

    fn on_media_cmd(&mut self, r: &mut Reader<'_>) {
        let mut k = 0;
        let (mut has_cmd, mut cmd) = (false, 0);
        let (mut has_vol, mut vol) = (false, 0.0);
        let (mut has_url, mut url) = (false, String::new());
        let (mut has_ann, mut ann) = (false, false);
        while let Some(f) = r.next() {
            match f.num {
                1 => k = f.u32(),
                2 => has_cmd = f.boolean(),
                3 => cmd = f.u32(),
                4 => has_vol = f.boolean(),
                5 => vol = f.flt(),
                6 => has_url = f.boolean(),
                7 => url = f.str(),
                8 => has_ann = f.boolean(),
                9 => ann = f.boolean(),
                _ => {}
            }
        }
        if k != key::MEDIA {
            return;
        }
        let command = if has_cmd {
            let c = media_command(cmd);
            if c.is_none() {
                debug!("api: unknown media command {cmd}");
            }
            c
        } else {
            None
        };
        self.emit(ApiEvent::MediaCommand(MediaCommandRequest {
            command,
            volume: has_vol.then(|| clamp01(vol)),
            media_url: has_url.then_some(url),
            announcement: has_ann.then_some(ann),
        }));
    }

    fn on_switch_cmd(&mut self, r: &mut Reader<'_>) {
        let (mut k, mut state) = (0, false);
        while let Some(f) = r.next() {
            match f.num {
                1 => k = f.u32(),
                2 => state = f.boolean(),
                _ => {}
            }
        }
        let id = match k {
            key::SW_MICROPHONES => SwitchId::Microphones,
            key::SW_CLOCK => SwitchId::Clock,
            key::SW_AUTO_UPDATE => SwitchId::AutoUpdate,
            key::SW_NEAREST_ANSWERS => SwitchId::NearestAnswers,
            key::SW_VOICE_INDICATION => SwitchId::VoiceIndication,
            _ => return,
        };
        self.emit(ApiEvent::Switch(id, state));
    }

    fn on_number_cmd(&mut self, r: &mut Reader<'_>) {
        let (mut k, mut v) = (0, 0.0f32);
        while let Some(f) = r.next() {
            match f.num {
                1 => k = f.u32(),
                2 => v = f.flt(),
                _ => {}
            }
        }
        if v.is_nan() {
            return;
        }
        let (id, v) = match k {
            key::NUMBER_CLOCK_BRI => (NumberId::ClockBrightness, v.clamp(0.0, 100.0)),
            key::NUMBER_CLOCK_SCROLL => (NumberId::ClockScroll, v.clamp(100.0, 2000.0)),
            key::NUMBER_LED_SPEED => (NumberId::LedSpeed, v.clamp(LED_SPEED_MIN, LED_SPEED_MAX)),
            _ => return,
        };
        self.emit(ApiEvent::Number(id, v));
    }

    fn on_text_cmd(&mut self, r: &mut Reader<'_>) {
        let (mut k, mut s): (u32, &[u8]) = (0, &[]);
        while let Some(f) = r.next() {
            match f.num {
                1 => k = f.u32(),
                2 => s = f.data,
                _ => {}
            }
        }
        if k != key::TEXT_CLOCK {
            return;
        }
        if let Some(s) = clock_text(s) {
            self.emit(ApiEvent::Text(TextId::ClockText, s));
        }
    }

    fn is_clock_face(&self, s: &str) -> bool {
        self.cfg.clock_faces.iter().any(|o| o == s)
    }

    fn on_select_cmd(&mut self, r: &mut Reader<'_>) {
        let (mut k, mut s) = (0, String::new());
        while let Some(f) = r.next() {
            match f.num {
                1 => k = f.u32(),
                2 => s = f.str(),
                _ => {}
            }
        }
        let (id, options, what) = match k {
            key::SELECT_ZIGBEE_MODE => {
                (SelectId::ZigbeeMode, &self.cfg.zigbee_modes, "Zigbee role")
            }
            key::SELECT_CLOCK_MUSIC => (
                SelectId::ClockMusic,
                &self.cfg.clock_music_styles,
                "clock music",
            ),
            key::SELECT_MUSIC_VIZ => (
                SelectId::MusicViz,
                &self.cfg.music_styles,
                "music visualization",
            ),
            key::SELECT_CASE_COLOR => (SelectId::CaseColor, &self.cfg.case_colors, "case color"),
            key::SELECT_CLOCK_FACE => (SelectId::ClockFace, &self.cfg.clock_faces, "clock face"),
            _ => return,
        };
        if !options.contains(&s) {
            warn!("api: unknown {what} '{s}'");
            return;
        }
        self.emit(ApiEvent::Select(id, s));
    }

    fn on_execute_service(&mut self, r: &mut Reader<'_>) {
        struct Arg<'a> {
            s: &'a [u8],
            i: i32,
        }
        let mut k = 0;
        // Only the first three are kept: no action takes more, and the count decides.
        let mut args: Vec<Arg<'_>> = Vec::with_capacity(3);
        let mut n_args = 0usize;
        while let Some(f) = r.next() {
            if f.num == 1 {
                k = f.u32();
            }
            // 3/4 call_id / return_response: actions declare no response.
            if f.num != 2 {
                continue;
            }
            let mut a = Arg { s: &[], i: 0 };
            let mut ar = Reader::new(f.data);
            while let Some(af) = ar.next() {
                match af.num {
                    4 => a.s = af.data,
                    5 => a.i = zigzag32(af.v),
                    2 => a.i = af.v as u32 as i32, // legacy_int
                    _ => {}
                }
            }
            n_args += 1;
            if args.len() < 3 {
                args.push(a);
            }
        }
        let want = if k == key::ACTION_LED_SHOW { 3 } else { 2 };
        if !matches!(
            k,
            key::ACTION_CLOCK_TEXT | key::ACTION_CLOCK_FACE | key::ACTION_LED_SHOW
        ) || n_args != want
        {
            warn!("api: bad action call key={k} args={n_args}");
            return;
        }
        if k == key::ACTION_LED_SHOW {
            let effect = lossy(args[0].s);
            if !effect.is_empty() && effect != "None" && !self.cfg.light_effects.contains(&effect) {
                warn!("api: unknown LED effect '{effect}'");
                return;
            }
            let effect = if effect == "None" {
                String::new()
            } else {
                effect
            };
            self.emit(ApiEvent::LedShow {
                effect,
                color: lossy(args[1].s),
                seconds: args[2].i,
            });
            return;
        }
        let (value, seconds) = (args[0].s, args[1].i);
        if k == key::ACTION_CLOCK_TEXT {
            let Some(value) = clock_text(value) else {
                return;
            };
            self.emit(ApiEvent::Action {
                id: ActionId::ClockShowText,
                value,
                seconds,
            });
        } else {
            let face = lossy(value);
            if !face.is_empty() && !self.is_clock_face(&face) {
                warn!("api: unknown clock face '{face}'");
                return;
            }
            self.emit(ApiEvent::Action {
                id: ActionId::ClockShowFace,
                value: face,
                seconds,
            });
        }
    }

    fn on_light_cmd(&mut self, r: &mut Reader<'_>) {
        let mut k = 0;
        let (mut has_state, mut state) = (false, false);
        let (mut has_bri, mut bri) = (false, 0.0f32);
        let (mut has_cbri, mut cbri) = (false, 1.0f32);
        let (mut has_rgb, mut rgb) = (false, [0.0f32; 3]);
        let (mut has_effect, mut effect) = (false, String::new());
        let (mut has_transition, mut transition) = (false, 0u32);
        while let Some(f) = r.next() {
            match f.num {
                1 => k = f.u32(),
                2 => has_state = f.boolean(),
                3 => state = f.boolean(),
                4 => has_bri = f.boolean(),
                5 => bri = f.flt(),
                20 => has_cbri = f.boolean(),
                21 => cbri = f.flt(),
                6 => has_rgb = f.boolean(),
                7 => rgb[0] = f.flt(),
                8 => rgb[1] = f.flt(),
                9 => rgb[2] = f.flt(),
                18 => has_effect = f.boolean(),
                19 => effect = f.str(),
                14 => has_transition = f.boolean(),
                15 => transition = f.u32(),
                _ => {}
            }
        }
        if k != key::LIGHT_RING {
            return;
        }
        // NaN would reach the LEDs as NaN (the C++ server let it through, clamp01 keeps it):
        // refused like a NaN number.
        if [bri, cbri, rgb[0], rgb[1], rgb[2]]
            .iter()
            .any(|v| v.is_nan())
        {
            warn!("api: light command with NaN ignored");
            return;
        }
        let mut st: LightState = self.cache.light.clone();
        if has_state {
            st.on = state;
        }
        if has_bri {
            st.brightness = clamp01(bri);
        }
        if has_rgb {
            let scale = if has_cbri { clamp01(cbri) } else { 1.0 };
            st.r = clamp01(rgb[0]) * scale;
            st.g = clamp01(rgb[1]) * scale;
            st.b = clamp01(rgb[2]) * scale;
        } else if has_cbri {
            let m = st.r.max(st.g).max(st.b);
            let scale = if m > 0.0 { clamp01(cbri) / m } else { 0.0 };
            st.r *= scale;
            st.g *= scale;
            st.b *= scale;
        }
        if has_effect {
            st.effect = if effect == "None" {
                String::new()
            } else {
                effect
            };
        }
        let transition_ms = if has_transition {
            transition.min(MAX_TRANSITION_MS)
        } else {
            DEFAULT_TRANSITION_MS
        };
        self.emit(ApiEvent::Light {
            state: st.clone(),
            transition_ms,
        });
        self.cache.light = st;
        if self.clients.iter().any(|c| c.hello && c.states) {
            self.send_light(None);
        }
    }

    fn on_voice_event(&mut self, id: u64, r: &mut Reader<'_>) {
        // Pipeline events only count from the pipeline owner.
        if !self.client(id).is_some_and(|c| c.voice) {
            return;
        }
        let mut type_v = 0;
        let mut data = HashMap::new();
        while let Some(f) = r.next() {
            if f.num == 1 {
                type_v = f.u32();
            }
            if f.num == 2 && f.wt == WireType::Len {
                let (mut k, mut v) = (String::new(), String::new());
                let mut sr = Reader::new(f.data);
                while let Some(sf) = sr.next() {
                    match sf.num {
                        1 => k = sf.str(),
                        2 => v = sf.str(),
                        _ => {}
                    }
                }
                data.insert(k, v);
            }
        }
        debug!("api: voice event {type_v}");
        self.emit(ApiEvent::VoiceEvent {
            event: va_event(type_v),
            data,
        });
    }

    fn on_timer_event(&mut self, r: &mut Reader<'_>) {
        let mut t = TimerInfo::default();
        let mut event = 0;
        while let Some(f) = r.next() {
            match f.num {
                1 => event = f.u32(),
                2 => t.id = f.str(),
                3 => t.name = f.str(),
                4 => t.total_seconds = f.u32(),
                5 => t.seconds_left = f.u32(),
                6 => t.is_active = f.boolean(),
                _ => {}
            }
        }
        match timer_event(event) {
            Some(e) => {
                t.event = e;
                self.emit(ApiEvent::Timer(t));
            }
            None => debug!("api: unknown timer event {event} dropped"),
        }
    }

    fn on_va_set_config(&mut self, r: &mut Reader<'_>) {
        let mut ids: Vec<String> = Vec::new();
        let (mut ignored, mut first_ignored) = (0usize, String::new());
        while let Some(f) = r.next() {
            if f.num != 1 || f.wt != WireType::Len {
                continue;
            }
            let ww = f.str();
            let known = self.cfg.wake_words.iter().any(|w| w.id == ww);
            // max_active_wake_words = 1
            if known && ids.is_empty() {
                ids.push(ww);
            } else {
                if ignored == 0 {
                    first_ignored = ww;
                }
                ignored += 1;
            }
        }
        // One line however many came (the C++ server logged each).
        if ignored > 0 {
            warn!("api: ignoring {ignored} wake word(s), first '{first_ignored}'");
        }
        // The spotter always runs one model: an empty / unknown selection leaves the current
        // word active, and the controller reports a switch back through set_active_wake_words.
        if ids.is_empty() {
            warn!("api: no known wake word in the new configuration, keeping the current one");
            return;
        }
        self.emit(ApiEvent::ActiveWakeWords(ids));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_text_counts_what_the_controller_gets() {
        assert_eq!(
            clock_text("я".repeat(64).as_bytes())
                .unwrap()
                .chars()
                .count(),
            64
        );
        assert!(clock_text("я".repeat(65).as_bytes()).is_none());
        // Each stray continuation byte becomes one U+FFFD: "a" + 63 of them is 64 characters,
        // + 64 is too long (C++ counted 1 either way).
        let mut b = b"a".to_vec();
        b.extend_from_slice(&[0x80; 63]);
        assert_eq!(clock_text(&b).unwrap().chars().count(), 64);
        b.push(0x80);
        assert!(clock_text(&b).is_none());
    }

    #[test]
    fn enum_values_from_the_wire() {
        assert_eq!(va_event(100), VaEvent::IntentProgress);
        assert_eq!(va_event(13), VaEvent::Unknown(13));
        for code in [0, 1, 2, 12, 98, 99, 100, 13, 97, 101, 7777] {
            assert_eq!(VaEvent::from_code(code).code(), code);
        }
        assert_eq!(media_command(13), Some(MediaCommand::TurnOff));
        assert_eq!(media_command(14), None);
        assert_eq!(timer_event(3), Some(TimerEvent::Finished));
        assert_eq!(timer_event(4), None);
    }
}
