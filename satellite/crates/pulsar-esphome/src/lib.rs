//! ESPHome native API server: exposes the speaker to Home Assistant as an assist satellite,
//! media player and auxiliary entities (port 6053; plaintext until a Noise key is provisioned,
//! then Noise). Several clients may connect (HA, log viewers, probes): states go to every
//! client subscribed to them, the voice pipeline to one owner.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod frame;
mod key;
mod proto;
mod server;
#[allow(unsafe_code)]
mod sys;
mod types;

use std::rc::Rc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use server::{Cmd, Publish, Shared};
pub use types::*;

/// Binds the API port (all IPv4 interfaces), reads the encryption key and starts the server
/// actor on the current `LocalSet`. Err (already logged) when the port cannot be bound.
pub fn spawn(
    id: DeviceIdentity,
    cfg: ApiConfig,
) -> Result<(ApiHandle, UnboundedReceiver<ApiEvent>), String> {
    let s = server::spawn(id, cfg)?;
    Ok((
        ApiHandle {
            cmds: s.cmds,
            shared: s.shared,
        },
        s.events,
    ))
}

/// Commands to the server actor; the server stops (DisconnectRequest to every client, port
/// closed) when the last clone is dropped.
#[derive(Clone)]
pub struct ApiHandle {
    cmds: UnboundedSender<Cmd>,
    shared: Rc<Shared>,
}

impl ApiHandle {
    /// Some client completed hello.
    ///
    /// Unlike the C++ server, a write error or a client that stopped reading never closes a
    /// client inside a call of this handle: the actor does the I/O, so right after
    /// `voice_request_start` / `voice_request_stop` / `voice_send_audio` /
    /// `voice_announce_finished` / `publish_*` / `fire_event` the getters still return the
    /// previous state, and the outcome
    /// arrives as events (`VoiceStartFailed`, `VoiceAudioOverflow`, `VoiceSubscribed(false)`
    /// possibly followed by `VoiceSubscribed(true)` when another client takes the pipeline over,
    /// `ClientDisconnected`). The actor updates the getters before it sends those events.
    pub fn connected(&self) -> bool {
        self.shared.connected.get()
    }

    /// A client owns the voice pipeline (see `connected` for when it changes).
    pub fn voice_subscribed(&self) -> bool {
        self.shared.voice_subscribed.get()
    }

    /// TXT records of `_esphomelib._tcp` for the current key state: computed by `spawn`, then
    /// by the actor 100 ms after a client changed the key, before `MdnsTxtChanged`.
    pub fn mdns_txt(&self) -> Vec<String> {
        self.shared.mdns_txt.borrow().clone()
    }

    /// Starts a pipeline run (VoiceAssistantRequest start=true) on the voice owner; an empty
    /// phrase means started by the button or to continue a conversation. Returns the run id
    /// (1, 2, …) that `VoiceStartFailed` and `VoiceAudioOverflow` carry when they happen
    /// during this run.
    pub fn voice_request_start(&self, wake_word_phrase: &str) -> u64 {
        let run = self.shared.run_seq.get() + 1;
        self.shared.run_seq.set(run);
        self.send(Cmd::VoiceRequestStart {
            run,
            phrase: wake_word_phrase.to_owned(),
        });
        run
    }

    pub fn voice_request_stop(&self) {
        self.send(Cmd::VoiceRequestStop);
    }

    /// 16 kHz mono samples while the pipeline listens. When the link to the owner falls 256 KiB
    /// behind, the server ends the run (start=false), sends `VoiceAudioOverflow` and drops audio
    /// until the next `voice_request_start`.
    pub fn voice_send_audio(&self, pcm: Vec<i16>) {
        self.send(Cmd::VoiceSendAudio(pcm));
    }

    /// VoiceAssistantAnnounceFinished for an HA announcement.
    pub fn voice_announce_finished(&self, success: bool) {
        self.send(Cmd::VoiceAnnounceFinished(success));
    }

    /// The same message for the current run's TTS answer (HA stays "responding" from TTS_START
    /// until it arrives). Like stop and audio, dropped once the voice owner changed until the
    /// next `voice_request_start`: it belongs to the previous owner's run.
    pub fn voice_answer_finished(&self, success: bool) {
        self.send(Cmd::VoiceAnswerFinished(success));
    }

    /// The wake word ids reported as active to HA from now on.
    pub fn set_active_wake_words(&self, ids: Vec<String>) {
        self.send(Cmd::SetActiveWakeWords(ids));
    }

    // State publishing: the server keeps the last value of each entity (replayed to a client
    // when it subscribes to states) and sends it to every state subscriber.

    /// Volume is clamped to 0..1.
    pub fn publish_media(&self, st: MediaState, volume: f32, muted: bool) {
        self.publish(Publish::Media(st, volume, muted));
    }

    pub fn publish_switch(&self, id: SwitchId, on: bool) {
        self.publish(Publish::Switch(id, on));
    }

    pub fn publish_binary_sensor(&self, id: BinarySensorId, on: bool) {
        self.publish(Publish::BinarySensor(id, on));
    }

    pub fn publish_sensor(&self, id: SensorId, v: f32) {
        self.publish(Publish::Sensor(id, v));
    }

    pub fn publish_number(&self, id: NumberId, v: f32) {
        self.publish(Publish::Number(id, v));
    }

    pub fn publish_text(&self, id: TextId, s: &str) {
        self.publish(Publish::Text(id, s.to_owned()));
    }

    pub fn publish_select(&self, id: SelectId, s: &str) {
        self.publish(Publish::Select(id, s.to_owned()));
    }

    /// Sent only when the Zigbee entities exist (`ApiConfig::zigbee_modes` not empty).
    pub fn publish_text_sensor(&self, id: TextSensorId, s: &str) {
        self.publish(Publish::TextSensor(id, s.to_owned()));
    }

    pub fn publish_light(&self, st: &LightState) {
        self.publish(Publish::Light(st.clone()));
    }

    /// An event of the Button entity ("action", "action_long", "volume_up", "volume_down");
    /// dropped without a state subscriber, like the C++ server.
    pub fn fire_event(&self, id: EventId, event_type: &str) {
        self.publish(Publish::Event(id, event_type.to_owned()));
    }

    pub fn publish_update(&self, st: &UpdateState) {
        self.publish(Publish::Update(Box::new(st.clone())));
    }

    fn publish(&self, p: Publish) {
        self.send(Cmd::Publish(p));
    }

    fn send(&self, cmd: Cmd) {
        // Fails only once the actor is gone (runtime shutting down).
        let _ = self.cmds.send(cmd);
    }
}
