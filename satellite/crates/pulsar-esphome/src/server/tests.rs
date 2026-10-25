//! The server on a `LocalSet` with real TCP clients on 127.0.0.1: plaintext and Noise (the
//! initiator on snow), the voice owner, eviction, backlog overflow, keys, timeouts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::LocalSet;
use tokio::time::{Instant, timeout};

use super::*;
use crate::ApiHandle;
use crate::frame::tests::{Initiator, MAC, NAME};
use crate::frame::{append_plain, parse_noise, parse_plain};
use crate::key::Psk;
use crate::proto::Field;
use crate::proto::golden::{cases, unhex};

const KEY_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn identity() -> DeviceIdentity {
    DeviceIdentity {
        name: NAME.into(),
        friendly_name: "Станция".into(),
        mac: "AA:BB:CC:DD:EE:FF".into(),
        model: "Yandex Station Midi".into(),
        board: "cucumber".into(),
        manufacturer: "Yandex".into(),
        project_version: "v1.2.3".into(),
        compilation_time: "Jan  1 2026, 00:00:00".into(),
    }
}

fn config(key_path: Option<PathBuf>) -> ApiConfig {
    ApiConfig {
        port: 0,
        key_path,
        ..ApiConfig::default()
    }
}

/// Server on an ephemeral port.
fn start(key_path: Option<PathBuf>) -> (ApiHandle, UnboundedReceiver<ApiEvent>, u16) {
    let s = spawn(identity(), config(key_path)).unwrap();
    let handle = ApiHandle {
        cmds: s.cmds,
        shared: s.shared,
    };
    (handle, s.events, s.port)
}

async fn event(rx: &mut UnboundedReceiver<ApiEvent>) -> ApiEvent {
    timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("no event")
        .expect("server gone")
}

fn fields(p: &[u8]) -> Vec<Field<'_>> {
    let mut r = Reader::new(p);
    let mut v = Vec::new();
    while let Some(f) = r.next() {
        v.push(f);
    }
    assert!(!r.error());
    v
}

fn field<'a>(p: &'a [u8], num: u32) -> Option<Field<'a>> {
    fields(p).into_iter().find(|f| f.num == num)
}

/// A temporary directory, removed on drop.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("pulsar-esphome-srv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An API client: aioesphomeapi's side of the framing.
struct Peer {
    s: TcpStream,
    rbuf: Vec<u8>,
    noise: Option<Initiator>,
    wait: Duration,
}

impl Peer {
    async fn connect(port: u16) -> Self {
        let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        Self::with(s)
    }

    fn with(s: TcpStream) -> Self {
        Self {
            s,
            rbuf: Vec::new(),
            noise: None,
            wait: Duration::from_secs(10),
        }
    }

    /// Noise session; Err = the server's rejection text.
    async fn noise(port: u16, psk: &Psk) -> Result<Self, String> {
        let mut p = Self::connect(port).await;
        let (init, hello) = Initiator::new(psk);
        p.write(&hello).await;
        let server_hello = p.raw_noise().await.expect("server hello");
        assert_eq!(server_hello, format!("\x01{NAME}\0{MAC}\0").as_bytes());
        p.noise = Some(init);
        let body = p.raw_noise().await.expect("handshake reply");
        p.noise.as_mut().unwrap().finish(&body)?;
        Ok(p)
    }

    async fn write(&mut self, b: &[u8]) {
        self.s.write_all(b).await.unwrap();
    }

    async fn send(&mut self, msg_type: u32, w: &Writer) {
        let bytes = match &mut self.noise {
            Some(n) => n.encrypt(msg_type as u16, w.data()),
            None => {
                let mut v = Vec::new();
                append_plain(&mut v, msg_type, w.data());
                v
            }
        };
        self.write(&bytes).await;
    }

    /// More bytes; false on EOF or reset.
    async fn fill(&mut self) -> bool {
        self.rbuf.reserve(65536);
        timeout(self.wait, self.s.read_buf(&mut self.rbuf))
            .await
            .expect("timed out waiting for the server")
            .unwrap_or(0)
            > 0
    }

    /// One frame outside the cipher (hellos, handshake, rejections).
    async fn raw_noise(&mut self) -> Option<Vec<u8>> {
        loop {
            if !self.rbuf.is_empty()
                && let crate::frame::NoiseFrame::Frame { body, used } =
                    parse_noise(&self.rbuf, false)
            {
                let body = body.to_vec();
                self.rbuf.drain(..used);
                return Some(body);
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    /// Next message; None on EOF.
    async fn recv(&mut self) -> Option<(u32, Vec<u8>)> {
        loop {
            if !self.rbuf.is_empty() {
                match &mut self.noise {
                    Some(n) => {
                        if let crate::frame::NoiseFrame::Frame { body, used } =
                            parse_noise(&self.rbuf, false)
                        {
                            let (t, m) = n.decrypt(body);
                            self.rbuf.drain(..used);
                            return Some((u32::from(t), m));
                        }
                    }
                    None => match parse_plain(&self.rbuf) {
                        Plain::Frame {
                            msg_type,
                            msg,
                            used,
                        } => {
                            let m = (msg_type, msg.to_vec());
                            self.rbuf.drain(..used);
                            return Some(m);
                        }
                        Plain::Incomplete => {}
                        other => panic!("bad frame from the server: {other:?}"),
                    },
                }
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    async fn expect(&mut self, msg_type: u32) -> Vec<u8> {
        let (t, m) = self.recv().await.expect("connection closed");
        assert_eq!(t, msg_type, "message {m:?}");
        m
    }

    /// Hello; returns the HelloResponse after the GetTimeRequest that follows it.
    async fn hello(&mut self, info: &str) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(1, info, false);
        w.u32(2, 1, false);
        w.u32(3, 10, false);
        self.send(msg::HELLO_REQ, &w).await;
        let resp = self.expect(msg::HELLO_RESP).await;
        self.expect(msg::GET_TIME_REQ).await;
        resp
    }

    async fn subscribe_voice(&mut self, on: bool) {
        let mut w = Writer::new();
        w.boolean(1, on, false);
        self.send(msg::SUBSCRIBE_VA_REQ, &w).await;
    }

    /// A ping round trip: everything the server sent before the reply is returned.
    async fn sync(&mut self) -> Vec<(u32, Vec<u8>)> {
        self.send(msg::PING_REQ, &Writer::new()).await;
        let mut before = Vec::new();
        loop {
            let m = self.recv().await.expect("connection closed");
            if m.0 == msg::PING_RESP {
                return before;
            }
            before.push(m);
        }
    }

    /// Reads until the server closes; returns the bytes that came.
    async fn closed(&mut self) -> Vec<u8> {
        while self.fill().await {}
        std::mem::take(&mut self.rbuf)
    }
}

fn local() -> LocalSet {
    LocalSet::new()
}

#[test]
fn posix_tz_golden() {
    for c in cases("posix_tz") {
        let tz = posix_tz_from_parsed(&unhex(c[0]));
        let want = if c[1] == "-" { "" } else { c[1] };
        assert_eq!(tz, want, "posix_tz {}", c[0]);
    }
}

#[test]
fn txt_records_follow_the_key_state() {
    let id = identity();
    let base = [
        "friendly_name=Станция",
        "version=2026.9.0",
        "mac=aabbccddeeff",
        "platform=host",
        "board=cucumber",
        "network=wifi",
        "project_name=pulsar.satellite",
        "project_version=v1.2.3",
    ];
    assert_eq!(txt_records(&id, false, false), base);
    let mut with = base.map(String::from).to_vec();
    with.push("api_encryption_supported=Noise_NNpsk0_25519_ChaChaPoly_SHA256".into());
    assert_eq!(txt_records(&id, true, false), with);
    with[8] = "api_encryption=Noise_NNpsk0_25519_ChaChaPoly_SHA256".into();
    assert_eq!(txt_records(&id, true, true), with);
    assert_eq!(mac_plain("0A:1b:FF"), "0a1bff");
}

#[test]
fn voice_owner_is_the_latest_subscriber_past_hello() {
    let t = Instant::now();
    let s = |id, hello, voice_wanted| Standing {
        id,
        hello,
        voice: false,
        voice_wanted,
        last_rx: t,
    };
    assert_eq!(voice_choice([].into_iter()), None);
    assert_eq!(
        voice_choice([s(1, true, 0), s(2, true, 0)].into_iter()),
        None
    );
    assert_eq!(
        voice_choice([s(1, true, 3), s(2, true, 5), s(3, true, 4)].into_iter()),
        Some(2)
    );
    // A subscription before hello does not count.
    assert_eq!(
        voice_choice([s(1, true, 3), s(2, false, 5)].into_iter()),
        Some(1)
    );
}

#[test]
fn eviction_rank() {
    let t = Instant::now();
    let s = |id, hello, voice, voice_wanted, age: u64| Standing {
        id,
        hello,
        voice,
        voice_wanted,
        last_rx: t + Duration::from_secs(100 - age),
    };
    // Pending connections neither count nor go.
    assert_eq!(
        eviction_victim([s(1, false, false, 0, 99)].into_iter()),
        (None, 0)
    );
    // Without interest in the voice pipeline first, the least recently heard among those.
    assert_eq!(
        eviction_victim(
            [
                s(1, true, true, 4, 90),
                s(2, true, false, 2, 80),
                s(3, true, false, 0, 10),
                s(4, true, false, 0, 20),
                s(5, false, false, 0, 99),
            ]
            .into_iter()
        ),
        (Some(4), 4)
    );
    // Then subscribers that do not own it; the owner last.
    assert_eq!(
        eviction_victim(
            [
                s(1, true, true, 4, 90),
                s(2, true, false, 2, 10),
                s(3, true, false, 3, 30)
            ]
            .into_iter()
        ),
        (Some(3), 3)
    );
    assert_eq!(
        eviction_victim([s(1, true, true, 4, 0)].into_iter()),
        (Some(1), 1)
    );
    // Ties go to the earlier connection.
    assert_eq!(
        eviction_victim([s(1, true, false, 0, 5), s(2, true, false, 0, 5)].into_iter()),
        (Some(1), 2)
    );
}

#[tokio::test]
async fn plaintext_session() {
    local()
        .run_until(async {
            let (h, mut rx, port) = start(None);
            assert!(!h.connected());
            assert_eq!(h.mdns_txt().len(), 8);
            let mut p = Peer::connect(port).await;
            let resp = p.hello("Home Assistant 2026.10").await;
            assert_eq!(field(&resp, 1).unwrap().u32(), 1);
            assert_eq!(field(&resp, 2).unwrap().u32(), 15);
            assert_eq!(field(&resp, 3).unwrap().str(), "pulsar v1.2.3");
            assert_eq!(field(&resp, 4).unwrap().str(), NAME);
            assert_eq!(
                event(&mut rx).await,
                ApiEvent::ClientConnected {
                    client_info: "Home Assistant 2026.10".into()
                }
            );
            assert!(h.connected());

            // GetTimeResponse: the TZ string wins over the parsed zone, which is the fallback.
            let mut w = Writer::new();
            w.fixed32(1, 1_700_000_000, false);
            w.bytes(3, &unhex("08dfa801"), false);
            p.send(msg::GET_TIME_RESP, &w).await;
            let mut w = Writer::new();
            w.fixed32(1, 1_700_000_001, false);
            w.str(2, "MSK-3", false);
            w.bytes(3, &unhex("08dfa801"), false);
            p.send(msg::GET_TIME_RESP, &w).await;
            // Epoch 0 is not a time.
            let mut w = Writer::new();
            w.str(2, "UTC0", false);
            p.send(msg::GET_TIME_RESP, &w).await;
            // Unknown messages are ignored, the session goes on.
            p.send(200, &Writer::new()).await;
            p.send(msg::PING_RESP, &Writer::new()).await;
            assert!(p.sync().await.is_empty());
            assert_eq!(
                event(&mut rx).await,
                ApiEvent::Time {
                    epoch: 1_700_000_000,
                    posix_tz: "<+0300>-3".into()
                }
            );
            assert_eq!(
                event(&mut rx).await,
                ApiEvent::Time {
                    epoch: 1_700_000_001,
                    posix_tz: "MSK-3".into()
                }
            );
            assert!(rx.try_recv().is_err());

            // A second hello on the same connection is answered but connects nobody new.
            let mut w = Writer::new();
            w.str(1, "again", false);
            p.send(msg::HELLO_REQ, &w).await;
            p.expect(msg::HELLO_RESP).await;
            assert!(p.sync().await.is_empty());
            assert!(rx.try_recv().is_err());

            p.send(msg::DISCONNECT_REQ, &Writer::new()).await;
            p.expect(msg::DISCONNECT_RESP).await;
            assert!(p.closed().await.is_empty());
            assert_eq!(event(&mut rx).await, ApiEvent::ClientDisconnected);
            assert!(!h.connected());

            // The server's own DisconnectRequest is acknowledged by the client.
            let mut p = Peer::connect(port).await;
            p.hello("probe").await;
            event(&mut rx).await;
            p.send(msg::DISCONNECT_RESP, &Writer::new()).await;
            assert!(p.closed().await.is_empty());
            assert_eq!(event(&mut rx).await, ApiEvent::ClientDisconnected);
        })
        .await;
}

#[tokio::test]
async fn protocol_violations_drop_the_client() {
    local()
        .run_until(async {
            let (h, mut rx, port) = start(None);
            // Ping and disconnect are allowed before hello, nothing else.
            let mut p = Peer::connect(port).await;
            assert!(p.sync().await.is_empty());
            p.subscribe_voice(true).await;
            assert!(p.closed().await.is_empty());

            for garbage in [&b"\x05hello"[..], b"\x00\x81\x80\x80\x01\x01"] {
                let mut p = Peer::connect(port).await;
                p.write(garbage).await;
                assert!(p.closed().await.is_empty(), "{garbage:?}");
            }
            // A header that never ends.
            let mut p = Peer::connect(port).await;
            p.write(&[0x00; 1]).await;
            p.write(&[0xff; 21]).await;
            assert!(p.closed().await.is_empty());
            // EOF in the middle of a frame.
            let mut p = Peer::connect(port).await;
            p.write(b"\x00\x05\x01ab").await;
            p.s.shutdown().await.unwrap();
            assert!(p.closed().await.is_empty());
            // A malformed payload is logged, the message still handled.
            let mut p = Peer::connect(port).await;
            p.write(b"\x00\x03\x01\x0a\x05x").await;
            p.expect(msg::HELLO_RESP).await;
            assert!(matches!(
                event(&mut rx).await,
                ApiEvent::ClientConnected { .. }
            ));
            drop(h);
        })
        .await;
}

#[tokio::test]
async fn pending_connections_are_capped() {
    local()
        .run_until(async {
            let (_h, _rx, port) = start(None);
            let mut pending = Vec::new();
            for _ in 0..MAX_PENDING {
                pending.push(Peer::connect(port).await);
            }
            // The server has seen all of them before the next one comes.
            let mut probe = pending.pop().unwrap();
            assert!(probe.sync().await.is_empty());
            let mut newest = Peer::connect(port).await;
            assert!(newest.sync().await.is_empty());
            // The oldest went; the others stay.
            assert!(pending[0].closed().await.is_empty());
            for p in &mut pending[1..] {
                assert!(p.sync().await.is_empty());
            }
            assert!(probe.sync().await.is_empty());
        })
        .await;
}

#[tokio::test]
async fn voice_owner_hands_over() {
    local()
        .run_until(async {
            let (h, mut rx, port) = start(None);
            let mut a = Peer::connect(port).await;
            a.hello("HA").await;
            event(&mut rx).await;
            assert_eq!(h.voice_request_start("Алиса"), 1);
            a.subscribe_voice(true).await;
            a.sync().await;
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(true));
            assert!(h.voice_subscribed());

            // A newer subscriber takes it over: false before true.
            let mut b = Peer::connect(port).await;
            b.hello("probe").await;
            event(&mut rx).await;
            b.subscribe_voice(true).await;
            b.sync().await;
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(false));
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(true));
            assert!(h.voice_subscribed());

            assert_eq!(h.voice_request_start("Алиса"), 2);
            let req = b.expect(msg::VA_REQUEST).await;
            assert!(field(&req, 1).unwrap().boolean());
            assert_eq!(field(&req, 5).unwrap().str(), "Алиса");
            let audio = field(&req, 4).unwrap();
            assert_eq!(fields(audio.data).len(), 1);
            assert_eq!(field(audio.data, 3).unwrap().flt(), 1.0);
            h.voice_send_audio(vec![1, -2]);
            let m = b.expect(msg::VA_AUDIO).await;
            assert_eq!(field(&m, 1).unwrap().data, [1, 0, 0xfe, 0xff]);
            h.voice_announce_finished(true);
            let m = b.expect(msg::VA_ANNOUNCE_FINISHED).await;
            assert!(field(&m, 1).unwrap().boolean());
            h.voice_request_stop();
            assert!(b.expect(msg::VA_REQUEST).await.is_empty());
            // The old owner got nothing.
            assert!(a.sync().await.is_empty());

            // HA answers for the run it was asked to start; answers from others are ignored.
            let mut w = Writer::new();
            w.boolean(2, true, false);
            a.send(msg::VA_RESPONSE, &w).await;
            a.sync().await;
            b.send(msg::VA_RESPONSE, &w).await;
            b.sync().await;
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceStartFailed { run: 2 });
            assert!(rx.try_recv().is_err());

            // The newer one leaves: back to the previous subscriber.
            drop(b);
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(false));
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(true));
            h.voice_request_start("");
            let req = a.expect(msg::VA_REQUEST).await;
            assert!(field(&req, 5).is_none());

            // Unsubscribing gives it up for good.
            a.subscribe_voice(false).await;
            a.sync().await;
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(false));
            assert!(!h.voice_subscribed() && h.connected());
            assert!(rx.try_recv().is_err());
            // Without an owner the voice calls send nothing.
            h.voice_request_start("x");
            h.voice_send_audio(vec![1]);
            h.voice_request_stop();
            assert!(a.sync().await.is_empty());

            // An owner that disconnects: false, then the last client is gone.
            a.subscribe_voice(true).await;
            a.sync().await;
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(true));
            drop(a);
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(false));
            assert_eq!(event(&mut rx).await, ApiEvent::ClientDisconnected);
            assert!(!h.voice_subscribed() && !h.connected());
        })
        .await;
}

#[tokio::test]
async fn full_server_evicts_the_least_useful_client() {
    local()
        .run_until(async {
            let (_h, mut rx, port) = start(None);
            let mut peers = Vec::new();
            for name in ["owner", "idle", "subscriber", "chatty"] {
                let mut p = Peer::connect(port).await;
                p.hello(name).await;
                event(&mut rx).await;
                peers.push(p);
            }
            peers[0].subscribe_voice(true).await;
            peers[0].sync().await;
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(true));
            // "subscriber" wants the pipeline too but it moves to it, so subscribe "owner"
            // again afterwards to keep it the owner.
            peers[2].subscribe_voice(true).await;
            peers[2].sync().await;
            peers[0].subscribe_voice(true).await;
            peers[0].sync().await;
            for _ in 0..4 {
                event(&mut rx).await;
            }
            peers[3].sync().await;

            // A fifth client: "idle" (no interest in voice, least recently heard) goes.
            let mut e = Peer::connect(port).await;
            e.hello("new").await;
            assert!(matches!(
                event(&mut rx).await,
                ApiEvent::ClientConnected { .. }
            ));
            assert!(peers[1].closed().await.is_empty());
            for i in [0, 2, 3] {
                peers[i].sync().await;
            }

            // Next: the subscriber that does not own the pipeline, never the owner.
            peers[3].subscribe_voice(true).await;
            peers[3].sync().await;
            peers[0].subscribe_voice(true).await;
            peers[0].sync().await;
            for _ in 0..4 {
                event(&mut rx).await;
            }
            e.subscribe_voice(true).await;
            e.sync().await;
            peers[0].subscribe_voice(true).await;
            peers[0].sync().await;
            for _ in 0..4 {
                event(&mut rx).await;
            }
            let mut f = Peer::connect(port).await;
            f.hello("newer").await;
            event(&mut rx).await;
            // "subscriber" (rank 1) was heard from before "chatty" and "new".
            assert!(peers[2].closed().await.is_empty());
            peers[0].sync().await;
            assert!(rx.try_recv().is_err());
        })
        .await;
}

#[tokio::test]
async fn audio_backlog_ends_the_run() {
    local()
        .run_until(async {
            let (h, mut rx, port) = start(None);
            // A tiny receive window keeps the kernel from absorbing the stream.
            let sock = TcpSocket::new_v4().unwrap();
            sock.set_recv_buffer_size(4096).unwrap();
            let s = sock
                .connect(SocketAddr::from(([127, 0, 0, 1], port)))
                .await
                .unwrap();
            let mut p = Peer::with(s);
            p.hello("slow").await;
            event(&mut rx).await;
            p.subscribe_voice(true).await;
            p.sync().await;
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(true));
            let run = h.voice_request_start("Алиса");

            let chunk = vec![7i16; 16384];
            let mut sent = 0;
            let overflow = loop {
                h.voice_send_audio(chunk.clone());
                sent += 1;
                tokio::task::yield_now().await;
                if let Ok(ev) = rx.try_recv() {
                    break ev;
                }
                assert!(sent < 2000, "no overflow");
            };
            assert_eq!(overflow, ApiEvent::VoiceAudioOverflow { run });
            assert!(h.voice_subscribed());
            // Dropped until the next start.
            h.voice_send_audio(chunk.clone());

            let before = p.sync().await;
            let (stop, audio) = before.split_last().unwrap();
            assert_eq!(stop, &(msg::VA_REQUEST, Vec::new()));
            assert_eq!(audio[0].0, msg::VA_REQUEST);
            assert!(audio[1..].iter().all(|m| m.0 == msg::VA_AUDIO));
            assert!(audio.len() - 1 < sent);

            let run2 = h.voice_request_start("");
            assert_eq!(run2, run + 1);
            h.voice_send_audio(vec![1; 4]);
            let after = p.sync().await;
            assert_eq!(after.len(), 2);
            assert_eq!(after[1].0, msg::VA_AUDIO);
            assert!(rx.try_recv().is_err());
        })
        .await;
}

#[tokio::test]
async fn client_not_reading_is_dropped() {
    local()
        .run_until(async {
            let (h, mut rx, port) = start(None);
            let sock = TcpSocket::new_v4().unwrap();
            sock.set_recv_buffer_size(4096).unwrap();
            let s = sock
                .connect(SocketAddr::from(([127, 0, 0, 1], port)))
                .await
                .unwrap();
            let mut p = Peer::with(s);
            p.hello("stuck").await;
            event(&mut rx).await;
            p.subscribe_voice(true).await;
            p.sync().await;
            event(&mut rx).await;
            h.voice_request_start("");
            // One message larger than the 8 MiB backlog limit, while the backlog is still
            // under the audio limit: the client is given up.
            h.voice_send_audio(vec![0; (MAX_BACKLOG / 2) + (1 << 20)]);
            // The actor does the I/O: right after the call the getters are unchanged.
            assert!(h.connected() && h.voice_subscribed());
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(false));
            assert_eq!(event(&mut rx).await, ApiEvent::ClientDisconnected);
            assert!(!h.connected() && !h.voice_subscribed());
            assert!(p.closed().await.len() < MAX_BACKLOG);
        })
        .await;
}

#[tokio::test]
async fn noise_provisioning_and_key_change() {
    let dir = TmpDir::new("noise");
    let key_path = dir.path().join("api_key");
    local()
        .run_until(async {
            let (h, mut rx, port) = start(Some(key_path.clone()));
            assert_eq!(
                h.mdns_txt()[8],
                "api_encryption_supported=Noise_NNpsk0_25519_ChaChaPoly_SHA256"
            );
            // Without a key: plaintext, and Noise with the all-zeros key (provisioning).
            let mut plain = Peer::connect(port).await;
            plain.hello("plain").await;
            event(&mut rx).await;
            assert_eq!(
                Peer::noise(port, &[1; 32]).await.err().unwrap(),
                "Handshake MAC failure"
            );
            let mut p = Peer::noise(port, &[0; 32]).await.unwrap();
            p.hello("HA").await;
            event(&mut rx).await;
            assert!(p.sync().await.is_empty());

            // A rejected key changes nothing.
            let mut w = Writer::new();
            w.str(1, "not base64", false);
            p.send(msg::NOISE_SET_KEY_REQ, &w).await;
            let resp = p.expect(msg::NOISE_SET_KEY_RESP).await;
            assert!(field(&resp, 1).is_none());

            let mut w = Writer::new();
            w.str(1, &format!(" {KEY_B64}\n"), false);
            p.send(msg::NOISE_SET_KEY_REQ, &w).await;
            let resp = p.expect(msg::NOISE_SET_KEY_RESP).await;
            assert!(field(&resp, 1).unwrap().boolean());
            assert_eq!(
                std::fs::read_to_string(&key_path).unwrap(),
                format!("{KEY_B64}\n")
            );
            // 100 ms later: new TXT, then every client is asked to leave.
            let txt = event(&mut rx).await;
            let ApiEvent::MdnsTxtChanged(txt) = txt else {
                panic!("{txt:?}")
            };
            assert_eq!(
                txt[8],
                "api_encryption=Noise_NNpsk0_25519_ChaChaPoly_SHA256"
            );
            assert_eq!(h.mdns_txt(), txt);
            assert!(p.expect(msg::DISCONNECT_REQ).await.is_empty());
            assert!(p.closed().await.is_empty());
            assert!(plain.expect(msg::DISCONNECT_REQ).await.is_empty());
            assert!(plain.closed().await.is_empty());
            assert_eq!(event(&mut rx).await, ApiEvent::ClientDisconnected);

            // Now only the key gets in.
            let mut plain = Peer::connect(port).await;
            let mut w = Writer::new();
            w.str(1, "plain", false);
            plain.send(msg::HELLO_REQ, &w).await;
            assert_eq!(plain.closed().await, b"\x01\x00\x13\x01Bad indicator byte");
            assert_eq!(
                Peer::noise(port, &[0; 32]).await.err().unwrap(),
                "Handshake MAC failure"
            );
            let key = crate::key::decode_psk(KEY_B64.as_bytes()).unwrap();
            let mut p = Peer::noise(port, &key).await.unwrap();
            p.hello("HA").await;
            event(&mut rx).await;

            // The same key again: success, nobody is disconnected.
            let mut w = Writer::new();
            w.str(1, KEY_B64, false);
            p.send(msg::NOISE_SET_KEY_REQ, &w).await;
            assert!(
                field(&p.expect(msg::NOISE_SET_KEY_RESP).await, 1)
                    .unwrap()
                    .boolean()
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(p.sync().await.is_empty());

            // Clearing it: back to plaintext.
            p.send(msg::NOISE_SET_KEY_REQ, &Writer::new()).await;
            assert!(
                field(&p.expect(msg::NOISE_SET_KEY_RESP).await, 1)
                    .unwrap()
                    .boolean()
            );
            assert!(!key_path.exists());
            let ApiEvent::MdnsTxtChanged(txt) = event(&mut rx).await else {
                panic!()
            };
            assert_eq!(
                txt[8],
                "api_encryption_supported=Noise_NNpsk0_25519_ChaChaPoly_SHA256"
            );
            assert!(p.expect(msg::DISCONNECT_REQ).await.is_empty());
            assert!(p.closed().await.is_empty());
            assert_eq!(event(&mut rx).await, ApiEvent::ClientDisconnected);
            let mut plain = Peer::connect(port).await;
            plain.hello("plain").await;
        })
        .await;
}

#[tokio::test]
async fn noise_frame_errors() {
    let dir = TmpDir::new("noise-err");
    let key_path = dir.path().join("api_key");
    std::fs::write(&key_path, format!("{KEY_B64}\n")).unwrap();
    local()
        .run_until(async {
            let (h, _rx, port) = start(Some(key_path.clone()));
            assert_eq!(
                h.mdns_txt()[8],
                "api_encryption=Noise_NNpsk0_25519_ChaChaPoly_SHA256"
            );
            let raw = |payload: &[u8]| {
                let mut v = Vec::new();
                crate::frame::append_noise_raw(&mut v, &[payload]);
                v
            };
            let reject = |reason: &str| raw(&[b"\x01", reason.as_bytes()].concat());
            let hello = raw(format!("\x01{NAME}\0{MAC}\0").as_bytes());
            let cases: [(&[u8], Vec<u8>); 4] = [
                (b"\x01\x00\x81", reject("Bad handshake packet len")),
                (
                    b"\x01\x00\x00\x01\x00\x00",
                    [&hello[..], &reject("Empty handshake message")].concat(),
                ),
                (
                    b"\x01\x00\x00\x01\x00\x02\x05x",
                    [&hello[..], &reject("Bad handshake error byte")].concat(),
                ),
                (
                    b"\x01\x00\x00\x02",
                    [&hello[..], &reject("Bad indicator byte")].concat(),
                ),
            ];
            for (input, want) in cases {
                let mut p = Peer::connect(port).await;
                p.write(input).await;
                assert_eq!(p.closed().await, want, "{input:?}");
            }
            // Garbage after the handshake: decryption fails, the client goes.
            let key = crate::key::decode_psk(KEY_B64.as_bytes()).unwrap();
            let mut p = Peer::noise(port, &key).await.unwrap();
            p.write(b"\x01\x00\x20").await;
            p.write(&[0x55; 32]).await;
            assert!(p.closed().await.is_empty());
        })
        .await;
}

#[tokio::test]
async fn corrupt_key_file_fails_closed() {
    let dir = TmpDir::new("corrupt");
    let key_path = dir.path().join("api_key");
    std::fs::write(&key_path, "garbage\n").unwrap();
    local()
        .run_until(async {
            let (h, _rx, port) = start(Some(key_path.clone()));
            assert_eq!(
                h.mdns_txt()[8],
                "api_encryption=Noise_NNpsk0_25519_ChaChaPoly_SHA256"
            );
            assert_eq!(
                Peer::noise(port, &[0; 32]).await.err().unwrap(),
                "Handshake MAC failure"
            );
            let mut p = Peer::connect(port).await;
            p.write(b"\x00\x00\x01").await;
            assert_eq!(p.closed().await, b"\x01\x00\x13\x01Bad indicator byte");
        })
        .await;
}

#[tokio::test]
async fn shutdown_asks_clients_to_disconnect() {
    let local = local();
    let (mut a, mut b) = local
        .run_until(async {
            let (h, mut rx, port) = start(None);
            let mut a = Peer::connect(port).await;
            a.hello("HA").await;
            event(&mut rx).await;
            let b = Peer::connect(port).await;
            assert!(a.sync().await.is_empty());
            drop(h);
            assert!(pulsar_core::actors::join_all(Duration::from_secs(3)).await);
            // No events from the teardown.
            assert!(rx.recv().await.is_none());
            (a, b)
        })
        .await;
    local
        .run_until(async {
            assert!(a.expect(msg::DISCONNECT_REQ).await.is_empty());
            assert!(a.closed().await.is_empty());
            // A connection before hello is just closed.
            assert!(b.closed().await.is_empty());
        })
        .await;
}

#[tokio::test]
async fn bind_failure_is_an_error() {
    local()
        .run_until(async {
            let taken = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
            let port = taken.local_addr().unwrap().port();
            let cfg = ApiConfig {
                port,
                ..ApiConfig::default()
            };
            let err = spawn(identity(), cfg).err().unwrap();
            assert!(
                err.starts_with(&format!("api: bind/listen :{port}: ")),
                "{err}"
            );
        })
        .await;
}

/// Within a few ticks of `d`. With the clock paused, tokio moves it to the next timer whenever
/// the runtime parks (also between socket round trips), so the client's and the server's
/// clocks of an event differ by up to a few 5 s ticks.
fn about(x: Duration, d: Duration) -> bool {
    let slack = Duration::from_secs(15);
    x + slack > d && x <= d + slack
}

#[tokio::test(start_paused = true)]
async fn hello_timeout_ping_and_keepalive_drop() {
    local()
        .run_until(async {
            let (h, mut rx, port) = start(None);
            let mut silent = Peer::connect(port).await;
            silent.wait = Duration::from_secs(600);
            let t0 = Instant::now();
            assert!(silent.closed().await.is_empty());
            assert!(about(t0.elapsed(), HELLO_TIMEOUT), "{:?}", t0.elapsed());

            let mut p = Peer::connect(port).await;
            p.wait = Duration::from_secs(600);
            p.hello("HA").await;
            event(&mut rx).await;
            let t0 = Instant::now();
            // Idle: the server pings once.
            p.expect(msg::PING_REQ).await;
            assert!(about(t0.elapsed(), IDLE_PING), "{:?}", t0.elapsed());
            // Unanswered: dropped.
            assert!(p.closed().await.is_empty());
            assert!(about(t0.elapsed(), IDLE_DROP), "{:?}", t0.elapsed());
            assert_eq!(event(&mut rx).await, ApiEvent::ClientDisconnected);

            // A client that answers stays, and gets the time again every 6 h.
            let mut p = Peer::connect(port).await;
            p.wait = Duration::from_secs(600);
            p.hello("HA").await;
            event(&mut rx).await;
            let t0 = Instant::now();
            let mut time_requests = 0;
            while t0.elapsed() < TIME_RESYNC + Duration::from_secs(60) {
                match p.recv().await.unwrap().0 {
                    msg::PING_REQ => p.send(msg::PING_RESP, &Writer::new()).await,
                    msg::GET_TIME_REQ => {
                        time_requests += 1;
                        assert!(about(t0.elapsed(), TIME_RESYNC), "{:?}", t0.elapsed());
                    }
                    t => panic!("message {t}"),
                }
            }
            assert_eq!(time_requests, 1);
            assert!(h.connected());
        })
        .await;
}

/// Limitation: without the pause this test hangs rather than fails (the spinning actor keeps
/// the paused clock from advancing); only the CI job timeout then ends it.
#[tokio::test(start_paused = true)]
async fn accept_errors_pause_accepting_without_starving_clients() {
    local()
        .run_until(async {
            let (_h, mut rx, port) = start(None);
            let mut p = Peer::connect(port).await;
            p.wait = Duration::from_secs(600);
            p.hello("HA").await;
            event(&mut rx).await;
            // Out of descriptors: the listener stays ready and every accept fails. Without the
            // pause the actor would spin and the paused clock would never move on.
            FAIL_ACCEPT.set(true);
            for _ in 0..5 {
                assert!(p.sync().await.is_empty());
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            let errors = ACCEPT_ERRORS.get();
            // One attempt per 500 ms pause (plus the ones before the sleep).
            assert!((5..=13).contains(&errors), "{errors}");
            assert!(p.sync().await.is_empty());
            FAIL_ACCEPT.set(false);
            let mut q = Peer::connect(port).await;
            q.wait = Duration::from_secs(600);
            q.hello("back").await;
            assert!(matches!(
                event(&mut rx).await,
                ApiEvent::ClientConnected { .. }
            ));
        })
        .await;
}

#[tokio::test]
async fn hand_over_drops_audio_stop_and_answer_report_of_the_old_run() {
    local()
        .run_until(async {
            let (h, mut rx, port) = start(None);
            let mut a = Peer::connect(port).await;
            a.hello("HA").await;
            event(&mut rx).await;
            a.subscribe_voice(true).await;
            a.sync().await;
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(true));
            h.voice_request_start("Алиса");
            a.expect(msg::VA_REQUEST).await;
            h.voice_send_audio(vec![1]);
            a.expect(msg::VA_AUDIO).await;

            let mut b = Peer::connect(port).await;
            b.hello("HA again").await;
            event(&mut rx).await;
            b.subscribe_voice(true).await;
            b.sync().await;
            // The controller has not seen the hand-over yet: audio, stop and answer report of the
            // old run. An announcement's report still goes to the owner.
            h.voice_send_audio(vec![2]);
            h.voice_request_stop();
            h.voice_answer_finished(true);
            h.voice_announce_finished(false);
            assert!(b.expect(msg::VA_ANNOUNCE_FINISHED).await.is_empty()); // success=false
            assert!(b.sync().await.is_empty());
            assert!(a.sync().await.is_empty());
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(false));
            assert_eq!(event(&mut rx).await, ApiEvent::VoiceSubscribed(true));

            // A new run on the new owner gets everything again.
            h.voice_request_start("");
            b.expect(msg::VA_REQUEST).await;
            h.voice_send_audio(vec![3]);
            b.expect(msg::VA_AUDIO).await;
            h.voice_answer_finished(true);
            let m = b.expect(msg::VA_ANNOUNCE_FINISHED).await;
            assert!(field(&m, 1).unwrap().boolean());
            h.voice_request_stop();
            assert!(b.expect(msg::VA_REQUEST).await.is_empty());
        })
        .await;
}

#[tokio::test]
async fn amplified_replies_still_meet_the_backlog_limit() {
    local()
        .run_until(async {
            let (h, mut rx, port) = start(None);
            let sock = TcpSocket::new_v4().unwrap();
            sock.set_recv_buffer_size(4096).unwrap();
            let s = sock
                .connect(SocketAddr::from(([127, 0, 0, 1], port)))
                .await
                .unwrap();
            let mut p = Peer::with(s);
            p.hello("flood").await;
            event(&mut rx).await;
            // 3-byte HelloRequests, each answered with ~40 bytes, never read: one read batch
            // yields ~1 MiB of replies. Checks the "client not reading" close and the byte
            // bound, not the early flush inside a batch itself.
            let chunk = b"\x00\x00\x01".repeat(20_000);
            for _ in 0..200 {
                if p.s.write_all(&chunk).await.is_err() {
                    break;
                }
                if !rx.is_empty() {
                    break;
                }
            }
            assert_eq!(event(&mut rx).await, ApiEvent::ClientDisconnected);
            assert!(!h.connected());
            assert!(p.closed().await.len() <= MAX_BACKLOG + BATCH_FLUSH);
        })
        .await;
}

mod entities;
