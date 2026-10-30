//! Public types of the API server: the device, its configuration, the entity ids and the
//! values exchanged with Home Assistant. Numeric values are the protocol's (api.proto /
//! aioesphomeapi), as in the C++ `api_server.h`.

use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// Hostname-like, e.g. "yandex-midi-nq3g" (at most 31 characters).
    pub name: String,
    /// E.g. "Яндекс Станция Миди".
    pub friendly_name: String,
    /// wlan0, "AA:BB:CC:DD:EE:FF".
    pub mac: String,
    /// "Yandex Station Midi".
    pub model: String,
    /// Devicetree codename, e.g. "cucumber".
    pub board: String,
    /// "Yandex".
    pub manufacturer: String,
    pub project_version: String,
    /// DeviceInfo compilation_time, in the format of C++ `__DATE__ ", " __TIME__`.
    pub compilation_time: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WakeWordInfo {
    /// E.g. "alisa".
    pub id: String,
    /// E.g. "Алиса".
    pub phrase: String,
    /// E.g. ["ru"].
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiConfig {
    /// 6053.
    pub port: u16,
    /// Wake words available on the device.
    pub wake_words: Vec<WakeWordInfo>,
    /// Initially active ids.
    pub active_wake_words: Vec<String>,
    /// Ring light effects, besides "None".
    pub light_effects: Vec<String>,
    /// Select «Clock face» options, "None" first.
    pub clock_faces: Vec<String>,
    /// Select «Music visualization» options.
    pub music_styles: Vec<String>,
    /// Select «Case color» options, "None" first.
    pub case_colors: Vec<String>,
    /// Select «Clock music» options.
    pub clock_music_styles: Vec<String>,
    /// Select «Zigbee role» options; empty: no Zigbee entities (no radio).
    pub zigbee_modes: Vec<String>,
    /// false: no Clock display / Clock brightness / Clock auto brightness / Clock text speed /
    /// Clock text / Clock face / Clock music entities, no clock_show_text / clock_show_face
    /// actions, none of their states sent.
    pub has_clock: bool,
    /// false: no Illuminance sensor or lux states.
    pub has_lux: bool,
    /// false: no Clock auto brightness switch or its states (listed only with `has_clock` as
    /// well).
    pub has_auto_brightness: bool,
    /// The model has wake-word pre-roll rules: the switch "Command right after wake word".
    pub has_wake_preroll: bool,
    /// The model has a readable hardware microphone mute switch/key.
    pub has_hw_mic_mute: bool,
    /// The model has the action button and volume keys (the Event «Button» entity).
    pub has_buttons: bool,
    /// File holding the Noise encryption key (base64 of 32 bytes, as ESPHome's `api: encryption:
    /// key`). Missing or empty file: plaintext (and provisioning with the all-zeros key). HA may
    /// set or rotate it with NoiseEncryptionSetKeyRequest. None: encryption unsupported.
    pub key_path: Option<PathBuf>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            port: 6053,
            wake_words: Vec::new(),
            active_wake_words: Vec::new(),
            light_effects: Vec::new(),
            clock_faces: Vec::new(),
            music_styles: Vec::new(),
            case_colors: Vec::new(),
            clock_music_styles: Vec::new(),
            zigbee_modes: Vec::new(),
            has_clock: true,
            has_lux: true,
            has_wake_preroll: true,
            has_hw_mic_mute: true,
            has_buttons: true,
            has_auto_brightness: true,
            key_path: None,
        }
    }
}

/// aioesphomeapi VoiceAssistantEventType; `code()` is the value on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VaEvent {
    Error,
    RunStart,
    RunEnd,
    SttStart,
    SttEnd,
    IntentStart,
    IntentEnd,
    TtsStart,
    TtsEnd,
    WakeWordStart,
    WakeWordEnd,
    SttVadStart,
    SttVadEnd,
    TtsStreamStart,
    TtsStreamEnd,
    IntentProgress,
    /// A type newer than this build. Still a pipeline event: the C++ controller re-armed its
    /// LISTENING / PROCESSING watchdog on any event, whatever the type.
    Unknown(u32),
}

impl VaEvent {
    const CODES: [(u32, VaEvent); 16] = [
        (0, VaEvent::Error),
        (1, VaEvent::RunStart),
        (2, VaEvent::RunEnd),
        (3, VaEvent::SttStart),
        (4, VaEvent::SttEnd),
        (5, VaEvent::IntentStart),
        (6, VaEvent::IntentEnd),
        (7, VaEvent::TtsStart),
        (8, VaEvent::TtsEnd),
        (9, VaEvent::WakeWordStart),
        (10, VaEvent::WakeWordEnd),
        (11, VaEvent::SttVadStart),
        (12, VaEvent::SttVadEnd),
        (98, VaEvent::TtsStreamStart),
        (99, VaEvent::TtsStreamEnd),
        (100, VaEvent::IntentProgress),
    ];

    pub fn from_code(code: u32) -> Self {
        Self::CODES
            .iter()
            .find(|(c, _)| *c == code)
            .map_or(VaEvent::Unknown(code), |&(_, e)| e)
    }

    pub fn code(self) -> u32 {
        match self {
            VaEvent::Unknown(c) => c,
            e => Self::CODES
                .iter()
                .find(|(_, x)| *x == e)
                .map_or(u32::MAX, |&(c, _)| c),
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TimerEvent {
    #[default]
    Started = 0,
    Updated = 1,
    Cancelled = 2,
    Finished = 3,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimerInfo {
    pub event: TimerEvent,
    pub id: String,
    pub name: String,
    pub total_seconds: u32,
    pub seconds_left: u32,
    pub is_active: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnnounceRequest {
    pub media_id: String,
    pub text: String,
    pub preannounce_media_id: String,
    pub start_conversation: bool,
}

/// api.proto MediaPlayerState.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaState {
    None = 0,
    Idle = 1,
    Playing = 2,
    Paused = 3,
    Announcing = 4,
    Off = 5,
    On = 6,
}

/// api.proto MediaPlayerCommand.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaCommand {
    Play = 0,
    Pause = 1,
    Stop = 2,
    Mute = 3,
    Unmute = 4,
    Toggle = 5,
    VolumeUp = 6,
    VolumeDown = 7,
    Enqueue = 8,
    RepeatOne = 9,
    RepeatOff = 10,
    ClearPlaylist = 11,
    TurnOn = 12,
    TurnOff = 13,
}

/// MediaPlayerCommandRequest: each field only when HA set it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaCommandRequest {
    pub command: Option<MediaCommand>,
    pub volume: Option<f32>,
    pub media_url: Option<String>,
    pub announcement: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LightState {
    pub on: bool,
    /// 0..1.
    pub brightness: f32,
    /// 0..1 each.
    pub r: f32,
    pub g: f32,
    pub b: f32,
    /// "" or "None" means no effect.
    pub effect: String,
}

impl Default for LightState {
    fn default() -> Self {
        Self {
            on: false,
            brightness: 1.0,
            r: 1.0,
            g: 1.0,
            b: 1.0,
            effect: String::new(),
        }
    }
}

// Auxiliary entities. Their keys are stable and owned by the server.

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SwitchId {
    Microphones = 1,
    Clock = 2,
    AutoUpdate = 3,
    NearestAnswers = 4,
    VoiceIndication = 5,
    WakePreroll = 6,
    ClockAutoBrightness = 7,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SensorId {
    Illuminance = 1,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinarySensorId {
    HwMicMute = 1,
}

/// Clock brightness 0..100 %, clock scroll 100..2000 ms, LED speed 0.25..4 ×.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NumberId {
    ClockBrightness = 1,
    ClockScroll = 2,
    LedSpeed = 3,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextId {
    ClockText = 1,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SelectId {
    ClockFace = 1,
    ZigbeeMode = 2,
    MusicViz = 3,
    CaseColor = 4,
    ClockMusic = 5,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ButtonId {
    ZigbeeJoin = 1,
    ZigbeeLeave = 2,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextSensorId {
    Zigbee = 1,
}

/// User-defined actions; arguments (text | face, seconds).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionId {
    ClockShowText = 1,
    ClockShowFace = 2,
}

/// Event types: "action", "action_long", "volume_up", "volume_down".
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventId {
    Button = 1,
}

/// "Firmware" update entity command (api.proto UpdateCommand).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UpdateCommand {
    None = 0,
    Install = 1,
    Check = 2,
}

/// UpdateStateResponse of the "Firmware" entity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpdateState {
    pub in_progress: bool,
    pub has_progress: bool,
    /// 0..100.
    pub progress: f32,
    pub current_version: String,
    /// Equals `current_version` when there is nothing newer (or nothing checked yet).
    pub latest_version: String,
    pub title: String,
    pub release_url: String,
}

/// What Home Assistant (or another API client) asked for or reported; the C++ `ApiHooks`.
#[derive(Clone, Debug, PartialEq)]
pub enum ApiEvent {
    /// A client completed hello (sent for each client).
    ClientConnected {
        client_info: String,
    },
    /// The last client past hello disconnected.
    ClientDisconnected,
    /// The voice pipeline owner appeared / went away. The owner is the most recent client still
    /// subscribed to the voice assistant; when ownership moves between clients (a newer
    /// subscriber, or the owner leaving while an older subscriber remains), false precedes true.
    VoiceSubscribed(bool),
    VoiceEvent {
        event: VaEvent,
        data: HashMap<String, String>,
    },
    Announce(AnnounceRequest),
    Timer(TimerInfo),
    /// HA changed the active wake words (ids known, non-empty). The reported active set changes
    /// only through `ApiHandle::set_active_wake_words`.
    ActiveWakeWords(Vec<String>),
    MediaCommand(MediaCommandRequest),
    Switch(SwitchId, bool),
    Button(ButtonId),
    /// `transition_ms`: the command's transition length, ESPHome's default (1 s) without one.
    Light {
        state: LightState,
        transition_ms: u32,
    },
    Number(NumberId, f32),
    /// At most 64 UTF-8 characters.
    Text(TextId, String),
    /// One of the select's options.
    Select(SelectId, String),
    /// User-defined action: `value` is the text (at most 64 characters) or the face ("" or one
    /// of `ApiConfig::clock_faces`).
    Action {
        id: ActionId,
        value: String,
        seconds: i32,
    },
    /// Action led_show: effect "" (solid colour) or one of `ApiConfig::light_effects`.
    LedShow {
        effect: String,
        color: String,
        seconds: i32,
    },
    /// Time from HA (GetTimeResponse); `posix_tz` is a POSIX TZ string (e.g. "MSK-3"), may be
    /// empty.
    Time {
        epoch: u32,
        posix_tz: String,
    },
    /// "Restart" button: the daemon should exit 0 (supervise.sh restarts it).
    Restart,
    /// HA answered the voice run's VoiceAssistantRequest(start) with an error: no pipeline
    /// runs.
    VoiceStartFailed {
        run: u64,
    },
    /// The link to HA fell too far behind to carry mic audio; the server already sent
    /// VoiceAssistantRequest(start=false), the controller should end the run locally.
    VoiceAudioOverflow {
        run: u64,
    },
    /// Update entity: install the offered release or check for one now.
    Update(UpdateCommand),
    /// The encryption key changed: the TXT records of `_esphomelib._tcp` to advertise now.
    MdnsTxtChanged(Vec<String>),
}
