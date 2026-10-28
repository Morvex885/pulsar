//! HA connection and entities, buttons, the light sensor, Zigbee and updates (controller.cpp
//! "HA connection", "entities", "updates").

use log::{error, info, warn};
use pulsar_core::now_ms;
use pulsar_esphome::{
    ActionId, ApiEvent, BinarySensorId, ButtonId, EventId, LightState, NumberId, SelectId,
    SensorId, SwitchId, TextId, TextSensorId, UpdateCommand, UpdateState,
};
use pulsar_hw::{Button, InputEvent, Press};
use pulsar_media::Channel;
use pulsar_update::UpdateEvent;
use pulsar_zigbee::{Mode, ZigbeeEvent};

use super::logic::{self, AutoUpdate, cxx_clamp, cxx_max, cxx_min};
use super::{
    Controller, ExitReason, Tk, UPDATE_IDLE_RETRY_MS, clock_face_index, light_rgb, model_dir,
};
use crate::version::VERSION;

impl Controller {
    pub(super) fn on_api(&mut self, ev: ApiEvent) {
        match ev {
            ApiEvent::ClientConnected { client_info } => self.on_client_connected(&client_info),
            ApiEvent::ClientDisconnected => {
                warn!("Home Assistant disconnected");
                self.voice_link_lost();
            }
            ApiEvent::VoiceSubscribed(s) => self.on_voice_subscribed(s),
            ApiEvent::VoiceEvent { event, data } => self.on_voice_event(event, &data),
            ApiEvent::Announce(req) => self.on_announce(req),
            ApiEvent::Timer(t) => self.on_timer_event(t),
            ApiEvent::ActiveWakeWords(ids) => self.on_active_wake_words(ids),
            ApiEvent::MediaCommand(cmd) => self.on_media_command(cmd),
            ApiEvent::Switch(id, on) => self.on_switch(id, on),
            ApiEvent::Button(id) => self.on_button(id),
            ApiEvent::Light {
                state,
                transition_ms,
            } => self.on_light(state, transition_ms),
            ApiEvent::Number(id, v) => self.on_number(id, v),
            ApiEvent::Text(id, s) => self.on_text(id, s),
            ApiEvent::Select(id, s) => self.on_select(id, s),
            ApiEvent::Action { id, value, seconds } => self.on_action(id, value, seconds),
            ApiEvent::LedShow {
                effect,
                color,
                seconds,
            } => self.on_led_show(effect, &color, seconds),
            ApiEvent::Time { epoch, posix_tz } => self.on_time(epoch, &posix_tz),
            ApiEvent::Restart => self.on_restart(),
            ApiEvent::VoiceStartFailed { run } => self.on_voice_start_failed(run),
            ApiEvent::VoiceAudioOverflow { run } => self.on_voice_audio_overflow(run),
            ApiEvent::Update(cmd) => self.on_update(cmd),
            ApiEvent::MdnsTxtChanged(txt) => {
                if let Some(m) = &self.mdns {
                    m.set_txt("_esphomelib._tcp", txt);
                }
            }
        }
    }

    fn on_client_connected(&mut self, client_info: &str) {
        info!("Home Assistant connected: {client_info}");
        self.error_shown = false;
        self.publish_media();
        self.publish_microphones();
        let api = &self.api;
        api.publish_switch(SwitchId::Clock, self.st.clock_on);
        api.publish_switch(SwitchId::AutoUpdate, self.st.auto_update);
        api.publish_switch(SwitchId::NearestAnswers, self.st.nearest_answers);
        api.publish_switch(SwitchId::VoiceIndication, self.st.voice_indication);
        api.publish_switch(SwitchId::WakePreroll, self.st.wake_preroll);
        self.publish_update();
        let api = &self.api;
        api.publish_binary_sensor(BinarySensorId::HwMicMute, self.hw_mic_muted);
        api.publish_number(NumberId::ClockBrightness, self.st.clock_brightness * 100.0);
        api.publish_number(NumberId::ClockScroll, self.st.clock_scroll_ms as f32);
        api.publish_text(TextId::ClockText, &self.clock_text);
        api.publish_select(SelectId::ClockFace, &self.clock_face);
        api.publish_number(NumberId::LedSpeed, self.st.led_speed);
        api.publish_select(SelectId::MusicViz, &self.st.music_viz);
        api.publish_select(SelectId::ClockMusic, &self.st.clock_music);
        api.publish_select(SelectId::CaseColor, self.case_color_option());
        if let Some(z) = &self.zigbee {
            api.publish_select(
                SelectId::ZigbeeMode,
                pulsar_zigbee::role(self.st.zigbee_role).label,
            );
            // Not through the "zigbee: …" dedup of Changed: its cache stays as it is.
            api.publish_text_sensor(TextSensorId::Zigbee, &z.status());
        }
        api.publish_light(&self.st.light);
        if self.last_lux_published >= 0.0 {
            api.publish_sensor(SensorId::Illuminance, self.last_lux_published);
        }
    }

    fn case_color_option(&self) -> &str {
        if self.st.led_case_color.is_empty() {
            "None"
        } else {
            &self.st.led_case_color
        }
    }

    fn on_time(&mut self, epoch: u32, tz: &str) {
        if !tz.is_empty() {
            // Only on a change: setenv races with threads reading the environment (GLib).
            if tz != self.applied_tz {
                pulsar_core::tz::set_tz(tz);
                self.applied_tz = tz.to_owned();
            }
            if tz != self.st.tz {
                self.st.tz = tz.to_owned();
                self.persist();
            }
        }
        info!("time from HA: epoch={epoch} tz='{tz}' (local clock now follows TZ)");
    }

    fn on_restart(&mut self) {
        // A restart cuts the upload short (the shutdown watchdog does not wait ~25 s); the radio
        // would be written again on the next start, but stays without Zigbee meanwhile.
        if self.zigbee_flashing() {
            warn!(
                "restart requested from Home Assistant: refused while the Zigbee radio is being written"
            );
            return;
        }
        warn!("restart requested from Home Assistant");
        self.exit = Some(ExitReason::Restart);
    }

    fn on_switch(&mut self, id: SwitchId, state: bool) {
        match id {
            SwitchId::Microphones => {
                self.st.mic_muted =
                    logic::sw_mute_after_switch(state, self.st.mic_muted, self.hw_mic_muted);
                if !state {
                    self.cancel_arbitration();
                    if self.phase == logic::Phase::Listening {
                        self.finish_interaction();
                    }
                }
                self.update_detection();
                self.update_ui();
                // The actual state, not the command: "on" under the button mute comes back off.
                self.publish_microphones();
                // Privacy: a mute must not be undone by a power cut right after it. A failed
                // write is logged and stays pending; the mute itself is in effect either way.
                let _ = self.persist_now();
                return;
            }
            SwitchId::Clock => {
                self.st.clock_on = state;
                self.clock.set_enabled(state);
                self.update_music();
            }
            SwitchId::AutoUpdate => self.st.auto_update = state,
            SwitchId::NearestAnswers => self.st.nearest_answers = state,
            SwitchId::VoiceIndication => {
                self.st.voice_indication = state;
                self.leds.set_voice_indication(state);
                self.clock.set_voice_indication(state);
            }
            SwitchId::WakePreroll => {
                self.st.wake_preroll = state;
                self.mic.set_preroll(state);
            }
        }
        self.api.publish_switch(id, state);
        self.persist();
        if id == SwitchId::AutoUpdate {
            self.maybe_auto_update();
        }
    }

    /// "Microphones" is on while the speaker listens: neither the mute from HA nor the mute
    /// button's.
    fn publish_microphones(&self) {
        self.api
            .publish_switch(SwitchId::Microphones, !self.mic_muted());
    }

    fn on_number(&mut self, id: NumberId, value: f32) {
        match id {
            NumberId::ClockBrightness => {
                self.st.clock_brightness = cxx_min(100.0, cxx_max(0.0, value)) / 100.0;
                self.clock.set_brightness(self.st.clock_brightness);
                self.api
                    .publish_number(id, self.st.clock_brightness * 100.0);
            }
            NumberId::ClockScroll => {
                // C++ int(lround(value / 50)) * 50 (Rust saturates where C++ is undefined).
                let steps = (value / 50.0).round() as i64 as i32;
                self.st.clock_scroll_ms = steps.wrapping_mul(50).clamp(100, 2000);
                self.clock.set_scroll_ms(self.st.clock_scroll_ms);
                self.api.publish_number(id, self.st.clock_scroll_ms as f32);
            }
            NumberId::LedSpeed => {
                self.st.led_speed = cxx_clamp(value, 0.25, 4.0);
                self.leds.set_speed(self.st.led_speed);
                self.api.publish_number(id, self.st.led_speed);
            }
        }
        self.persist();
    }

    fn on_text(&mut self, id: TextId, value: String) {
        self.clock.set_text(value.clone());
        self.api.publish_text(id, &value);
        // Text and face replace each other on the display; the clock already dropped the face.
        let clears_face = !value.is_empty() && self.clock_face != "None";
        self.clock_text = value;
        if clears_face {
            self.clock_face = "None".into();
            self.api
                .publish_select(SelectId::ClockFace, &self.clock_face);
        }
    }

    fn on_select(&mut self, id: SelectId, option: String) {
        match id {
            SelectId::ZigbeeMode => {
                if self.zigbee.is_none() {
                    return;
                }
                let role = pulsar_zigbee::ROLES
                    .iter()
                    .find(|r| r.label == option)
                    .map_or(self.st.zigbee_role, |r| r.mode);
                if role == self.st.zigbee_role {
                    return self.api.publish_select(id, pulsar_zigbee::role(role).label);
                }
                // The role starts with the daemon, which writes the radio firmware it needs; the
                // coordinator's mDNS advertisement is only made in that role.
                warn!(
                    "zigbee: role {} chosen, restarting",
                    pulsar_zigbee::role(role).name
                );
                self.st.zigbee_role = role;
                self.st.zigbee_flash_failed = 0;
                // Before the restart: the next start acts on the role. A failed write stays
                // pending for the exit flush after run().
                let _ = self.persist_now();
                self.api.publish_select(id, pulsar_zigbee::role(role).label);
                self.exit = Some(ExitReason::ZigbeeRoleChanged);
            }
            SelectId::MusicViz => {
                self.leds.set_music_style(option.clone());
                self.api.publish_select(id, &option);
                self.st.music_viz = option;
                self.persist();
                self.update_music();
            }
            SelectId::ClockMusic => {
                self.clock.set_music_style(option.clone());
                self.api.publish_select(id, &option);
                self.st.clock_music = option;
                self.persist();
                self.update_music();
            }
            SelectId::CaseColor => {
                let stem = if option == "None" { "" } else { &option };
                self.st.led_case_color = if self.leds.set_case_color(stem) {
                    stem.to_owned()
                } else {
                    String::new()
                };
                self.api.publish_select(id, self.case_color_option());
                self.persist();
            }
            SelectId::ClockFace => {
                self.clock.set_face(clock_face_index(&option));
                self.api.publish_select(id, &option);
                let clears_text = option != "None" && !self.clock_text.is_empty();
                self.clock_face = option;
                if clears_text {
                    self.clock_text.clear();
                    self.api.publish_text(TextId::ClockText, &self.clock_text);
                }
            }
        }
    }

    fn on_button(&mut self, id: ButtonId) {
        let Some(z) = &self.zigbee else {
            return;
        };
        if z.mode() != Mode::Router {
            warn!("zigbee: join/leave only apply to the router role");
            return;
        }
        match id {
            ButtonId::ZigbeeJoin => z.join(),
            ButtonId::ZigbeeLeave => z.leave(),
        }
    }

    fn on_action(&mut self, id: ActionId, value: String, seconds: i32) {
        let text = id == ActionId::ClockShowText;
        info!(
            "clock: temporary {} '{value}' for {seconds} s",
            if text { "text" } else { "face" }
        );
        if text {
            self.clock.show_temporary_text(value, seconds);
        } else {
            self.clock
                .show_temporary_face(clock_face_index(&value), seconds);
        }
    }

    fn on_led_show(&mut self, effect: String, color: &str, seconds: i32) {
        if effect.is_empty() && color.is_empty() {
            return self.leds.clear_temporary();
        }
        let Some(rgb) = logic::parse_led_color(color) else {
            warn!("led_show: bad colour '{color}'");
            return;
        };
        info!(
            "leds: show '{effect}' #{:02x}{:02x}{:02x}, {seconds} s requested",
            rgb.r, rgb.g, rgb.b
        );
        self.leds.show_temporary(effect, rgb, seconds);
    }

    fn on_light(&mut self, l: LightState, transition_ms: u32) {
        let effect = if l.effect == "None" {
            String::new()
        } else {
            l.effect.clone()
        };
        self.leds
            .set_user_light(l.on, light_rgb(&l), l.brightness, effect, transition_ms);
        self.st.light = l;
        self.persist();
        self.update_music();
    }

    fn on_active_wake_words(&mut self, ids: Vec<String>) {
        let Some(first) = ids.into_iter().next() else {
            return;
        };
        if first == self.st.wake_word {
            return;
        }
        self.st.wake_word = first;
        info!("wake word -> {}", self.st.wake_word);
        self.mic.set_model(model_dir(&self.st.wake_word));
        self.api
            .set_active_wake_words(vec![self.st.wake_word.clone()]);
        self.persist();
    }

    // ---------------- buttons, sensors, Zigbee ----------------

    pub(super) fn on_input(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Mute(m) => {
                if m {
                    self.cancel_arbitration();
                }
                self.hw_mic_muted = m;
                info!("hardware mic mute: {}", u8::from(m));
                self.api.publish_binary_sensor(BinarySensorId::HwMicMute, m);
                let sw_muted = logic::sw_mute_after_button(self.st.mic_muted, m);
                if sw_muted != self.st.mic_muted {
                    info!("mute button lifted the mute from Home Assistant");
                    self.st.mic_muted = sw_muted;
                    // A lost write leaves the speaker muted after a restart (the safe side).
                    self.persist();
                }
                self.publish_microphones();
                if m && self.phase == logic::Phase::Listening {
                    self.finish_interaction();
                }
                self.update_detection();
                self.update_ui();
            }
            InputEvent::Button(Button::Action, p) => {
                self.api.fire_event(
                    EventId::Button,
                    if p == Press::Long {
                        "action_long"
                    } else {
                        "action"
                    },
                );
                if p == Press::Long {
                    return; // exposed to HA automations only
                }
                if self.sendspin_pairing_prompt
                    && let Some(ss) = &self.sendspin
                {
                    return ss.confirm_pairing_window();
                }
                self.action_button();
            }
            InputEvent::Button(b, _) => {
                let up = b == Button::VolumeUp;
                self.api.fire_event(
                    EventId::Button,
                    if up { "volume_up" } else { "volume_down" },
                );
                self.set_volume(
                    pulsar_media::mixer::step_level(self.model, self.st.volume, up),
                    true,
                );
            }
        }
    }

    pub(super) fn on_lux(&mut self, lux: f32) {
        self.clock.set_ambient_lux(lux);
        self.leds.set_ambient_lux(lux);
        let now = now_ms();
        if logic::publish_lux(lux, self.last_lux_published, self.last_lux_ms, now) {
            self.api.publish_sensor(SensorId::Illuminance, lux);
            self.last_lux_published = lux;
            self.last_lux_ms = now;
        }
    }

    /// The status is read only here and when a client connects (as the C++ hook and
    /// on_client_connected); right after join()/leave() it may still be the previous one.
    pub(super) fn on_zigbee(&mut self, ev: ZigbeeEvent) {
        match ev {
            ZigbeeEvent::Changed => {
                let Some(z) = &self.zigbee else {
                    return;
                };
                let s = z.status();
                if s == self.zigbee_status {
                    return;
                }
                info!("zigbee: {s}");
                self.api.publish_text_sensor(TextSensorId::Zigbee, &s);
                self.zigbee_status = s;
            }
            ZigbeeEvent::FlashOutcome { failed_version } => {
                if failed_version == self.st.zigbee_flash_failed {
                    return;
                }
                self.st.zigbee_flash_failed = failed_version;
                // An image the radio refused is not written again after a crash or restart. A
                // failed write stays pending (logged).
                let _ = self.persist_now();
            }
        }
    }

    // ---------------- updates ----------------

    pub(super) fn publish_update(&self) {
        let release = self.release.as_ref();
        let version = release.map_or("", |r| r.version.as_str());
        // latest == current hides the offer: nothing newer, or not checked yet.
        let offer = !version.is_empty() && pulsar_update::release_newer(version, VERSION).0;
        self.api.publish_update(&UpdateState {
            in_progress: self.update_installing,
            has_progress: self.update_installing,
            progress: self.update_progress,
            current_version: VERSION.into(),
            latest_version: if offer { version } else { VERSION }.into(),
            title: "pulsar".into(),
            release_url: release.map_or_else(String::new, |r| r.url.clone()),
        });
    }

    pub(super) fn check_update(&mut self) {
        // None: a request is in flight (C++ Updater::check returned silently).
        if let Some(id) = self.updater.check() {
            self.pending_check = Some(id);
        }
    }

    fn release_version(&self) -> &str {
        self.release.as_ref().map_or("", |r| r.version.as_str())
    }

    fn idle_for_update(&self) -> bool {
        logic::idle_for_update(
            self.phase,
            self.announcing,
            self.timer_ringing,
            self.player.state(Channel::Music),
            self.sendspin_streaming(),
            self.zigbee_flashing(),
        )
    }

    pub(super) fn maybe_auto_update(&mut self) {
        self.timers.cancel(Tk::UpdateRetry);
        match logic::auto_update(
            self.st.auto_update,
            self.release_version(),
            self.update_installing,
            VERSION,
            &self.st.update_attempt,
            self.idle_for_update(),
        ) {
            AutoUpdate::No => {}
            AutoUpdate::WaitIdle => self.timers.set(Tk::UpdateRetry, UPDATE_IDLE_RETRY_MS),
            AutoUpdate::Install => self.install_update("automatic"),
        }
    }

    fn install_update(&mut self, why: &str) {
        // A check not handled yet would still replace the release (C++ busy() until then).
        if self.updater.busy() || self.update_installing || self.pending_check.is_some() {
            return;
        }
        if self.zigbee_flashing() {
            // The install ends with a restart.
            warn!("update: not while the Zigbee radio is being written");
            return;
        }
        let Ok(exe) = std::fs::read_link("/proc/self/exe") else {
            error!("update: cannot find the running binary");
            return;
        };
        let binary = std::path::PathBuf::from(logic::strip_deleted(exe.as_os_str()));
        let Some(r) = self.release.clone() else {
            return;
        };
        warn!("installing release {} ({why})", r.version);
        let previous = std::mem::replace(&mut self.st.update_attempt, r.version.clone());
        // On disk before the updater can replace the binary: still set under another version
        // after a restart, it tells that the release was rolled back. Without it a bad release
        // would carry no marker and automatic updates would install it again.
        if !self.persist_now() {
            warn!(
                "update to {}: update_attempt not written to state.conf, install refused",
                r.version
            );
            self.st.update_attempt = previous;
            self.install_failed();
            return;
        }
        self.update_installing = true;
        self.clock.set_updating(true);
        self.update_progress = 0.0;
        self.publish_update();
        self.install_version.clone_from(&r.version);
        if let Some(id) = self.updater.install(r, binary) {
            self.install_id = Some(id);
        }
    }

    fn on_update(&mut self, cmd: UpdateCommand) {
        match cmd {
            UpdateCommand::Check => self.check_update(),
            UpdateCommand::Install => {
                let v = self.release_version();
                if v.is_empty() || v == VERSION {
                    warn!("update: no other release to install");
                    return;
                }
                self.install_update("requested from Home Assistant");
            }
            UpdateCommand::None => {}
        }
    }

    /// Every Checked / Installed is handled (the C++ callbacks carried no sequence number);
    /// the ids only tell when the request in flight has been answered.
    pub(super) fn on_updater(&mut self, ev: UpdateEvent) {
        match ev {
            UpdateEvent::Checked { id, result } => {
                if self.pending_check == Some(id) {
                    self.pending_check = None;
                }
                match result {
                    Err(e) => info!("update check: {e}"),
                    Ok(r) => {
                        if r.version != self.release_version() {
                            info!("latest release {} (running {VERSION})", r.version);
                        }
                        self.release = Some(r);
                        self.publish_update();
                        self.maybe_auto_update();
                    }
                }
            }
            UpdateEvent::Progress { value, .. } => {
                self.update_progress = value;
                self.publish_update();
            }
            UpdateEvent::Installed { id, result } => {
                if self.install_id == Some(id) {
                    self.install_id = None;
                }
                let version = std::mem::take(&mut self.install_version);
                match result {
                    Ok(()) => {
                        warn!("installed {version}, restarting");
                        self.update_progress = 100.0;
                        self.publish_update();
                        self.timers.set(Tk::UpdateExit, 500);
                    }
                    Err((err, fatal)) => {
                        error!("update to {version} failed: {err}");
                        if !fatal {
                            // A network failure: the next check tries again.
                            self.st.update_attempt.clear();
                            // Left set by a crash, it would make automatic updates skip it. A
                            // failed write stays pending (logged).
                            let _ = self.persist_now();
                        }
                        self.install_failed();
                    }
                }
            }
        }
    }

    /// An install that ended without a new binary, or never started: HA sees the update entity
    /// idle again, the clock shows the time.
    fn install_failed(&mut self) {
        self.update_installing = false;
        self.clock.set_updating(false);
        self.publish_update();
    }
}
