//! Entities: the golden bytes of the C++ api_selftest configuration, the command parsers at
//! their limits, publishing.

use super::*;
use crate::server::entities::key;
use crate::types::*;

const RAW: &str = include_str!("../../../tests/golden/selftest_raw.txt");

/// The C++ api_selftest's identity and configuration (satellite/tools/api_selftest.cpp, tag rust-port-base).
fn selftest(compilation_time: &str) -> (DeviceIdentity, ApiConfig) {
    let s = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let ww = |id: &str, phrase: &str| WakeWordInfo {
        id: id.into(),
        phrase: phrase.into(),
        languages: s(&["ru"]),
    };
    let id = DeviceIdentity {
        name: "pulsar-selftest".into(),
        friendly_name: "Яндекс Станция Миди (selftest)".into(),
        mac: "02:00:00:C0:FF:EE".into(),
        model: "Yandex Station Midi".into(),
        board: "cucumber".into(),
        manufacturer: "Yandex".into(),
        project_version: "0.1.0".into(),
        compilation_time: compilation_time.into(),
    };
    let cfg = ApiConfig {
        port: 0,
        wake_words: vec![ww("alisa", "Алиса"), ww("yandex", "Яндекс")],
        active_wake_words: s(&["alisa"]),
        light_effects: s(&["Rainbow", "Pulse"]),
        clock_faces: s(&["None", "Smiley"]),
        music_styles: s(&["Off", "All styles", "Spectrum"]),
        case_colors: s(&["None", "blue"]),
        clock_music_styles: s(&["Off", "Levels"]),
        zigbee_modes: s(&["Coordinator", "Router"]),
        key_path: None,
    };
    (id, cfg)
}

fn publish_selftest_states(h: &ApiHandle) {
    h.publish_media(MediaState::Idle, 0.5, false);
    h.publish_switch(SwitchId::MicMute, false);
    h.publish_switch(SwitchId::Clock, true);
    h.publish_switch(SwitchId::VoiceIndication, true);
    h.publish_number(NumberId::LedSpeed, 1.0);
    h.publish_select(SelectId::MusicViz, "All styles");
    h.publish_select(SelectId::CaseColor, "None");
    h.publish_select(SelectId::ClockMusic, "Off");
    h.publish_binary_sensor(BinarySensorId::HwMicMute, false);
    h.publish_number(NumberId::ClockBrightness, 50.0);
    h.publish_light(&LightState {
        on: true,
        brightness: 0.8,
        r: 0.0,
        g: 0.5,
        b: 1.0,
        effect: String::new(),
    });
    h.publish_number(NumberId::ClockScroll, 400.0);
    h.publish_text(TextId::ClockText, "");
    h.publish_select(SelectId::ClockFace, "None");
    h.publish_select(SelectId::ZigbeeMode, "Coordinator");
    h.publish_text_sensor(TextSensorId::Zigbee, "coordinator: PAN 0x1234, channel 15");
}

fn start_with(id: DeviceIdentity, cfg: ApiConfig) -> (ApiHandle, UnboundedReceiver<ApiEvent>, u16) {
    let s = spawn(id, cfg).unwrap();
    let h = ApiHandle {
        cmds: s.cmds,
        shared: s.shared,
    };
    (h, s.events, s.port)
}

/// Message type and fixed32 key of a state message (field 1), for the masked comparison.
fn state_key(m: &[u8]) -> Option<u32> {
    field(m, 1)
        .filter(|f| f.wt == WireType::Fixed32)
        .map(|f| f.u32())
}

#[tokio::test]
async fn selftest_matches_cxx_golden_bytes() {
    let golden: Vec<(u32, Vec<u8>)> = RAW
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let (t, hex) = l.split_once(' ').unwrap();
            (t.parse().unwrap(), unhex(hex))
        })
        .collect();
    // compilation_time is the C++ build's __DATE__ / __TIME__.
    let info = &golden
        .iter()
        .find(|(t, _)| *t == msg::DEVICE_INFO_RESP)
        .unwrap()
        .1;
    let built = field(info, 5).unwrap().str();
    local()
        .run_until(async {
            let (id, cfg) = selftest(&built);
            let (h, _rx, port) = start_with(id, cfg);
            publish_selftest_states(&h);
            let mut p = Peer::connect(port).await;
            let mut got = Vec::new();
            let mut w = Writer::new();
            w.str(1, "raw", false);
            w.u32(2, 1, false);
            w.u32(3, 10, false);
            p.send(msg::HELLO_REQ, &w).await;
            got.push(p.recv().await.unwrap());
            got.push(p.recv().await.unwrap());
            for req in [
                msg::DEVICE_INFO_REQ,
                msg::DEVICE_CAPS_REQ,
                msg::VA_CONFIG_REQ,
            ] {
                p.send(req, &Writer::new()).await;
                got.push(p.recv().await.unwrap());
            }
            p.send(msg::LIST_ENTITIES_REQ, &Writer::new()).await;
            loop {
                let m = p.recv().await.unwrap();
                let done = m.0 == msg::LIST_DONE;
                got.push(m);
                if done {
                    break;
                }
            }
            p.send(msg::SUBSCRIBE_STATES_REQ, &Writer::new()).await;
            while got.len() < golden.len() {
                got.push(p.recv().await.unwrap());
            }
            assert!(p.sync().await.is_empty());
            assert_eq!(got.len(), golden.len());
            for (i, (g, r)) in golden.iter().zip(&got).enumerate() {
                assert_eq!(g.0, r.0, "message {i}");
                // Diagnostics of the host: Wi-Fi signal, uptime, IP.
                let k = state_key(&g.1);
                let host_dependent = matches!(
                    (g.0, k),
                    (
                        msg::SENSOR_STATE,
                        Some(key::SENSOR_RSSI | key::SENSOR_UPTIME)
                    ) | (msg::TEXT_SENSOR_STATE, Some(key::TEXT_IP))
                );
                if host_dependent {
                    assert_eq!(k, state_key(&r.1), "message {i}");
                } else {
                    assert_eq!(
                        crate::proto::golden::hex(&g.1),
                        crate::proto::golden::hex(&r.1),
                        "message {i} type {}",
                        g.0
                    );
                }
            }
        })
        .await;
}

struct Cmd<'a> {
    p: &'a mut Peer,
}

impl Cmd<'_> {
    async fn send(&mut self, t: u32, w: Writer) {
        self.p.send(t, &w).await;
    }
}

fn kv(k: u32, f: impl FnOnce(&mut Writer)) -> Writer {
    let mut w = Writer::new();
    w.fixed32(1, k, true);
    f(&mut w);
    w
}

/// Every event that came, after a round trip.
async fn drain(p: &mut Peer, rx: &mut UnboundedReceiver<ApiEvent>) -> Vec<ApiEvent> {
    p.sync().await;
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

#[tokio::test]
async fn commands_are_validated_like_cxx() {
    local()
        .run_until(async {
            let (id, cfg) = selftest("now");
            let (h, mut rx, port) = start_with(id, cfg);
            let mut p = Peer::connect(port).await;
            p.hello("HA").await;
            event(&mut rx).await;
            let mut c = Cmd { p: &mut p };

            // Numbers: NaN dropped, values clamped to each entity's range.
            for (k, v) in [
                (key::NUMBER_CLOCK_BRI, f32::NAN),
                (key::NUMBER_CLOCK_BRI, 150.0),
                (key::NUMBER_CLOCK_BRI, -5.0),
                (key::NUMBER_CLOCK_SCROLL, 50.0),
                (key::NUMBER_CLOCK_SCROLL, 5000.0),
                (key::NUMBER_LED_SPEED, 0.1),
                (key::NUMBER_LED_SPEED, 9.0),
                (999, 1.0),
            ] {
                c.send(msg::NUMBER_CMD, kv(k, |w| w.flt(2, v, true))).await;
            }
            assert_eq!(
                drain(c.p, &mut rx).await,
                [
                    ApiEvent::Number(NumberId::ClockBrightness, 100.0),
                    ApiEvent::Number(NumberId::ClockBrightness, 0.0),
                    ApiEvent::Number(NumberId::ClockScroll, 100.0),
                    ApiEvent::Number(NumberId::ClockScroll, 2000.0),
                    ApiEvent::Number(NumberId::LedSpeed, 0.25),
                    ApiEvent::Number(NumberId::LedSpeed, 4.0),
                ]
            );

            // Text: at most 64 characters, counted on the bytes as sent (as C++ did).
            let text = |b: Vec<u8>| kv(key::TEXT_CLOCK, |w| w.bytes(2, &b, true));
            c.send(msg::TEXT_CMD, text("я".repeat(64).into_bytes()))
                .await;
            c.send(msg::TEXT_CMD, text("я".repeat(65).into_bytes()))
                .await;
            // Counted on the decoded text: "a" + 63 stray continuation bytes is 64 characters
            // (63 U+FFFD), one more is too long.
            let mut odd = b"a".to_vec();
            odd.extend_from_slice(&[0x80; 63]);
            c.send(msg::TEXT_CMD, text(odd.clone())).await;
            let mut too_odd = odd.clone();
            too_odd.push(0x80);
            c.send(msg::TEXT_CMD, text(too_odd)).await;
            assert_eq!(
                drain(c.p, &mut rx).await,
                [
                    ApiEvent::Text(TextId::ClockText, "я".repeat(64)),
                    ApiEvent::Text(
                        TextId::ClockText,
                        String::from_utf8_lossy(&odd).into_owned()
                    ),
                ]
            );

            // Selects: only the entity's options; switches: known keys only.
            let select = |k, s: &str| kv(k, |w| w.str(2, s, true));
            c.send(msg::SELECT_CMD, select(key::SELECT_CLOCK_FACE, "Smiley"))
                .await;
            c.send(msg::SELECT_CMD, select(key::SELECT_CLOCK_FACE, "Frown"))
                .await;
            c.send(msg::SELECT_CMD, select(key::SELECT_MUSIC_VIZ, "Spectrum"))
                .await;
            c.send(msg::SELECT_CMD, select(key::SELECT_CASE_COLOR, "Spectrum"))
                .await;
            c.send(msg::SELECT_CMD, select(key::SELECT_ZIGBEE_MODE, "Router"))
                .await;
            c.send(msg::SELECT_CMD, select(key::SELECT_CLOCK_MUSIC, "Levels"))
                .await;
            c.send(
                msg::SWITCH_CMD,
                kv(key::SW_NEAREST_ANSWERS, |w| w.boolean(2, true, true)),
            )
            .await;
            c.send(msg::SWITCH_CMD, kv(key::SW_VOICE_INDICATION, |_| {}))
                .await;
            c.send(
                msg::SWITCH_CMD,
                kv(key::MEDIA, |w| w.boolean(2, true, true)),
            )
            .await;
            assert_eq!(
                drain(c.p, &mut rx).await,
                [
                    ApiEvent::Select(SelectId::ClockFace, "Smiley".into()),
                    ApiEvent::Select(SelectId::MusicViz, "Spectrum".into()),
                    ApiEvent::Select(SelectId::ZigbeeMode, "Router".into()),
                    ApiEvent::Select(SelectId::ClockMusic, "Levels".into()),
                    ApiEvent::Switch(SwitchId::NearestAnswers, true),
                    ApiEvent::Switch(SwitchId::VoiceIndication, false),
                ]
            );

            // Media: volume clamped, unknown command values dropped, fields only when set.
            c.send(
                msg::MEDIA_PLAYER_CMD,
                kv(key::MEDIA, |w| {
                    w.boolean(4, true, false);
                    w.flt(5, 1.5, false);
                    w.boolean(2, true, false);
                    w.u32(3, 99, false);
                }),
            )
            .await;
            c.send(
                msg::MEDIA_PLAYER_CMD,
                kv(key::MEDIA, |w| {
                    w.boolean(6, true, false);
                    w.str(7, "http://x/a.mp3", false);
                    w.boolean(8, true, false);
                    w.boolean(9, true, false);
                    w.boolean(2, true, false); // command PLAY (0 = absent field)
                }),
            )
            .await;
            assert_eq!(
                drain(c.p, &mut rx).await,
                [
                    ApiEvent::MediaCommand(MediaCommandRequest {
                        command: None,
                        volume: Some(1.0),
                        media_url: None,
                        announcement: None,
                    }),
                    ApiEvent::MediaCommand(MediaCommandRequest {
                        command: Some(MediaCommand::Play),
                        volume: None,
                        media_url: Some("http://x/a.mp3".into()),
                        announcement: Some(true),
                    }),
                ]
            );

            // Update: the entity's key and commands 0..2 only.
            for (k, cmd) in [(key::UPDATE, 2), (key::UPDATE, 3), (key::MEDIA, 1)] {
                c.send(msg::UPDATE_CMD, kv(k, |w| w.u32(2, cmd, false)))
                    .await;
            }
            assert_eq!(
                drain(c.p, &mut rx).await,
                [ApiEvent::Update(UpdateCommand::Check)]
            );

            // Actions: argument counts, sint32 and legacy int arguments, effect and face checks.
            let action = |k, args: &[(Option<&str>, Option<i32>, Option<i32>)]| {
                let mut w = Writer::new();
                w.fixed32(1, k, true);
                for (s, sint, legacy) in args {
                    let mut a = Writer::new();
                    if let Some(s) = s {
                        a.str(4, s, true);
                    }
                    if let Some(i) = *sint {
                        a.u32(5, ((i << 1) ^ (i >> 31)) as u32, true);
                    }
                    if let Some(i) = *legacy {
                        a.i32(2, i, true);
                    }
                    w.msg(2, &a);
                }
                w
            };
            let s = |v| (Some(v), None, None);
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(key::ACTION_CLOCK_TEXT, &[s("Hi"), (None, Some(-5), None)]),
            )
            .await;
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(key::ACTION_CLOCK_TEXT, &[s("Hi")]),
            )
            .await;
            let long = "x".repeat(65);
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(key::ACTION_CLOCK_TEXT, &[s(&long), (None, Some(1), None)]),
            )
            .await;
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(
                    key::ACTION_CLOCK_FACE,
                    &[s("Smiley"), (None, None, Some(7))],
                ),
            )
            .await;
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(key::ACTION_CLOCK_FACE, &[s("Frown"), (None, None, Some(7))]),
            )
            .await;
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(key::ACTION_CLOCK_FACE, &[s(""), (None, None, Some(0))]),
            )
            .await;
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(
                    key::ACTION_LED_SHOW,
                    &[s("None"), s("#ff0000"), (None, Some(3), None)],
                ),
            )
            .await;
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(
                    key::ACTION_LED_SHOW,
                    &[s("Strobe"), s("#ff0000"), (None, Some(3), None)],
                ),
            )
            .await;
            c.send(
                msg::EXECUTE_SERVICE_REQ,
                action(
                    key::ACTION_LED_SHOW,
                    &[s("Pulse"), s("red"), (None, Some(3), None)],
                ),
            )
            .await;
            c.send(msg::EXECUTE_SERVICE_REQ, action(999, &[s("a"), s("b")]))
                .await;
            let act = |id, v: &str, seconds| ApiEvent::Action {
                id,
                value: v.into(),
                seconds,
            };
            let show = |e: &str, color: &str| ApiEvent::LedShow {
                effect: e.into(),
                color: color.into(),
                seconds: 3,
            };
            assert_eq!(
                drain(c.p, &mut rx).await,
                [
                    act(ActionId::ClockShowText, "Hi", -5),
                    act(ActionId::ClockShowFace, "Smiley", 7),
                    act(ActionId::ClockShowFace, "", 0),
                    show("", "#ff0000"),
                    show("Pulse", "red"),
                ]
            );

            // Light: rgb scaled by color_brightness, effect "None" -> "", transition default and
            // cap; the server echoes the normalised state to state subscribers.
            c.send(
                msg::LIGHT_CMD,
                kv(key::LIGHT_RING, |w| {
                    w.boolean(2, true, false);
                    w.boolean(3, true, false);
                    w.boolean(6, true, false);
                    w.flt(7, 1.0, false);
                    w.flt(8, 0.5, false);
                    w.flt(9, 2.0, false);
                    w.boolean(20, true, false);
                    w.flt(21, 0.5, false);
                    w.boolean(18, true, false);
                    w.str(19, "None", false);
                }),
            )
            .await;
            c.send(
                msg::LIGHT_CMD,
                kv(key::LIGHT_RING, |w| {
                    w.boolean(20, true, false);
                    w.flt(21, 1.0, false);
                    w.boolean(14, true, false);
                    w.u32(15, 600_000, false);
                }),
            )
            .await;
            let light = |r, g, b, transition_ms| ApiEvent::Light {
                state: LightState {
                    on: true,
                    brightness: 1.0,
                    r,
                    g,
                    b,
                    effect: String::new(),
                },
                transition_ms,
            };
            assert_eq!(
                drain(c.p, &mut rx).await,
                [light(0.5, 0.25, 0.5, 1000), light(1.0, 0.5, 1.0, 60_000)]
            );

            // Buttons: Zigbee (entities exist here) and Restart.
            c.send(msg::BUTTON_CMD, kv(key::BUTTON_ZIGBEE_LEAVE, |_| {}))
                .await;
            c.send(msg::BUTTON_CMD, kv(key::BUTTON_RESTART, |_| {}))
                .await;
            c.send(msg::BUTTON_CMD, kv(key::MEDIA, |_| {})).await;
            assert_eq!(
                drain(c.p, &mut rx).await,
                [ApiEvent::Button(ButtonId::ZigbeeLeave), ApiEvent::Restart]
            );

            // Voice assistant configuration: one known wake word.
            let mut w = Writer::new();
            for ww in ["nope", "yandex", "alisa"] {
                w.str(1, ww, true);
            }
            c.send(msg::VA_SET_CONFIG, w).await;
            let mut w = Writer::new();
            w.str(1, "nope", true);
            c.send(msg::VA_SET_CONFIG, w).await;
            // Timers: unknown event values dropped.
            for ev in [3, 9] {
                let mut w = Writer::new();
                w.u32(1, ev, false);
                w.str(2, "t1", false);
                w.u32(5, 42, false);
                w.boolean(6, true, false);
                c.send(msg::VA_TIMER_EVENT_RESP, w).await;
            }
            // Pipeline events only from the voice owner (nobody here).
            let mut w = Writer::new();
            w.u32(1, 1, false);
            c.send(msg::VA_EVENT_RESP, w).await;
            let mut w = Writer::new();
            w.str(1, "http://x/a.mp3", false);
            w.str(2, "Привет", false);
            w.boolean(4, true, false);
            c.send(msg::VA_ANNOUNCE_REQ, w).await;
            assert_eq!(
                drain(c.p, &mut rx).await,
                [
                    ApiEvent::ActiveWakeWords(vec!["yandex".into()]),
                    ApiEvent::Timer(TimerInfo {
                        event: TimerEvent::Finished,
                        id: "t1".into(),
                        name: String::new(),
                        total_seconds: 0,
                        seconds_left: 42,
                        is_active: true,
                    }),
                    ApiEvent::Announce(AnnounceRequest {
                        media_id: "http://x/a.mp3".into(),
                        text: "Привет".into(),
                        preannounce_media_id: String::new(),
                        start_conversation: true,
                    }),
                ]
            );
            drop(h);
        })
        .await;
}

#[tokio::test]
async fn voice_events_come_from_the_owner() {
    local()
        .run_until(async {
            let (id, cfg) = selftest("now");
            let (_h, mut rx, port) = start_with(id, cfg);
            let mut p = Peer::connect(port).await;
            p.hello("HA").await;
            p.subscribe_voice(true).await;
            assert_eq!(
                drain(&mut p, &mut rx).await[1..],
                [ApiEvent::VoiceSubscribed(true)]
            );
            let ev = |t: u32, data: &[(&str, &str)]| {
                let mut w = Writer::new();
                w.u32(1, t, false);
                for (k, v) in data {
                    let mut d = Writer::new();
                    d.str(1, k, false);
                    d.str(2, v, false);
                    w.msg(2, &d);
                }
                w
            };
            p.send(msg::VA_EVENT_RESP, &ev(4, &[("text", "a"), ("text", "b")]))
                .await;
            p.send(msg::VA_EVENT_RESP, &ev(13, &[])).await;
            p.send(msg::VA_EVENT_RESP, &ev(100, &[])).await;
            assert_eq!(
                drain(&mut p, &mut rx).await,
                [
                    ApiEvent::VoiceEvent {
                        event: VaEvent::SttEnd,
                        data: [("text".to_owned(), "b".to_owned())].into(),
                    },
                    // A type newer than this build still reaches the controller (watchdog).
                    ApiEvent::VoiceEvent {
                        event: VaEvent::Unknown(13),
                        data: Default::default(),
                    },
                    ApiEvent::VoiceEvent {
                        event: VaEvent::IntentProgress,
                        data: Default::default(),
                    },
                ]
            );
        })
        .await;
}

#[tokio::test]
async fn states_go_to_subscribers_and_zigbee_entities_need_modes() {
    local()
        .run_until(async {
            let (id, mut cfg) = selftest("now");
            cfg.zigbee_modes.clear();
            let (h, mut rx, port) = start_with(id, cfg);
            let mut p = Peer::connect(port).await;
            p.hello("HA").await;
            event(&mut rx).await;
            // Without a state subscription nothing is pushed, events are dropped.
            h.publish_switch(SwitchId::MicMute, true);
            h.fire_event(EventId::Button, "action");
            assert!(p.sync().await.is_empty());
            // No Zigbee entities, states or buttons.
            p.send(msg::LIST_ENTITIES_REQ, &Writer::new()).await;
            let mut keys = Vec::new();
            loop {
                let (t, m) = p.recv().await.unwrap();
                if t == msg::LIST_DONE {
                    break;
                }
                keys.push(field(&m, 2).unwrap().u32());
            }
            assert!(!keys.contains(&key::SELECT_ZIGBEE_MODE));
            assert!(!keys.contains(&key::TEXT_ZIGBEE));
            assert!(!keys.contains(&key::BUTTON_ZIGBEE_JOIN));
            p.send(msg::SUBSCRIBE_STATES_REQ, &Writer::new()).await;
            let states = p.sync().await;
            // Cached value replayed; no Zigbee states.
            let mic = states
                .iter()
                .find(|(t, m)| *t == msg::SWITCH_STATE && state_key(m) == Some(key::SW_MIC_MUTE))
                .unwrap();
            assert!(field(&mic.1, 2).unwrap().boolean());
            assert!(
                !states
                    .iter()
                    .any(|(_, m)| state_key(m) == Some(key::TEXT_ZIGBEE))
            );
            // Subscribed: publishes and events arrive; the Zigbee text sensor still does not.
            h.publish_text_sensor(TextSensorId::Zigbee, "x");
            h.publish_sensor(SensorId::Illuminance, 12.5);
            h.fire_event(EventId::Button, "action_long");
            h.publish_update(&UpdateState {
                current_version: "v1".into(),
                latest_version: "v2".into(),
                ..UpdateState::default()
            });
            let got = p.sync().await;
            let types: Vec<u32> = got.iter().map(|(t, _)| *t).collect();
            assert_eq!(
                types,
                [msg::SENSOR_STATE, msg::EVENT_RESP, msg::UPDATE_STATE]
            );
            assert_eq!(field(&got[0].1, 2).unwrap().flt(), 12.5);
            assert_eq!(field(&got[1].1, 2).unwrap().str(), "action_long");
            assert_eq!(field(&got[2].1, 7).unwrap().str(), "v2");
            // Zigbee buttons are ignored without the entities.
            p.send(msg::BUTTON_CMD, &kv(key::BUTTON_ZIGBEE_JOIN, |_| {}))
                .await;
            assert!(drain(&mut p, &mut rx).await.is_empty());
        })
        .await;
}

#[tokio::test]
async fn entity_list_flood_yields_to_other_clients() {
    local()
        .run_until(async {
            let (id, cfg) = selftest("now");
            let (_h, mut rx, port) = start_with(id, cfg);
            let mut flood = Peer::connect(port).await;
            flood.hello("flood").await;
            let mut other = Peer::connect(port).await;
            other.hello("other").await;
            // 300 ListEntities requests in one write: ~1 MiB of replies, parsed in several
            // batches; the other client is served meanwhile and nothing is lost.
            let mut req = Vec::new();
            for _ in 0..300 {
                append_plain(&mut req, msg::LIST_ENTITIES_REQ, &[]);
            }
            flood.write(&req).await;
            assert!(other.sync().await.is_empty());
            let mut done = 0;
            while done < 300 {
                if flood.recv().await.unwrap().0 == msg::LIST_DONE {
                    done += 1;
                }
            }
            assert!(flood.sync().await.is_empty());
            assert!(other.sync().await.is_empty());
            while rx.try_recv().is_ok() {}
        })
        .await;
}

#[tokio::test]
async fn pending_batch_does_not_read_more_input() {
    local()
        .run_until(async {
            let (id, cfg) = selftest("now");
            let (_h, rx, port) = start_with(id, cfg);
            super::MAX_RBUF.with(|m| m.set(0));
            let sock = TcpSocket::new_v4().unwrap();
            sock.set_recv_buffer_size(4096).unwrap();
            let s = sock
                .connect(SocketAddr::from(([127, 0, 0, 1], port)))
                .await
                .unwrap();
            let mut p = Peer::with(s);
            p.hello("flood").await;
            let mut req = Vec::new();
            for _ in 0..1000 {
                append_plain(&mut req, msg::LIST_ENTITIES_REQ, &[]);
            }
            // Never reads; keeps sending until the server closes it or the writes block.
            for _ in 0..200 {
                let w = tokio::time::timeout(Duration::from_millis(100), p.s.write_all(&req));
                match w.await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
                if !rx.is_empty() {
                    break;
                }
            }
            let max = super::MAX_RBUF.with(|m| m.get());
            assert!(max > 0);
            assert!(
                max <= READ_PER_WAKEUP + req.len(),
                "rbuf grew to {max} bytes"
            );
        })
        .await;
}

#[tokio::test]
async fn eof_with_pending_batch_still_answers_everything() {
    local()
        .run_until(async {
            let (id, cfg) = selftest("now");
            let (_h, _rx, port) = start_with(id, cfg);
            let mut p = Peer::connect(port).await;
            p.hello("eof").await;
            let mut req = Vec::new();
            for _ in 0..300 {
                append_plain(&mut req, msg::LIST_ENTITIES_REQ, &[]);
            }
            p.write(&req).await;
            p.s.shutdown().await.unwrap();
            let mut done = 0;
            while let Some((t, _)) = p.recv().await {
                if t == msg::LIST_DONE {
                    done += 1;
                }
            }
            assert_eq!(done, 300);
        })
        .await;
}
