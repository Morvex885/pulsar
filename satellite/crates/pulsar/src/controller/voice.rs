//! Voice pipeline, wake word arbitration, announcements, HA timers and local voice commands
//! (controller.cpp "voice pipeline", "timers", on_command).

use std::collections::HashMap;

use log::{debug, info, warn};
use pulsar_core::{ClockShot, UiState, now_ms};
use pulsar_esphome::{AnnounceRequest, TimerEvent, TimerInfo, VaEvent};
use pulsar_lan::arbiter::Claim;
use pulsar_media::{Channel, PlayState};
use pulsar_sendspin::Transport;

use super::logic::{self, COMMAND_AFTER_WAKE_MS, HaTimer, Phase, VoiceCommand};
use super::{
    Controller, LISTEN_TIMEOUT_MS, PROCESS_TIMEOUT_MS, Pending, SOUNDS, TIMER_RING_MAX_MS, Tk,
};

impl Controller {
    fn arbitration_on(&self) -> bool {
        self.st.nearest_answers && self.arbiter.ok()
    }

    /// Plays `urls` on ANNOUNCE; `done` runs on its end.
    fn play_announce(&mut self, urls: Vec<String>, done: Pending) {
        let id = self.player.play(Channel::Announce, urls);
        self.pending.insert(id, done);
    }

    /// The C++ completion callbacks of `play()`.
    pub(super) fn play_done(&mut self, p: Pending, ok: bool) {
        match p {
            Pending::Tts { dialog } => {
                if dialog == self.dialog && self.phase == Phase::Speaking {
                    self.tts_finished(ok);
                }
                self.resume_ring();
            }
            Pending::Earcon { seq } => {
                if seq != self.earcon_seq {
                    return;
                }
                self.closing_earcon = false;
                self.update_duck();
            }
            Pending::Announce { conv, seq } => {
                // Exactly once per announcement, also when cut short.
                self.api.voice_announce_finished(ok);
                if seq == self.announce_seq {
                    self.announcing = false;
                    if ok
                        && conv
                        && self.phase == Phase::Idle
                        && !self.timer_ringing
                        && !self.mic_muted()
                    {
                        self.start_pipeline("", false, Vec::new());
                    }
                    self.end_announce();
                }
                self.resume_ring();
            }
            Pending::MediaAnnounce { seq } => {
                if seq == self.announce_seq {
                    self.announcing = false;
                    self.end_announce();
                }
                self.resume_ring();
            }
            Pending::Ring => {
                if self.timer_ringing && !self.timers.is_set(Tk::RingTick) {
                    self.timers.set(Tk::RingTick, 300);
                }
            }
        }
    }

    // ---------------- connection ----------------

    pub(super) fn on_voice_subscribed(&mut self, subscribed: bool) {
        info!(
            "voice assistant {}",
            if subscribed {
                "subscribed"
            } else {
                "unsubscribed"
            }
        );
        if !subscribed {
            self.voice_link_lost();
        }
    }

    /// No more events come for the current run, and a stop would reach whichever client
    /// subscribes next. A dialog still listening or thinking ends now. An answer already playing
    /// is fetched over HTTP independently of the API link, so it plays out (still ducked) and its
    /// end closes the dialog; a stream that dies with HA ends it through the player instead. The
    /// AnnounceFinished owed for the answer belongs to the lost client: the next one could be
    /// waiting for an announcement, which that message would end.
    pub(super) fn voice_link_lost(&mut self) {
        self.run.reset();
        self.continue_conversation = false;
        self.tts_report_owed = false;
        if matches!(self.phase, Phase::Listening | Phase::Processing) {
            self.finish_interaction();
        }
    }

    // ---------------- wake word ----------------

    pub(super) fn on_wake(&mut self, phrase: &str, level_db: f32, preroll: Vec<i16>) {
        if self.mic_muted() {
            return;
        }
        info!("wake word '{phrase}', level {level_db:.1} dB");
        if self.timer_ringing {
            // The wake word works as a stop word while ringing. A ringing station takes the
            // turn so that its neighbours do not start a dialog.
            self.cancel_arbitration();
            if self.arbitration_on() {
                self.arbiter
                    .claim(&self.st.wake_word, level_db, true, false);
            }
            return self.stop_timer_ring();
        }
        if matches!(self.phase, Phase::Listening | Phase::Processing) || self.arbitrating {
            return;
        }
        // Without the HA pipeline this station could win and stay silent: it does not take part.
        if !self.arbitration_on() || !self.api.voice_subscribed() {
            return self.answer_wake(phrase, preroll);
        }
        self.arbitrating = true;
        // A station that lost the previous wake word may win this one.
        self.commands_blocked_until_ms = 0;
        self.arb_phrase = phrase.to_owned();
        self.arb_level = level_db;
        self.arb_audio = preroll;
        self.arb_command.clear();
        self.mic.set_streaming(true);
        self.update_ui();
        let busy = self.phase == Phase::Speaking || self.announcing;
        self.arb_claim = Some(self.arbiter.claim(&self.st.wake_word, level_db, busy, true));
    }

    fn answer_wake(&mut self, phrase: &str, preroll: Vec<i16>) {
        if self.timer_ringing {
            return self.stop_timer_ring();
        }
        if matches!(self.phase, Phase::Listening | Phase::Processing) {
            return;
        }
        if self.phase == Phase::Speaking || self.announcing {
            self.interrupt_speech(); // barge-in: cut the answer short
        }
        self.start_pipeline(phrase, true, preroll);
    }

    pub(super) fn arbitration_done(&mut self, won: bool, best: &Claim, peers: usize) {
        self.arbitrating = false;
        self.arb_claim = None;
        let audio = std::mem::take(&mut self.arb_audio);
        let command = std::mem::take(&mut self.arb_command);
        if !won {
            self.mic.set_streaming(false);
            info!(
                "wake word: {} answers ({}{:.1} dB, ours {:.1} dB)",
                best.station,
                if best.busy { "busy, " } else { "" },
                best.level_db,
                self.arb_level
            );
            // Its command spotter would still act on "Алиса, громче" if it sounds or owns music.
            self.commands_blocked_until_ms = now_ms() + COMMAND_AFTER_WAKE_MS;
            self.update_ui();
            return;
        }
        info!("wake word: this station answers ({peers} other station(s) heard it)");
        if !command.is_empty() {
            // "Алиса, громче": handled locally, as right after the wake word.
            self.mic.set_streaming(false);
            self.update_ui();
            return self.on_command(&command, true);
        }
        // Capture keeps streaming into the new dialog: switching it off here could drop the
        // chunk between the preroll and live audio.
        let phrase = self.arb_phrase.clone();
        self.answer_wake(&phrase, audio);
        if self.phase != Phase::Listening {
            self.mic.set_streaming(false); // no dialog started (e.g. HA gone)
        }
    }

    pub(super) fn cancel_arbitration(&mut self) {
        if !self.arbitrating {
            return;
        }
        self.arbiter.cancel();
        self.arbitrating = false;
        self.arb_claim = None;
        self.arb_audio.clear();
        self.arb_command.clear();
        if self.phase != Phase::Listening {
            self.mic.set_streaming(false);
        }
    }

    // ---------------- pipeline ----------------

    /// The only way into LISTENING (the watchdog bounds it).
    pub(super) fn start_pipeline(&mut self, phrase: &str, earcon: bool, preroll: Vec<i16>) {
        // The button, a continued conversation or HA's start_conversation win.
        self.cancel_arbitration();
        if !self.api.voice_subscribed() {
            warn!("wake ignored: Home Assistant voice assistant not subscribed");
            self.show_ui(UiState::Error);
            self.timers.set(Tk::UiAfterNotSubscribed, 1500);
            return;
        }
        info!("starting voice pipeline (wake='{phrase}')");
        self.commands_blocked_until_ms = 0;
        self.phase = Phase::Listening;
        self.dialog += 1;
        self.run.start();
        self.stop_sent = false;
        self.listen_started_ms = now_ms();
        self.tts_url.clear();
        self.tts_played = false;
        self.continue_conversation = false;
        self.update_duck();
        if earcon {
            let sound = format!("{SOUNDS}{}", self.model.wake_sound);
            self.player.play(Channel::Announce, vec![sound]);
        }
        // HA applies AnnounceFinished by forcing idle: an answer's report must not arrive over
        // the new dialog's listening state.
        self.send_tts_report(false);
        // A failed send comes back as VoiceStartFailed / VoiceAudioOverflow of this run.
        self.voice_run = self.api.voice_request_start(phrase);
        // HA drops audio queued before the start and queues what follows it on the same
        // connection.
        for chunk in preroll.chunks(512) {
            self.api.voice_send_audio(chunk.to_vec());
        }
        self.mic.set_streaming(true);
        self.arm_watchdog();
        self.update_ui();
    }

    fn stop_streaming(&mut self) {
        // A wake word heard meanwhile keeps its audio.
        self.mic.set_streaming(self.arbitrating);
        if self.phase == Phase::Listening {
            self.phase = Phase::Processing;
        }
    }

    pub(super) fn on_voice_event(&mut self, ev: VaEvent, data: &HashMap<String, String>) {
        let get = |k: &str| data.get(k).map_or("", String::as_str);
        let code = ev.code();
        if ev != VaEvent::IntentProgress {
            info!("voice event {code}");
        }
        if !self.run.accept(ev) {
            debug!("voice event {code}: from the replaced run, ignored");
            return;
        }
        if self.phase == Phase::Idle {
            // The run of a dialog that already ended; it is being stopped. A TTS_START that got
            // out before the stop still leaves HA waiting for the answer's report.
            if ev == VaEvent::TtsStart {
                self.tts_report_owed = true;
                self.send_tts_report(false);
            }
            debug!("voice event {code}: dialog already ended, ignored");
            return;
        }
        match ev {
            VaEvent::RunStart => {
                self.tts_url = get("url").to_owned();
                self.tts_played = false;
                self.continue_conversation = false;
            }
            VaEvent::SttVadEnd | VaEvent::SttEnd => {
                self.stop_streaming();
                self.update_ui();
            }
            VaEvent::IntentStart => {
                if self.phase == Phase::Listening {
                    self.stop_streaming();
                }
                self.update_ui();
            }
            VaEvent::IntentProgress => {
                if get("tts_start_streaming") == "1" {
                    self.play_tts();
                }
            }
            VaEvent::IntentEnd => {
                self.continue_conversation = get("continue_conversation") == "1";
            }
            VaEvent::TtsStart => self.tts_report_owed = true,
            VaEvent::TtsEnd => {
                if !get("url").is_empty() {
                    self.tts_url = get("url").to_owned();
                }
                self.play_tts();
            }
            VaEvent::Error => {
                // HA accepts one wake word phrase per 2 s across satellites: another one answers.
                if get("code") == "duplicate_wake_up_detected" {
                    info!(
                        "pipeline: another satellite answers this wake word (HA duplicate wake-up)"
                    );
                    self.stop_streaming();
                } else {
                    warn!("pipeline error: {} {}", get("code"), get("message"));
                    self.stop_streaming();
                    self.error_shown = true;
                    self.show_ui(UiState::Error);
                    if get("code") != "stt-no-text-recognized" {
                        let sound = format!("{SOUNDS}{}", self.model.error_sound);
                        self.play_closing_earcon(sound);
                    }
                }
            }
            VaEvent::RunEnd => {
                self.stop_streaming();
                if !self.tts_played {
                    self.finish_interaction();
                }
            }
            // Any other type, including one newer than this build, only re-arms the watchdog.
            _ => {}
        }
        self.arm_watchdog();
    }

    fn play_tts(&mut self) {
        if self.tts_played || self.tts_url.is_empty() {
            return;
        }
        self.tts_played = true;
        self.phase = Phase::Speaking;
        self.update_ui();
        let url = self.tts_url.clone();
        let dialog = self.dialog;
        self.play_announce(vec![url], Pending::Tts { dialog });
    }

    /// A cut-short or failed answer does not reopen the microphone, nor does a ringing timer.
    fn tts_finished(&mut self, ok: bool) {
        self.send_tts_report(ok);
        if ok && self.continue_conversation && !self.timer_ringing && !self.mic_muted() {
            self.continue_conversation = false;
            self.start_pipeline("", false, Vec::new());
            return;
        }
        self.finish_interaction();
    }

    /// Sends the AnnounceFinished owed since the run's TTS_START, at most once. Dropped while an
    /// assist announcement's completion is pending (also when cut short): its own
    /// AnnounceFinished ends HA's wait as well, and one more would end the announcement's wait
    /// early. Media player announcements send none.
    fn send_tts_report(&mut self, ok: bool) {
        if !std::mem::take(&mut self.tts_report_owed)
            || self
                .pending
                .values()
                .any(|p| matches!(p, Pending::Announce { .. }))
        {
            return;
        }
        info!("pipeline: answer finished reported to HA (ok={ok})");
        self.api.voice_answer_finished(ok);
    }

    pub(super) fn finish_interaction(&mut self) {
        // The dialog may end before its run does (answer cut short, cancel, timeout): stop the
        // run so HA does not go on generating; whatever it still sends is dropped while idle.
        if (self.run.run_active || self.run.awaiting_run_start) && !self.stop_sent {
            self.api.voice_request_stop();
            self.stop_sent = true;
        }
        self.run.awaiting_run_start = false;
        self.send_tts_report(false);
        self.mic.set_streaming(self.arbitrating);
        self.phase = Phase::Idle;
        self.dialog += 1;
        self.tts_played = false;
        self.tts_url.clear();
        self.continue_conversation = false;
        self.update_duck();
        self.arm_watchdog();
        if self.error_shown {
            self.error_shown = false;
            self.timers.set(Tk::UiAfterError, 1200);
        } else {
            self.update_ui();
        }
    }

    /// The dialog failed without a pipeline run to end it: error ring, then idle.
    pub(super) fn abort_dialog(&mut self, why: &str) {
        warn!("voice pipeline: {why}");
        self.error_shown = true;
        self.show_ui(UiState::Error);
        self.finish_interaction();
    }

    pub(super) fn on_voice_start_failed(&mut self, run: u64) {
        if run != self.voice_run || !matches!(self.phase, Phase::Listening | Phase::Processing) {
            return;
        }
        self.run.awaiting_run_start = false; // no run follows
        self.abort_dialog("Home Assistant refused to start it");
    }

    pub(super) fn on_voice_audio_overflow(&mut self, run: u64) {
        if run != self.voice_run || !matches!(self.phase, Phase::Listening | Phase::Processing) {
            return;
        }
        self.stop_sent = true; // by the server itself
        self.abort_dialog("network too slow for the microphone audio");
    }

    /// Without it a run HA never ends (lost connection state, a hung agent) would leave the ring
    /// listening, the microphone streaming and the wake word ignored.
    fn arm_watchdog(&mut self) {
        self.timers.cancel(Tk::Watchdog);
        let ms = match self.phase {
            Phase::Listening => LISTEN_TIMEOUT_MS,
            Phase::Processing => PROCESS_TIMEOUT_MS,
            _ => return,
        };
        self.timers.set(Tk::Watchdog, ms);
    }

    /// Cuts the TTS answer or HA announcement short. An interrupted announcement ends here (its
    /// completion then only reports to HA); the dialog of an interrupted answer is ended or
    /// replaced by the caller, otherwise by the TTS completion.
    pub(super) fn interrupt_speech(&mut self) {
        self.player.stop(Channel::Announce);
        if self.announcing {
            self.announcing = false;
            self.announce_seq += 1;
        }
    }

    /// Error and cancel sounds outlive the dialog they close; music stays ducked until they end.
    pub(super) fn play_closing_earcon(&mut self, path: String) {
        self.closing_earcon = true;
        self.earcon_seq += 1;
        let seq = self.earcon_seq;
        self.play_announce(vec![path], Pending::Earcon { seq });
    }

    /// Cancels a LISTENING / PROCESSING dialog: the action button's short press and "стоп" past
    /// the window after the wake word. An announcement playing over the dialog goes with it.
    fn cancel_dialog(&mut self) {
        self.interrupt_speech();
        self.play_closing_earcon(format!("{SOUNDS}{}", self.model.cancel_sound));
        self.finish_interaction();
    }

    /// The Button's short press.
    pub(super) fn action_button(&mut self) {
        if self.timer_ringing {
            return self.stop_timer_ring();
        }
        if matches!(self.phase, Phase::Listening | Phase::Processing) {
            return self.cancel_dialog();
        }
        if self.phase == Phase::Speaking || self.announcing {
            self.interrupt_speech();
            return self.finish_interaction();
        }
        self.start_pipeline("", true, Vec::new());
    }

    // ---------------- announcements ----------------

    /// Announcements do not take over the dialog phase, and their end leaves any dialog alone. HA
    /// cancels its own running pipeline before it announces (assist_satellite
    /// async_internal_announce), so a dialog still active here is either ending (its RUN_END or
    /// the watchdog follows) or was started by HA after the announcement.
    pub(super) fn on_announce(&mut self, req: AnnounceRequest) {
        info!(
            "announce: '{}'{}",
            req.text,
            if self.phase == Phase::Idle {
                ""
            } else {
                " (during a dialog)"
            }
        );
        let mut urls = Vec::with_capacity(2);
        if !req.preannounce_media_id.is_empty() {
            urls.push(req.preannounce_media_id);
        }
        urls.push(req.media_id);
        self.announcing = true;
        self.announce_seq += 1;
        let seq = self.announce_seq;
        self.update_duck();
        self.update_ui();
        self.publish_media();
        self.play_announce(
            urls,
            Pending::Announce {
                conv: req.start_conversation,
                seq,
            },
        );
    }

    pub(super) fn end_announce(&mut self) {
        self.update_duck();
        self.update_ui();
        self.publish_media();
    }

    // ---------------- timers ----------------

    pub(super) fn on_timer_event(&mut self, t: TimerInfo) {
        info!(
            "timer {} '{}' ({}) left={} active={}",
            t.event as u32,
            t.name,
            t.id,
            t.seconds_left,
            u8::from(t.is_active)
        );
        let data = HaTimer {
            active: t.is_active,
            end_ms: now_ms() + i64::from(t.seconds_left) * 1000,
            seconds_left: t.seconds_left,
        };
        match t.event {
            TimerEvent::Started => {
                if !self.ha_timers.contains_key(&t.id) {
                    self.leds.show_timer_started();
                }
                self.ha_timers.insert(t.id, data);
            }
            TimerEvent::Updated => {
                self.ha_timers.insert(t.id, data);
            }
            TimerEvent::Cancelled => {
                self.ha_timers.remove(&t.id);
            }
            TimerEvent::Finished => {
                self.ha_timers.remove(&t.id);
                if !self.timer_ringing {
                    self.timer_ringing = true;
                    self.timer_ring_started_ms = now_ms();
                    self.update_duck();
                    self.update_ui();
                    self.ring_timer();
                }
            }
        }
        self.show_timers();
    }

    /// The clock counts a running timer down by itself, so it is set only when the choice or the
    /// timer's data changes, and looked at again when the shown one runs out.
    pub(super) fn show_timers(&mut self) {
        self.timers.cancel(Tk::TimerTick);
        let Some((id, best, left)) = logic::pick_timer(&self.ha_timers, now_ms()) else {
            if !self.shown_timer.is_empty() {
                self.clock.set_timer(0, true);
            }
            self.shown_timer.clear();
            return;
        };
        let same = id == self.shown_timer
            && best.active == self.shown.active
            && if best.active {
                best.end_ms == self.shown.end_ms
            } else {
                best.seconds_left == self.shown.seconds_left
            };
        if !same {
            self.clock.set_timer(left, best.active);
            self.shown_timer = id.to_owned();
            self.shown = best;
        }
        if best.active {
            self.timers.set(Tk::TimerTick, left as u64);
        }
    }

    /// The ring gives the ANNOUNCE channel to speech (TTS answer, HA announcement) and resumes
    /// after it.
    pub(super) fn ring_timer(&mut self) {
        self.timers.cancel(Tk::RingTick);
        if !self.timer_ringing {
            return;
        }
        if now_ms() - self.timer_ring_started_ms > TIMER_RING_MAX_MS {
            return self.stop_timer_ring();
        }
        if self.announcing || self.phase == Phase::Speaking {
            self.ring_yielded = true;
            return;
        }
        let sound = format!("{SOUNDS}{}", self.model.timer_sound);
        self.play_announce(vec![sound], Pending::Ring);
    }

    fn resume_ring(&mut self) {
        if !self.ring_yielded {
            return;
        }
        self.ring_yielded = false;
        self.ring_timer();
    }

    pub(super) fn stop_timer_ring(&mut self) {
        self.timer_ringing = false;
        self.ring_yielded = false;
        self.timers.cancel(Tk::RingTick);
        self.interrupt_speech(); // the ring, or the speech it yielded to
        self.update_duck();
        self.update_ui();
    }

    // ---------------- local commands ----------------

    /// Stock command spotter ("стоп", "хватит", "пауза", "продолжи", "громче", "тише", "дальше",
    /// "назад"): on unless the microphone is muted, the dialog included; which words act where is
    /// logic::command_allowed.
    pub(super) fn update_commands(&self) {
        self.mic.set_commands(!self.mic_muted());
    }

    fn owns_music(&self) -> bool {
        logic::owns_music(
            now_ms(),
            self.player.state(Channel::Music),
            self.music_paused_ms,
            self.sendspin_streaming(),
            self.sendspin.as_ref().is_some_and(|ss| ss.connected()),
            self.sendspin_ended_ms,
        )
    }

    /// True when something was playing. Reads the player and Sendspin states before the
    /// commands (neither changes them synchronously, as in C++).
    fn music_pause(&self) -> bool {
        let mut paused = false;
        if matches!(
            self.player.state(Channel::Music),
            PlayState::Playing | PlayState::Loading
        ) {
            self.player.pause(Channel::Music);
            paused = true;
        }
        if let Some(ss) = &self.sendspin
            && ss.streaming()
        {
            ss.transport(Transport::Pause);
            paused = true;
        }
        paused
    }

    fn music_resume(&self) {
        if self.player.state(Channel::Music) == PlayState::Paused {
            self.player.resume(Channel::Music);
        } else if let Some(ss) = &self.sendspin {
            ss.transport(Transport::Play);
        }
    }

    /// `held`: the word came during a wake arbitration this station won, right after its wake
    /// word.
    pub(super) fn on_command(&mut self, word: &str, held: bool) {
        if self.mic_muted() {
            return;
        }
        let Some(c) = logic::command_of(word) else {
            return;
        };
        if now_ms() < self.commands_blocked_until_ms {
            info!("voice command '{word}' ignored: another station answers this wake word");
            return;
        }
        if self.arbitrating {
            self.arb_command = word.to_owned();
            info!("voice command '{word}' held until the wake arbitration ends");
            return;
        }
        let after_wake = logic::after_wake(self.phase, now_ms(), self.listen_started_ms);
        // A dialog past the window after its wake word: its words are the request's, except
        // "стоп".
        let in_dialog = matches!(self.phase, Phase::Listening | Phase::Processing) && !after_wake;
        let sounding = self.phase == Phase::Speaking || self.announcing || self.timer_ringing;
        if !logic::command_allowed(
            c,
            held || after_wake,
            in_dialog,
            sounding,
            self.owns_music(),
        ) {
            if in_dialog {
                info!("voice command '{word}' ignored: part of the request");
            } else {
                info!("voice command '{word}' ignored: not for this station");
            }
            return;
        }
        info!(
            "voice command '{word}'{}",
            if after_wake {
                " (after wake word, HA pipeline cancelled)"
            } else if held {
                " (after wake word)"
            } else if in_dialog {
                " (in a dialog)"
            } else {
                ""
            }
        );
        // A word in the dialog's own window ends the dialog first; one held through an
        // arbitration in SPEAKING / IDLE leaves the answer playing.
        if after_wake {
            self.finish_interaction();
        }
        match c {
            VoiceCommand::Stop => {
                if self.timer_ringing {
                    self.stop_timer_ring();
                } else if in_dialog {
                    self.cancel_dialog();
                } else if self.phase == Phase::Speaking {
                    self.interrupt_speech();
                    self.finish_interaction();
                } else if self.announcing {
                    // A dialog listening meanwhile goes on.
                    self.interrupt_speech();
                    self.update_duck();
                    self.update_ui();
                } else if self.music_pause() {
                    self.clock.play(ClockShot::Pause);
                }
            }
            VoiceCommand::Pause => {
                if self.music_pause() {
                    self.clock.play(ClockShot::Pause);
                }
            }
            VoiceCommand::Resume => self.music_resume(),
            VoiceCommand::Louder | VoiceCommand::Quieter => {
                let up = c == VoiceCommand::Louder;
                self.set_volume(pulsar_media::mixer::step_alice(self.st.volume, up), true);
            }
            VoiceCommand::Next | VoiceCommand::Prev => {
                let (t, shot) = if c == VoiceCommand::Next {
                    (Transport::Next, ClockShot::Next)
                } else {
                    (Transport::Previous, ClockShot::Previous)
                };
                if self.sendspin.as_ref().is_some_and(|ss| ss.transport(t)) {
                    self.clock.play(shot);
                } else {
                    info!("'{word}' ignored: no Sendspin queue to skip in");
                }
            }
        }
    }
}
