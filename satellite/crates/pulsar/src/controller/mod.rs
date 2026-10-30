//! Glue between Home Assistant (ESPHome API), the microphone front-end, playback and the hardware
//! UI (C++ controller.cpp). Voice-satellite behaviour follows OHF linux-voice-assistant
//! (satellite.py). The controller owns its state and every service handle; the services report
//! through channels, which `run` serves one event at a time (no re-entrant hooks as in C++).

mod entities;
mod logic;
mod media;
mod voice;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use log::{debug, error, info, warn};
use pulsar_audio::FrontendEvent;
use pulsar_core::model::Model;
use pulsar_core::{ClockShot, Timers, UiState, now_ms};
use pulsar_esphome::{ApiConfig, ApiEvent, ApiHandle, WakeWordInfo};
use pulsar_hw::{InputEvent, InputHandle, LedsHandle, Rgb};
use pulsar_lan::arbiter::{self, ArbiterEvent, ArbiterHandle};
use pulsar_lan::mdns::{self, MdnsHandle, MdnsService};
use pulsar_media::{Channel, Mixer, PlayId, PlayState, PlayerEvent, PlayerHandle};
use pulsar_sendspin::{SendspinConfig, SendspinEvent, SendspinHandle};
use pulsar_update::{Release, UpdateEvent, UpdaterHandle};
use pulsar_zigbee::{ZigbeeConfig, ZigbeeEvent, ZigbeeHandle};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::settings::{Settings, StateFile};
use crate::version::{BUILD_TIME, VERSION};
use logic::{HaTimer, Phase, RunBinding};

const MODELS: &str = "/system/vendor/quasar/spotter_models/activation/";
const COMMAND_MODEL: &str = "/system/vendor/quasar/spotter_models/command";
const SOUNDS: &str = "/system/vendor/quasar/sounds/";

const DUCK_GAIN: f32 = 0.15;
const TIMER_RING_MAX_MS: i64 = 15 * 60 * 1000;
// Longest wait for the next pipeline event before the dialog is given up locally. HA's voice
// command segmenter ends speech capture after at most 15 s of audio. Between INTENT_START and
// INTENT_END nothing arrives (ESPHome forwards INTENT_PROGRESS only once, when TTS streaming
// starts), and conversation agents (LLMs with tool calls) may think for a minute or more.
const LISTEN_TIMEOUT_MS: u64 = 30_000;
const PROCESS_TIMEOUT_MS: u64 = 120_000;
// Updates: releases of this repository on GitHub. The first check waits for the network
// after boot; an automatic install waits until the speaker is idle (it restarts the daemon).
const UPDATE_FIRST_CHECK_MS: u64 = 60_000;
const UPDATE_CHECK_PERIOD_MS: u64 = 6 * 3_600_000;
const UPDATE_IDLE_RETRY_MS: u64 = 10 * 60_000;
// The C++ LightSensor::on_lux default.
const LUX_PERIOD_MS: u64 = 2000;

/// Why `run` returned; the daemon exits 0 for each (supervise.sh starts it again).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// SIGTERM / SIGINT.
    Signal,
    /// Home Assistant's Restart button.
    Restart,
    /// Another Zigbee role was chosen: it starts with the daemon.
    ZigbeeRoleChanged,
    /// A new release replaced the binary.
    Updated,
}

/// Loop timers of the C++ controller. Intended difference: C++ armed an independent timer on
/// every call, so with two error rings pending update_ui ran at the earlier deadline (and again
/// at the later one); here a re-arm of `UiAfterNotSubscribed` / `UiAfterError` moves the
/// deadline. Each firing runs the same idempotent update, so the final state is identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tk {
    /// The error ring of a wake word without a voice subscriber ends.
    UiAfterNotSubscribed,
    /// The error ring of a failed dialog ends.
    UiAfterError,
    /// Ends a LISTENING / PROCESSING phase HA never advances.
    Watchdog,
    /// The shown running HA timer runs out.
    TimerTick,
    /// Next round of the timer ring.
    RingTick,
    UpdateCheck,
    /// An automatic install waits for idle.
    UpdateRetry,
    /// Exit after an installed update.
    UpdateExit,
}

/// What a `play()` was for: the C++ completion callback and the values it captured.
#[derive(Clone, Copy, Debug)]
enum Pending {
    /// Pipeline TTS of dialog `dialog`.
    Tts { dialog: u64 },
    /// Error / cancel earcon number `seq`.
    Earcon { seq: u64 },
    /// HA (assist satellite) announcement number `seq`.
    Announce { conv: bool, seq: u64 },
    /// Media player announcement number `seq`.
    MediaAnnounce { seq: u64 },
    /// A round of the timer ring.
    Ring,
}

/// Event channels of the services; a closed one is dropped.
struct Rx {
    api: Option<UnboundedReceiver<ApiEvent>>,
    frontend: Option<UnboundedReceiver<FrontendEvent>>,
    player: Option<UnboundedReceiver<PlayerEvent>>,
    arbiter: Option<UnboundedReceiver<ArbiterEvent>>,
    input: Option<UnboundedReceiver<InputEvent>>,
    lux: Option<UnboundedReceiver<f32>>,
    sendspin: Option<UnboundedReceiver<SendspinEvent>>,
    zigbee: Option<UnboundedReceiver<ZigbeeEvent>>,
    updater: Option<UnboundedReceiver<UpdateEvent>>,
}

/// Next event of an optional channel; never ready without one.
async fn recv<T>(rx: &mut Option<UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

pub struct Controller {
    // Service handles first: fields drop in declaration order, and the C++ members went in
    // reverse declaration order (Sendspin first, the API server last). Dropping the last handle
    // of a service stops it; the front-end joins its threads (up to ~200 ms of snd_pcm_wait plus
    // the chunk in progress), inside the teardown watchdog that main arms before the drop.
    sendspin: Option<SendspinHandle>,
    zigbee: Option<ZigbeeHandle>,
    updater: UpdaterHandle,
    arbiter: ArbiterHandle,
    /// Held only: the input actor runs while its handle exists.
    _input: Option<InputHandle>,
    clock: pulsar_clock::ClockHandle,
    leds: LedsHandle,
    mixer: Mixer,
    player: PlayerHandle,
    mic: pulsar_audio::FrontendHandle,
    mdns: Option<MdnsHandle>,
    api: ApiHandle,
    rx: Rx,
    term: Signal,
    int: Signal,

    model: &'static Model,
    state: StateFile,
    st: Settings,
    timers: Timers<Tk>,
    /// Completion of each `play()` whose end matters.
    pending: HashMap<PlayId, Pending>,
    exit: Option<ExitReason>,
    /// TZ last given to libc ("" = none yet).
    applied_tz: String,

    phase: Phase,
    /// Start of the current LISTENING phase (commands window).
    listen_started_ms: i64,
    hw_mic_muted: bool,
    error_shown: bool,
    // Wake arbitration: the window between our claim and the decision. Microphone audio and a
    // command word heard in it are held until the decision.
    arbitrating: bool,
    /// Our claim being decided; forgotten on cancel (a `Decided` already queued then still
    /// arrives and is ignored).
    arb_claim: Option<u64>,
    arb_phrase: String,
    arb_level: f32,
    arb_audio: Vec<i16>,
    arb_command: String,
    /// Another station answers the wake word just heard.
    commands_blocked_until_ms: i64,
    run: RunBinding,
    /// voice_request_stop already sent for this dialog's run.
    stop_sent: bool,
    /// Id of the last voice_request_start: VoiceStartFailed / VoiceAudioOverflow of another
    /// run are ignored.
    voice_run: u64,
    tts_url: String,
    tts_played: bool,
    /// A run's TTS_START left HA's assist satellite "responding" until AnnounceFinished, also
    /// when nothing plays here, and that report is still to be sent (`send_tts_report`).
    tts_report_owed: bool,
    continue_conversation: bool,
    /// An HA announcement (assist or media player) plays on ANNOUNCE.
    announcing: bool,
    ducked: bool,
    closing_earcon: bool,
    earcon_seq: u64,
    // Completions act only for what is still current: `dialog` changes whenever a dialog starts
    // or ends, `announce_seq` whenever an HA announcement starts or is interrupted.
    dialog: u64,
    announce_seq: u64,
    timer_ringing: bool,
    /// The ring gave the ANNOUNCE channel to speech; resume_ring() continues it.
    ring_yielded: bool,
    timer_ring_started_ms: i64,
    /// When the HA media channel last paused (logic::owns_music).
    music_paused_ms: Option<i64>,
    /// When the Sendspin stream last ended: pause or stop in Music Assistant (logic::owns_music).
    sendspin_ended_ms: Option<i64>,
    /// HA timers by TimerInfo::id.
    ha_timers: BTreeMap<String, HaTimer>,
    /// Id on the clock, "" = none.
    shown_timer: String,
    /// Its data when it was put there.
    shown: HaTimer,
    last_lux_published: f32,
    last_lux_ms: i64,
    // Clock text / face entity states; not persisted (the clock shows the time after a
    // restart).
    clock_text: String,
    clock_face: String,
    /// Latest release found.
    release: Option<Release>,
    update_installing: bool,
    update_progress: f32,
    /// A check whose `Checked` has not been handled yet (C++ `Updater::busy()` until its
    /// callback ran).
    pending_check: Option<u64>,
    /// An install whose `Installed` has not been handled yet.
    install_id: Option<u64>,
    /// Version being installed (for the messages of its outcome).
    install_version: String,
    /// Last Zigbee status line published.
    zigbee_status: String,
    sendspin_pairing_prompt: bool,
}

fn read_line(path: &str) -> String {
    std::fs::read(path)
        .map(|b| {
            let line = b.split(|&c| c == b'\n').next().unwrap_or_default();
            String::from_utf8_lossy(line).into_owned()
        })
        .unwrap_or_default()
}

/// Index in `pulsar_clock::face_names()`; -1 for "None" / "".
fn clock_face_index(name: &str) -> i32 {
    pulsar_clock::face_names()
        .iter()
        .position(|&n| n == name)
        .map_or(-1, |i| i as i32)
}

fn model_dir(id: &str) -> PathBuf {
    PathBuf::from(format!("{MODELS}{id}"))
}

/// C++ `uint8_t(x * 255)`.
fn light_rgb(l: &pulsar_esphome::LightState) -> Rgb {
    Rgb {
        r: (l.r * 255.0) as u8,
        g: (l.g * 255.0) as u8,
        b: (l.b * 255.0) as u8,
    }
}

impl Controller {
    /// Loads the settings and starts every service. Err (to be logged by the caller, exit 1):
    /// an invalid `PULSAR_PORT_OFFSET`, or the API server could not bind its port.
    pub fn start(model: &'static Model, state_path: PathBuf) -> Result<Controller, String> {
        // Before anything starts: a SIGTERM during the start-up still ends the daemon cleanly.
        let term = signal(SignalKind::terminate()).map_err(|e| format!("signals: {e}"))?;
        let int = signal(SignalKind::interrupt()).map_err(|e| format!("signals: {e}"))?;
        let ports = logic::ports(std::env::var_os("PULSAR_PORT_OFFSET").as_deref())
            .ok_or_else(|| "invalid PULSAR_PORT_OFFSET".to_owned())?;
        let mut st = Settings::default();
        st.load(&state_path);
        let mut applied_tz = String::new();
        if !st.tz.is_empty() {
            // The clock shows local time before HA connects.
            pulsar_core::tz::set_tz(&st.tz);
            applied_tz.clone_from(&st.tz);
        }
        // After the TZ: the log's timestamps are local time.
        if ports.offset != 0 {
            info!(
                "PULSAR_PORT_OFFSET={}: API {}, Sendspin {}, deCONZ {}, arbiter {}, no mDNS",
                ports.offset, ports.api, ports.sendspin, ports.deconz, ports.arbiter
            );
        }

        let id = logic::identity(
            model,
            &read_line("/sys/class/net/wlan0/address"),
            VERSION,
            BUILD_TIME,
        );
        if st.wake_word != "alisa" && st.wake_word != "yandex" {
            st.wake_word = "alisa".into();
        }
        let case_colors = pulsar_hw::case_colors();
        let dir = logic::state_dir(state_path.as_os_str());
        let strings = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let with_none = |l: Vec<String>| std::iter::once("None".to_owned()).chain(l).collect();
        // Opened before the entity list: the Clock auto brightness switch exists only when the
        // sensor opened, and where the model makes the sensor optional, so does the Illuminance
        // entity.
        let lux_rx = match pulsar_clock::spawn_light_sensor(LUX_PERIOD_MS) {
            Ok(rx) => Some(rx),
            Err(e) if model.has_lux => {
                error!("{e}");
                None
            }
            Err(e) => {
                info!("{e}");
                None
            }
        };
        let cfg = ApiConfig {
            port: ports.api,
            wake_words: vec![
                WakeWordInfo {
                    id: "alisa".into(),
                    phrase: "Алиса".into(),
                    languages: vec!["ru".into()],
                },
                WakeWordInfo {
                    id: "yandex".into(),
                    phrase: "Яндекс".into(),
                    languages: vec!["ru".into()],
                },
            ],
            active_wake_words: vec![st.wake_word.clone()],
            light_effects: strings(pulsar_hw::effect_names()),
            clock_faces: with_none(strings(pulsar_clock::face_names())),
            music_styles: strings(pulsar_hw::music_styles()),
            case_colors: with_none(case_colors.clone()),
            clock_music_styles: strings(pulsar_clock::music_styles()),
            zigbee_modes: if model.zigbee_tty.is_empty() {
                Vec::new()
            } else {
                pulsar_zigbee::ROLES
                    .iter()
                    .map(|r| r.label.to_owned())
                    .collect()
            },
            has_clock: model.has_clock || pulsar_clock::panel_present(model),
            has_lux: model.has_lux || lux_rx.is_some(),
            has_wake_preroll: !model.wake_preroll.is_empty(),
            has_hw_mic_mute: !model.mute_input.is_empty() || model.mute_gpio != 0,
            has_buttons: model.has_button_events(),
            key_path: Some(logic::in_dir(&dir, "api_key")),
            has_auto_brightness: lux_rx.is_some(),
        };
        // The C++ server bound its port last (ApiServer::start); here the bind comes first, so
        // a failure exits before the hardware starts. Nothing reaches the server before `run`.
        let (api, api_rx) = pulsar_esphome::spawn(id.clone(), cfg)
            .map_err(|_| "ESPHome API server failed to start".to_owned())?;
        // Before any other thread exists: the player's spawn sets SSL_CERT_FILE (set_var), which
        // C code on another thread could be reading.
        let (player, player_rx) = pulsar_media::spawn(model);
        let (arbiter, arbiter_rx) = arbiter::spawn(id.name.clone(), ports.arbiter);

        // Hardware first: UI feedback should work even if HA is not connected.
        let leds = pulsar_hw::spawn_leds(model);
        // spawn handles an absent panel (empty clock_dir) with a no-op handle.
        let clock = pulsar_clock::spawn(model);
        clock.set_enabled(st.clock_on);
        clock.set_brightness(st.clock_brightness);
        clock.set_auto_brightness(st.clock_auto_brightness);
        clock.set_scroll_ms(st.clock_scroll_ms);
        clock.set_voice_indication(st.voice_indication);
        clock.set_music_style(st.clock_music.clone());
        leds.set_speed(st.led_speed);
        leds.set_voice_indication(st.voice_indication);
        leds.set_music_style(st.music_viz.clone());
        leds.set_user_light(
            st.light.on,
            light_rgb(&st.light),
            st.light.brightness,
            st.light.effect.clone(),
            0,
        );

        let mut mixer = Mixer::open(model);
        if !mixer.available() {
            warn!("mixer unavailable");
        }
        let (volume, migrated) = logic::initial_volume(
            st.has_volume,
            st.volume,
            st.volume_legacy,
            st.legacy_on_stock,
            mixer.volume(),
            |v| pulsar_core::volume::from_legacy(model, v),
        );
        st.volume = volume;
        // Changes made while starting are written like any other (StateFile, 1 s later).
        let mut changed = false;
        if migrated {
            info!(
                "volume {:.3} from an earlier build -> {:.3}",
                st.volume_legacy, st.volume
            );
            changed = true;
        }
        mixer.set_volume(st.volume);
        mixer.set_muted(st.muted);
        if !st.led_case_color.is_empty() {
            // After the volume: the write covers it too.
            let listed = case_colors.contains(&st.led_case_color);
            if !listed {
                warn!(
                    "case colour {} not in this firmware, none used",
                    st.led_case_color
                );
            }
            // Leds logs an unreadable file.
            if !listed || !leds.set_case_color(&st.led_case_color) {
                st.led_case_color.clear();
                changed = true;
            }
        }
        if !st.update_attempt.is_empty() {
            if st.update_attempt == VERSION {
                info!("updated to {VERSION}");
                st.update_attempt.clear();
                changed = true;
            } else {
                warn!(
                    "update to {} did not stay installed (rolled back?); automatic updates skip it",
                    st.update_attempt
                );
            }
        }
        let (updater, updater_rx) =
            pulsar_update::spawn(pulsar_update::UPDATE_REPO.into(), VERSION.into());

        let (input, input_rx) = match pulsar_hw::spawn_input(model) {
            Ok((h, rx)) => (Some(h), Some(rx)),
            Err(e) => {
                warn!("{e}");
                (None, None)
            }
        };
        let hw_mic_muted = input.as_ref().is_some_and(InputHandle::hw_muted);
        let (mic, mic_rx) = pulsar_audio::spawn(
            model,
            model_dir(&st.wake_word),
            Some(PathBuf::from(COMMAND_MODEL)),
        );
        mic.set_preroll(st.wake_preroll);

        let ss_cfg = SendspinConfig {
            name: id.friendly_name.clone(),
            product_name: id.model.clone(),
            manufacturer: id.manufacturer.clone(),
            software_version: id.project_version.clone(),
            mac: id.mac.to_ascii_lowercase(),
            state_dir: logic::in_dir(&dir, "sendspin"),
            model,
            port: ports.sendspin,
        };
        let master = pulsar_core::volume::master_gain(model, st.volume);
        let (sendspin, sendspin_rx) =
            match pulsar_sendspin::spawn(ss_cfg, st.volume, st.muted, master) {
                Ok((h, rx)) => (Some(h), Some(rx)),
                Err(e) => {
                    debug!("{e}");
                    error!("Sendspin player unavailable");
                    (None, None)
                }
            };

        // A test instance (port offset) stays invisible to HA and Music Assistant.
        let mdns = if ports.offset == 0 {
            match mdns::spawn(id.name.clone()) {
                Ok(m) => {
                    m.add_service(MdnsService {
                        service: "_esphomelib._tcp".into(),
                        instance: id.name.clone(),
                        port: ports.api,
                        txt: api.mdns_txt(),
                    });
                    if sendspin.is_some() {
                        m.add_service(MdnsService {
                            service: "_sendspin._tcp".into(),
                            instance: id.name.clone(),
                            port: ports.sendspin,
                            txt: vec![
                                "path=/sendspin".into(),
                                format!("name={}", id.friendly_name),
                            ],
                        });
                    }
                    Some(m)
                }
                Err(_) => {
                    warn!("api: mDNS advertising unavailable");
                    None
                }
            }
        } else {
            None
        };

        let (zigbee, zigbee_rx) = if model.zigbee_tty.is_empty() {
            (None, None)
        } else {
            let zcfg = ZigbeeConfig {
                lines: pulsar_zigbee::RadioLines {
                    tty: model.zigbee_tty.into(),
                    reset: model.zigbee_reset.into(),
                    boot: model.zigbee_boot.into(),
                },
                coordinator_image: logic::in_dir(&dir, "zigbee-coordinator.ybl"),
                router_image: logic::in_dir(&dir, "zigbee-router.ybl"),
                deconz_port: ports.deconz,
                failed_version: st.zigbee_flash_failed,
            };
            updater.add_extra_asset("zigbee-coordinator.ybl", zcfg.coordinator_image.clone());
            updater.add_extra_asset("zigbee-router.ybl", zcfg.router_image.clone());
            let mode = st.zigbee_role;
            let (h, rx) = pulsar_zigbee::spawn(zcfg, mode);
            // ZHA discovers the coordinator (radio_type selects zigpy-deconz, serial_number is
            // its unique id).
            if mode == pulsar_zigbee::Mode::Coordinator
                && let Some(m) = &mdns
            {
                m.add_service(MdnsService {
                    service: "_zigbee-coordinator._tcp".into(),
                    instance: id.name.clone(),
                    port: ports.deconz,
                    txt: vec![
                        "radio_type=deconz".into(),
                        format!("serial_number={}", id.name),
                        format!("name={}", id.friendly_name),
                    ],
                });
            }
            (Some(h), Some(rx))
        };

        let mut c = Controller {
            sendspin,
            zigbee,
            updater,
            arbiter,
            _input: input,
            clock,
            leds,
            mixer,
            player,
            mic,
            mdns,
            api,
            rx: Rx {
                api: Some(api_rx),
                frontend: Some(mic_rx),
                player: Some(player_rx),
                arbiter: Some(arbiter_rx),
                input: input_rx,
                lux: lux_rx,
                sendspin: sendspin_rx,
                zigbee: zigbee_rx,
                updater: Some(updater_rx),
            },
            term,
            int,
            model,
            state: StateFile::new(state_path),
            st,
            timers: Timers::new(),
            pending: HashMap::new(),
            exit: None,
            applied_tz,
            phase: Phase::Idle,
            listen_started_ms: 0,
            hw_mic_muted,
            error_shown: false,
            arbitrating: false,
            arb_claim: None,
            arb_phrase: String::new(),
            arb_level: -100.0,
            arb_audio: Vec::new(),
            arb_command: String::new(),
            commands_blocked_until_ms: 0,
            run: RunBinding::default(),
            stop_sent: false,
            voice_run: 0,
            tts_url: String::new(),
            tts_played: false,
            tts_report_owed: false,
            continue_conversation: false,
            announcing: false,
            ducked: false,
            closing_earcon: false,
            earcon_seq: 0,
            dialog: 0,
            announce_seq: 0,
            timer_ringing: false,
            ring_yielded: false,
            timer_ring_started_ms: 0,
            music_paused_ms: None,
            sendspin_ended_ms: None,
            ha_timers: BTreeMap::new(),
            shown_timer: String::new(),
            shown: HaTimer {
                active: false,
                end_ms: 0,
                seconds_left: 0,
            },
            last_lux_published: -1.0,
            last_lux_ms: 0,
            clock_text: String::new(),
            clock_face: "None".into(),
            release: None,
            update_installing: false,
            update_progress: 0.0,
            pending_check: None,
            install_id: None,
            install_version: String::new(),
            zigbee_status: String::new(),
            sendspin_pairing_prompt: false,
        };
        // The rest of the C++ start(), in its order: every call below only queues commands to
        // the services, which run once `run` is awaited.
        c.publish_update();
        c.timers.set_repeating(
            Tk::UpdateCheck,
            UPDATE_FIRST_CHECK_MS,
            UPDATE_CHECK_PERIOD_MS,
        );
        c.update_detection();
        c.apply_gains(); // a saved mute
        c.update_ui();
        c.clock.play(ClockShot::Boot);
        if changed {
            c.persist();
        }
        Ok(c)
    }

    /// Serves the services' events until the daemon should exit. Leaving while an update
    /// installs: the updater cancels the install when its handle goes (the binary stays as it
    /// was, whereas C++ finished it), so `update_attempt` is cleared; left set, automatic
    /// updates would skip that release. An install that already succeeded (its `Installed` still
    /// queued) keeps it: the new binary clears it. A settings change still pending is written by
    /// `flush_settings` afterwards.
    pub async fn run(&mut self) -> ExitReason {
        let reason = loop {
            if let Some(r) = self.exit.take() {
                break r;
            }
            // Timers first (biased): keys due at one wake-up fire back to back (pulsar_core
            // Timers contract).
            tokio::select! {
                biased;
                k = self.timers.next() => self.on_timer(k),
                () = self.state.due() => self.state.flush(&self.st),
                _ = self.term.recv() => {
                    info!("signal {}, exiting", libc::SIGTERM);
                    break ExitReason::Signal;
                }
                _ = self.int.recv() => {
                    info!("signal {}, exiting", libc::SIGINT);
                    break ExitReason::Signal;
                }
                ev = recv(&mut self.rx.api) => match ev {
                    Some(ev) => self.on_api(ev),
                    None => self.rx.api = None,
                },
                ev = recv(&mut self.rx.player) => match ev {
                    Some(ev) => self.on_player(ev),
                    None => self.rx.player = None,
                },
                ev = recv(&mut self.rx.frontend) => match ev {
                    Some(ev) => self.on_frontend(ev),
                    None => self.rx.frontend = None,
                },
                ev = recv(&mut self.rx.arbiter) => match ev {
                    Some(ArbiterEvent::Decided { claim_id, won, best, peers }) => {
                        if self.arbitrating && self.arb_claim == Some(claim_id) {
                            self.arbitration_done(won, &best, peers);
                        }
                    }
                    None => self.rx.arbiter = None,
                },
                ev = recv(&mut self.rx.input) => match ev {
                    Some(ev) => self.on_input(ev),
                    None => self.rx.input = None,
                },
                ev = recv(&mut self.rx.sendspin) => match ev {
                    Some(ev) => self.on_sendspin(ev),
                    None => self.rx.sendspin = None,
                },
                ev = recv(&mut self.rx.zigbee) => match ev {
                    Some(ev) => self.on_zigbee(ev),
                    None => self.rx.zigbee = None,
                },
                ev = recv(&mut self.rx.updater) => match ev {
                    Some(ev) => self.on_updater(ev),
                    None => self.rx.updater = None,
                },
                lux = recv(&mut self.rx.lux) => match lux {
                    Some(lux) => self.on_lux(lux),
                    None => self.rx.lux = None,
                },
            }
        };
        if let Some(id) = self.install_id {
            let mut installed = false;
            if let Some(rx) = &mut self.rx.updater {
                while let Ok(ev) = rx.try_recv() {
                    if let UpdateEvent::Installed { id: i, result } = ev
                        && i == id
                    {
                        installed = result.is_ok();
                    }
                }
            }
            if !installed {
                info!(
                    "update to {}: cut short by the exit, not installed",
                    self.install_version
                );
                self.st.update_attempt.clear();
                self.persist();
            }
        }
        reason
    }

    fn on_timer(&mut self, k: Tk) {
        match k {
            Tk::UiAfterNotSubscribed | Tk::UiAfterError => self.update_ui(),
            Tk::Watchdog => self.abort_dialog("no progress from Home Assistant, giving up"),
            Tk::TimerTick => self.show_timers(),
            Tk::RingTick => self.ring_timer(),
            Tk::UpdateCheck => self.check_update(),
            Tk::UpdateRetry => self.maybe_auto_update(),
            // exit 0: supervise.sh starts the new binary
            Tk::UpdateExit => self.exit = Some(ExitReason::Updated),
        }
    }

    fn on_player(&mut self, ev: PlayerEvent) {
        match ev {
            // HA media state follows playback (C++ Player::on_state hook).
            PlayerEvent::State(ch, s) => {
                if ch == Channel::Music {
                    match s {
                        PlayState::Paused => self.music_paused_ms = Some(now_ms()),
                        // HA media took the speaker: an ended Sendspin stream is not this
                        // station's music any more, whichever of the two events came first.
                        PlayState::Loading | PlayState::Playing => self.sendspin_ended_ms = None,
                        _ => {}
                    }
                }
                self.publish_media();
            }
            PlayerEvent::Done { id, ok } => {
                if let Some(p) = self.pending.remove(&id) {
                    self.play_done(p, ok);
                }
            }
        }
    }

    fn on_frontend(&mut self, ev: FrontendEvent) {
        match ev {
            FrontendEvent::Wake {
                phrase,
                level_db,
                preroll,
            } => {
                self.on_wake(&phrase, level_db, preroll);
                // After on_wake has switched streaming on where the wake word starts a request.
                self.mic.wake_handled();
            }
            FrontendEvent::Command { word } => self.on_command(&word, false),
            FrontendEvent::Audio(pcm) => {
                if self.phase == Phase::Listening {
                    self.api.voice_send_audio(pcm);
                } else if self.arbitrating {
                    // The request goes on with what was said right after the wake word.
                    self.arb_audio.extend_from_slice(&pcm);
                }
            }
            FrontendEvent::Music(f) => {
                self.leds.music_frame(f, false);
                self.clock.music_frame(f, false);
            }
        }
    }

    /// `st` changed: written a moment after the last change (`StateFile`). `volume=` follows
    /// `volume2=` on the curve of earlier builds.
    fn persist(&mut self) {
        self.st.volume_legacy = pulsar_core::volume::to_legacy(self.model, self.st.volume);
        self.state.changed();
    }

    /// `st` changed in a way that must survive a power cut or a crash right away: before a
    /// restart or a binary swap, or the microphone mute. True = on disk; on false the change
    /// stays pending (`StateFile`).
    #[must_use = "a write that must be on disk has to check it"]
    fn persist_now(&mut self) -> bool {
        self.st.volume_legacy = pulsar_core::volume::to_legacy(self.model, self.st.volume);
        self.state.write_now(&self.st)
    }

    /// Writes a pending change now: main calls it once after `run`, when nothing changes any
    /// more.
    pub fn flush_settings(&mut self) {
        self.state.flush(&self.st);
    }

    fn mic_muted(&self) -> bool {
        self.st.mic_muted || self.hw_mic_muted
    }

    fn sendspin_streaming(&self) -> bool {
        self.sendspin
            .as_ref()
            .is_some_and(SendspinHandle::streaming)
    }

    fn zigbee_flashing(&self) -> bool {
        self.zigbee.as_ref().is_some_and(ZigbeeHandle::flashing)
    }

    fn update_ui(&mut self) {
        self.leds.set_muted(self.mic_muted());
        self.update_commands();
        let s = if self.timer_ringing {
            UiState::TimerRing
        } else {
            match self.phase {
                Phase::Listening => UiState::Listening,
                Phase::Processing => UiState::Thinking,
                Phase::Speaking => UiState::Speaking,
                Phase::Idle if self.arbitrating => UiState::Listening,
                Phase::Idle if self.announcing => UiState::Speaking,
                Phase::Idle => UiState::Idle,
            }
        };
        self.show_ui(s);
    }

    /// Ring and clock.
    fn show_ui(&self, s: UiState) {
        self.leds.set_ui_state(s);
        self.clock.set_ui_state(s);
    }

    fn update_detection(&self) {
        self.mic.set_detection(!self.mic_muted());
    }
}
