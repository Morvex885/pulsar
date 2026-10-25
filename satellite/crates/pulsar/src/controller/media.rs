//! Volume, mute, ducking, the HA media player and the Sendspin hooks (controller.cpp "media").

use log::debug;
use pulsar_core::{ClockShot, now_ms};
use pulsar_esphome::{MediaCommand, MediaCommandRequest};
use pulsar_media::{Channel, PlayState};
use pulsar_sendspin::SendspinEvent;

use super::logic::{self, cxx_max, cxx_min};
use super::{Controller, DUCK_GAIN, Pending, SND_VOLUME};

impl Controller {
    pub(super) fn update_duck(&mut self) {
        let want = logic::want_duck(
            self.phase,
            self.announcing,
            self.timer_ringing,
            self.closing_earcon,
        );
        if want == self.ducked {
            return;
        }
        self.ducked = want;
        self.apply_gains();
    }

    /// Mute: softvol raw 0 is the bottom of its dB range (-60 dB on the Midi), still audible, so
    /// mute also zeroes the software gains of both playback channels and Sendspin.
    pub(super) fn apply_gains(&self) {
        let music = if self.st.muted {
            0.0
        } else if self.ducked {
            DUCK_GAIN
        } else {
            1.0
        };
        self.player.set_channel_gain(Channel::Music, music);
        self.player
            .set_channel_gain(Channel::Announce, if self.st.muted { 0.0 } else { 1.0 });
        if let Some(ss) = &self.sendspin {
            ss.set_duck_gain(music);
        }
    }

    pub(super) fn set_muted(&mut self, m: bool) {
        self.st.muted = m;
        self.mixer.set_muted(m);
        self.apply_gains();
        if let Some(ss) = &self.sendspin {
            ss.report_volume(self.st.volume, self.st.muted);
        }
        self.publish_media();
        self.persist();
    }

    /// `feedback`: a local change (buttons, voice), shown on the ring and the clock; it also
    /// unmutes.
    pub(super) fn set_volume(&mut self, v: f32, feedback: bool) {
        let before = (self.st.volume, self.st.muted);
        self.st.volume = cxx_min(1.0, cxx_max(0.0, v));
        self.mixer.set_volume(self.st.volume);
        if self.st.muted && feedback {
            self.st.muted = false;
            self.mixer.set_muted(false);
            self.apply_gains();
        }
        if feedback {
            self.leds.show_volume(self.st.volume);
            // mul_add: the station C++ fuses `volume * 100 + 0.5` (fmadd on aarch64, vfma.f32 on
            // armhf, checked in the -O2 asm of Controller::set_volume); the rounding follows it.
            self.clock
                .flash_number(self.st.volume.mul_add(100.0, 0.5) as i32);
            self.volume_click(before);
        }
        if let Some(ss) = &self.sendspin {
            ss.report_volume(self.st.volume, self.st.muted);
        }
        self.publish_media();
        self.persist();
    }

    /// Stock click after a local change (keys, voice words); `before` = (volume, muted) before it.
    fn volume_click(&self, before: (f32, bool)) {
        if !logic::want_volume_click(
            before,
            (self.st.volume, self.st.muted),
            self.phase,
            self.arbitrating,
            self.announcing,
            self.timer_ringing,
            self.closing_earcon,
        ) {
            return;
        }
        // Music is not ducked under it; its Done has no Pending entry and is ignored.
        self.player.play(Channel::Announce, vec![SND_VOLUME.into()]);
    }

    /// Reads the player state as it is now: right after a command to the player it is still the
    /// previous one (as in C++); the change comes as `PlayerEvent::State`, which publishes again.
    pub(super) fn publish_media(&self) {
        let s = logic::media_state(
            self.announcing,
            self.player.state(Channel::Music),
            self.sendspin_streaming(),
        );
        self.api.publish_media(s, self.st.volume, self.st.muted);
        self.update_music();
    }

    pub(super) fn update_music(&self) {
        let ps = self.player.state(Channel::Music);
        let media =
            matches!(ps, PlayState::Playing | PlayState::Loading) || self.sendspin_streaming();
        self.leds.set_media_playing(media);
        self.clock.set_media_playing(media);
        // The speaker-reference analysis only runs while its result can be shown.
        self.mic.set_music_analysis(
            media
                && ((self.st.music_viz != "Off" && !self.st.light.on)
                    || (self.st.clock_music != "Off" && self.st.clock_on)),
        );
    }

    pub(super) fn on_media_command(&mut self, cmd: MediaCommandRequest) {
        if let Some(url) = cmd.media_url {
            if cmd.announcement == Some(true) {
                // Media-player announcements report completion through state only.
                self.announcing = true;
                self.announce_seq += 1;
                let seq = self.announce_seq;
                self.update_duck();
                self.update_ui();
                let id = self.player.play(Channel::Announce, vec![url]);
                self.pending.insert(id, Pending::MediaAnnounce { seq });
            } else {
                if let Some(ss) = &self.sendspin {
                    ss.yield_to_local(); // HA media takes the speaker from Sendspin
                }
                self.player.play(Channel::Music, vec![url]);
            }
        }
        if let Some(v) = cmd.volume {
            self.set_volume(v, false);
        }
        if let Some(c) = cmd.command {
            let ss_on = self.sendspin_streaming();
            let music = || self.player.state(Channel::Music);
            match c {
                MediaCommand::Play => {
                    if ss_on
                        && music() == PlayState::Paused
                        && let Some(ss) = &self.sendspin
                    {
                        ss.yield_to_local();
                    }
                    self.player.resume(Channel::Music);
                }
                MediaCommand::Pause => {
                    if ss_on || matches!(music(), PlayState::Playing | PlayState::Loading) {
                        self.clock.play(ClockShot::Pause);
                    }
                    // While Sendspin plays, HA's pause/stop leave the Sendspin group (the
                    // speaker stops).
                    if ss_on
                        && music() != PlayState::Playing
                        && let Some(ss) = &self.sendspin
                    {
                        ss.yield_to_local();
                    }
                    self.player.pause(Channel::Music);
                }
                MediaCommand::Toggle => {
                    if music() == PlayState::Playing {
                        self.clock.play(ClockShot::Pause);
                        self.player.pause(Channel::Music);
                    } else if ss_on {
                        self.clock.play(ClockShot::Pause);
                        if let Some(ss) = &self.sendspin {
                            ss.yield_to_local();
                        }
                    } else {
                        self.player.resume(Channel::Music);
                    }
                }
                MediaCommand::Stop => {
                    self.player.stop(Channel::Music);
                    if ss_on && let Some(ss) = &self.sendspin {
                        ss.yield_to_local();
                    }
                }
                MediaCommand::Mute | MediaCommand::Unmute => {
                    self.set_muted(c == MediaCommand::Mute);
                }
                MediaCommand::VolumeUp => self.set_volume(self.st.volume + 0.05, false),
                MediaCommand::VolumeDown => self.set_volume(self.st.volume - 0.05, false),
                _ => debug!("media command {} ignored", c as u32),
            }
        }
        self.publish_media();
    }

    pub(super) fn on_sendspin(&mut self, ev: SendspinEvent) {
        match ev {
            SendspinEvent::Stream(active) => {
                // Music Assistant took over the speaker: the HA media channel yields to it.
                if active && self.player.state(Channel::Music) != PlayState::Idle {
                    self.player.stop(Channel::Music);
                }
                // The HA media channel playing here took the speaker over (yield_to_local): not a
                // pause in Music Assistant. on_player clears the stamp if its state comes later.
                let taken_over = matches!(
                    self.player.state(Channel::Music),
                    PlayState::Playing | PlayState::Loading
                );
                self.sendspin_ended_ms = (!active && !taken_over).then(now_ms);
                self.publish_media();
            }
            SendspinEvent::Volume(v) => self.set_volume(v, false),
            SendspinEvent::Mute(m) => self.set_muted(m),
            SendspinEvent::PairingCode(code) => {
                if code.is_empty() {
                    self.clock.clear_code();
                } else {
                    self.clock.show_code(code);
                }
            }
            SendspinEvent::PairingWindow(open) => {
                self.sendspin_pairing_prompt = open;
                if open {
                    self.clock.show_code("----".into());
                } else {
                    self.clock.clear_code();
                }
            }
            SendspinEvent::Music(f) => {
                self.leds.music_frame(f, true);
                self.clock.music_frame(f, true);
            }
        }
    }
}
