//! The API server actor: TCP transport (plaintext or Noise, chosen by the client's first
//! byte), connection limits and keepalive, the voice pipeline owner among several clients, the
//! log stream, the encryption key; entity messages are in `server/entities.rs` and
//! `server/commands.rs`.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::future::poll_fn;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use log::{Level, error, info, warn};
use pulsar_core::{Timers, actors};
use socket2::{Domain, SockRef, Socket, TcpKeepalive, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;

use crate::frame::{self, EncryptError, HandshakeError, NOISE_PROTOCOL, Noise, NoiseFrame, Plain};
use crate::key::{self, Key, SetKey, os_error_text};
use crate::proto::{Reader, WireType, Writer, zigzag32};
use crate::types::{ApiConfig, ApiEvent, DeviceIdentity};

/// API version announced in HelloResponse. 1.15 = DeviceCapabilitiesRequest/Response supported.
const API_MAJOR: u32 = 1;
const API_MINOR: u32 = 15;
pub(crate) const ESPHOME_VERSION: &str = "2026.9.0";
pub(crate) const PROJECT_NAME: &str = "pulsar.satellite";

/// Message ids (api.proto `option (id)`).
mod msg {
    pub const HELLO_REQ: u32 = 1;
    pub const HELLO_RESP: u32 = 2;
    pub const DISCONNECT_REQ: u32 = 5;
    pub const DISCONNECT_RESP: u32 = 6;
    pub const PING_REQ: u32 = 7;
    pub const PING_RESP: u32 = 8;
    pub const DEVICE_INFO_REQ: u32 = 9;
    pub const DEVICE_INFO_RESP: u32 = 10;
    pub const LIST_ENTITIES_REQ: u32 = 11;
    pub const LIST_BINARY_SENSOR: u32 = 12;
    pub const LIST_LIGHT: u32 = 15;
    pub const LIST_SENSOR: u32 = 16;
    pub const LIST_SWITCH: u32 = 17;
    pub const LIST_TEXT_SENSOR: u32 = 18;
    pub const LIST_DONE: u32 = 19;
    pub const SUBSCRIBE_STATES_REQ: u32 = 20;
    pub const BINARY_SENSOR_STATE: u32 = 21;
    pub const LIGHT_STATE: u32 = 24;
    pub const SENSOR_STATE: u32 = 25;
    pub const SWITCH_STATE: u32 = 26;
    pub const TEXT_SENSOR_STATE: u32 = 27;
    pub const SUBSCRIBE_LOGS_REQ: u32 = 28;
    pub const SUBSCRIBE_LOGS_RESP: u32 = 29;
    pub const LIGHT_CMD: u32 = 32;
    pub const SWITCH_CMD: u32 = 33;
    pub const SUBSCRIBE_HA_SERVICES_REQ: u32 = 34;
    pub const GET_TIME_REQ: u32 = 36;
    pub const GET_TIME_RESP: u32 = 37;
    pub const SUBSCRIBE_HA_STATES_REQ: u32 = 38;
    pub const LIST_SERVICES: u32 = 41;
    pub const EXECUTE_SERVICE_REQ: u32 = 42;
    pub const LIST_NUMBER: u32 = 49;
    pub const NUMBER_STATE: u32 = 50;
    pub const NUMBER_CMD: u32 = 51;
    pub const LIST_SELECT: u32 = 52;
    pub const SELECT_STATE: u32 = 53;
    pub const SELECT_CMD: u32 = 54;
    pub const LIST_BUTTON: u32 = 61;
    pub const BUTTON_CMD: u32 = 62;
    pub const LIST_MEDIA_PLAYER: u32 = 63;
    pub const MEDIA_PLAYER_STATE: u32 = 64;
    pub const MEDIA_PLAYER_CMD: u32 = 65;
    pub const SUBSCRIBE_VA_REQ: u32 = 89;
    pub const VA_REQUEST: u32 = 90;
    pub const VA_RESPONSE: u32 = 91;
    pub const VA_EVENT_RESP: u32 = 92;
    pub const LIST_TEXT: u32 = 97;
    pub const TEXT_STATE: u32 = 98;
    pub const TEXT_CMD: u32 = 99;
    pub const VA_AUDIO: u32 = 106;
    pub const LIST_EVENT: u32 = 107;
    pub const EVENT_RESP: u32 = 108;
    pub const VA_TIMER_EVENT_RESP: u32 = 115;
    pub const LIST_UPDATE: u32 = 116;
    pub const UPDATE_STATE: u32 = 117;
    pub const UPDATE_CMD: u32 = 118;
    pub const VA_ANNOUNCE_REQ: u32 = 119;
    pub const VA_ANNOUNCE_FINISHED: u32 = 120;
    pub const VA_CONFIG_REQ: u32 = 121;
    pub const VA_CONFIG_RESP: u32 = 122;
    pub const VA_SET_CONFIG: u32 = 123;
    pub const NOISE_SET_KEY_REQ: u32 = 124;
    pub const NOISE_SET_KEY_RESP: u32 = 125;
    pub const DEVICE_CAPS_REQ: u32 = 149;
    pub const DEVICE_CAPS_RESP: u32 = 150;
}

mod commands;
mod entities;

pub(crate) use entities::Publish;
use entities::{Cache, DIAG_PERIOD_MS};

/// End the voice run when the socket is this far behind: a hole in the utterance would make HA
/// transcribe garbage.
const AUDIO_BACKLOG_LIMIT: usize = 256 << 10;
/// Give up on a client that stopped reading.
const MAX_BACKLOG: usize = 8 << 20;
#[cfg(test)]
thread_local! {
    /// Largest `rbuf` seen by `read_into` on this thread.
    static MAX_RBUF: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
/// Per wake-up and client, so a flooding peer cannot starve the runtime (capture hand-off,
/// timers) or grow its buffer without bound before it is parsed.
const READ_PER_WAKEUP: usize = 64 << 10;
/// HA plus e.g. the ESPHome dashboard log viewer or aioesphomeapi tools.
const MAX_CLIENTS: usize = 4;
/// Connections that have not sent a valid hello yet.
const MAX_PENDING: usize = 4;
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_PING: Duration = Duration::from_secs(45);
const IDLE_DROP: Duration = Duration::from_secs(100);
const TIME_RESYNC: Duration = Duration::from_secs(6 * 3600);
const TICK_MS: u64 = 5000;
/// The SetKey reply goes out on the old session before the key file is reloaded.
const KEY_RELOAD_MS: u64 = 100;
/// Compact the write buffer once this much of it has been sent and that is most of it.
const WBUF_COMPACT: usize = 64 << 10;
/// Client buffers keep at most this much capacity once they drain after growing past
/// BUF_SHRINK_ABOVE (a burst must not pin megabytes on the Lite 2's 237 MB).
const BUF_KEEP: usize = 64 << 10;
const BUF_SHRINK_ABOVE: usize = 256 << 10;
/// Accepting is paused this long after an accept error (EMFILE, ENFILE, ENOBUFS, ENOMEM):
/// the listener stays ready, so retrying at once would spin.
const ACCEPT_RETRY_MS: u64 = 500;
/// Inside an input batch, replies are flushed early once this much is pending.
const BATCH_FLUSH: usize = 64 << 10;
/// Parsing of one read batch stops once its replies exceed this (e.g. a client asking for the
/// entity list over and over); the rest is parsed on a later wake-up, after other clients.
const BATCH_YIELD: usize = 256 << 10;

/// Accept failed for want of descriptors or memory (the listener stays ready), not because of
/// the one connection being accepted.
fn accept_exhausted(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

#[cfg(test)]
thread_local! {
    /// Test seam: while set, every accept fails with EMFILE and the listener stays ready.
    pub(crate) static FAIL_ACCEPT: Cell<bool> = const { Cell::new(false) };
    /// Accept errors returned by the seam.
    pub(crate) static ACCEPT_ERRORS: Cell<u32> = const { Cell::new(0) };
}

fn poll_accept(
    listener: &TcpListener,
    cx: &mut Context<'_>,
) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
    #[cfg(test)]
    if FAIL_ACCEPT.get() {
        ACCEPT_ERRORS.set(ACCEPT_ERRORS.get() + 1);
        return Poll::Ready(Err(io::Error::from_raw_os_error(libc::EMFILE)));
    }
    listener.poll_accept(cx)
}

/// State the handle reads; the actor updates it before sending the matching event.
pub(crate) struct Shared {
    pub connected: Cell<bool>,
    pub voice_subscribed: Cell<bool>,
    pub mdns_txt: RefCell<Vec<String>>,
    /// Last voice run id handed out (written by the handle only).
    pub run_seq: Cell<u64>,
}

pub(crate) enum Cmd {
    VoiceRequestStart { run: u64, phrase: String },
    VoiceRequestStop,
    VoiceSendAudio(Vec<i16>),
    VoiceAnnounceFinished(bool),
    SetActiveWakeWords(Vec<String>),
    Publish(Publish),
}

pub(crate) struct Spawned {
    pub cmds: UnboundedSender<Cmd>,
    pub shared: Rc<Shared>,
    pub events: UnboundedReceiver<ApiEvent>,
    /// The bound port (the configured one, or the kernel's choice for port 0).
    #[cfg_attr(not(test), expect(dead_code, reason = "tests bind port 0"))]
    pub port: u16,
}

/// Binds the port and starts the actor on the current `LocalSet`.
pub(crate) fn spawn(id: DeviceIdentity, mut cfg: ApiConfig) -> Result<Spawned, String> {
    let listener = bind(cfg.port)?;
    let port = listener.local_addr().map_or(cfg.port, |a| a.port());
    info!("api: listening on :{} as '{}'", cfg.port, id.name);
    // An empty path means "no encryption", as in the C++ configuration.
    cfg.key_path = cfg.key_path.filter(|p| !p.as_os_str().is_empty());
    let key = key::load_key(cfg.key_path.as_deref());
    let shared = Rc::new(Shared {
        connected: Cell::new(false),
        voice_subscribed: Cell::new(false),
        mdns_txt: RefCell::new(txt_records(&id, cfg.key_path.is_some(), key.has_key)),
        run_seq: Cell::new(0),
    });
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (ev_tx, ev_rx) = unbounded_channel();
    let mut timers = Timers::new();
    timers.set_repeating(Timer::Tick, TICK_MS, TICK_MS);
    timers.set_repeating(Timer::Diag, DIAG_PERIOD_MS, DIAG_PERIOD_MS);
    let mut server = Server {
        mac_plain: mac_plain(&id.mac),
        id,
        cfg,
        shared: shared.clone(),
        events: ev_tx,
        cmds: cmd_rx,
        listener,
        timers,
        log_notify: Arc::new(Notify::new()),
        clients: Vec::new(),
        next_client: 0,
        rr: 0,
        key,
        voice_seq: 0,
        run: 0,
        audio_overflowed: false,
        tx_scratch: Vec::new(),
        rx_scratch: Vec::new(),
        accept_paused: false,
        accept_failing: false,
        batch: None,
        batch_out: 0,
        voice_stale: false,
        cache: Cache::new(),
    };
    // Diagnostics for the first SubscribeStates; the Diag timer keeps them fresh.
    server.refresh_diagnostics(false);
    actors::spawn_actor("api", server.run());
    Ok(Spawned {
        cmds: cmd_tx,
        shared,
        events: ev_rx,
        port,
    })
}

fn bind(port: u16) -> Result<TcpListener, String> {
    let fail = |m: String| {
        error!("{m}");
        m
    };
    let sock = Socket::new(Domain::IPV4, Type::STREAM.nonblocking(), None)
        .map_err(|e| fail(format!("api: socket: {}", os_error_text(&e))))?;
    let _ = sock.set_reuse_address(true);
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    if let Err(e) = sock.bind(&addr.into()).and_then(|()| sock.listen(4)) {
        return Err(fail(format!(
            "api: bind/listen :{port}: {}",
            os_error_text(&e)
        )));
    }
    TcpListener::from_std(sock.into())
        .map_err(|e| fail(format!("api: socket: {}", os_error_text(&e))))
}

/// "aabbccddeeff", as ESPHome puts it in mDNS TXT and the Noise server hello.
fn mac_plain(mac: &str) -> String {
    mac.chars()
        .filter(|&c| c != ':')
        .map(|c| {
            if ('A'..='F').contains(&c) {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// TXT records of `_esphomelib._tcp`.
pub(crate) fn txt_records(id: &DeviceIdentity, key_supported: bool, has_key: bool) -> Vec<String> {
    let mut txt = vec![
        format!("friendly_name={}", id.friendly_name),
        format!("version={ESPHOME_VERSION}"),
        format!("mac={}", mac_plain(&id.mac)),
        "platform=host".to_owned(),
        format!("board={}", id.board),
        "network=wifi".to_owned(),
        format!("project_name={PROJECT_NAME}"),
        format!("project_version={}", id.project_version),
    ];
    // HA reads these to ask for a key / offer to provision one before connecting.
    if key_supported {
        let field = if has_key {
            "api_encryption"
        } else {
            "api_encryption_supported"
        };
        txt.push(format!("{field}={NOISE_PROTOCOL}"));
    }
    txt
}

/// POSIX TZ string for a ParsedTimezone (GetTimeResponse field 3) without DST rules, e.g.
/// "<+0300>-3". std_offset_seconds uses the POSIX sign convention (seconds west of UTC: MSK-3 =
/// -10800). "" when the zone has DST (not representable here) or the message is malformed.
fn posix_tz_from_parsed(p: &[u8]) -> String {
    let mut std_off: i32 = 0;
    let mut has_dst = false;
    let mut r = Reader::new(p);
    while let Some(f) = r.next() {
        if f.num == 1 {
            std_off = zigzag32(f.v);
        }
        if f.num == 3 || f.num == 4 {
            let mut rr = Reader::new(f.data);
            while let Some(rf) = rr.next() {
                // DSTRule.type != NONE
                if rf.num == 3 && rf.v != 0 {
                    has_dst = true;
                }
            }
        }
    }
    if has_dst || r.error() {
        return String::new();
    }
    let east = std_off.wrapping_neg();
    let a = east.unsigned_abs();
    let sign = if east < 0 { '-' } else { '+' };
    let mut tz = format!("<{sign}{:02}{:02}>", a / 3600, a / 60 % 60);
    // POSIX offset: west-positive hours[:mm[:ss]].
    let w = std_off.unsigned_abs();
    let west = if std_off < 0 { "-" } else { "" };
    let _ = if !w.is_multiple_of(60) {
        write!(tz, "{west}{}:{:02}:{:02}", w / 3600, w / 60 % 60, w % 60)
    } else if !w.is_multiple_of(3600) {
        write!(tz, "{west}{}:{:02}", w / 3600, w / 60 % 60)
    } else {
        write!(tz, "{west}{}", w / 3600)
    };
    tz
}

/// Our levels <-> ESPHome LogLevel (DEBUG 5, INFO 3, WARN 2, ERROR 1).
fn esphome_level(level: Level) -> u32 {
    match level {
        Level::Error => 1,
        Level::Warn => 2,
        Level::Info => 3,
        Level::Debug | Level::Trace => 5,
    }
}

/// Wi-Fi drops leave half-open connections that never see a FIN. Keepalive probes detect an
/// idle dead peer in ~35 s and TCP_USER_TIMEOUT a stalled one (e.g. during mic streaming) in
/// 30 s, well before the API-level ping timeout.
fn configure_socket(stream: &TcpStream) {
    let s = SockRef::from(stream);
    let _ = s.set_tcp_nodelay(true);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(20))
        .with_interval(Duration::from_secs(5))
        .with_retries(3);
    let _ = s.set_tcp_keepalive(&keepalive);
    let _ = s.set_tcp_user_timeout(Some(Duration::from_millis(30_000)));
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Timer {
    Tick,
    KeyReload,
    AcceptResume,
    Diag,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Proto {
    Unknown,
    Plain,
    Noise,
}

/// What the voice owner choice and the eviction rank look at.
#[derive(Clone, Copy, Debug)]
struct Standing {
    id: u64,
    hello: bool,
    voice: bool,
    voice_wanted: u64,
    last_rx: Instant,
}

/// The voice pipeline belongs to the client that subscribed most recently among those still
/// subscribed: HA reconnecting after a Wi-Fi drop takes it from its dead socket at once, and a
/// probe or second HA that subscribes and goes away hands it back to the one before it.
fn voice_choice(clients: impl Iterator<Item = Standing>) -> Option<u64> {
    let mut want: Option<Standing> = None;
    for c in clients {
        if c.hello && c.voice_wanted != 0 && want.is_none_or(|w| c.voice_wanted > w.voice_wanted) {
            want = Some(c);
        }
    }
    want.map(|w| w.id)
}

/// A client completing hello at the cap replaces an established client: preferably one without
/// interest in the voice pipeline, never its owner, otherwise the one heard from least recently
/// (a reconnecting HA whose old socket has not timed out yet gets in, and the old one goes).
/// Returns the candidate and the number of established clients.
fn eviction_victim(clients: impl Iterator<Item = Standing>) -> (Option<u64>, usize) {
    let rank = |c: &Standing| {
        if c.voice {
            2
        } else if c.voice_wanted != 0 {
            1
        } else {
            0
        }
    };
    let mut victim: Option<Standing> = None;
    let mut established = 0;
    for c in clients.filter(|c| c.hello) {
        established += 1;
        if victim
            .is_none_or(|v| rank(&c) < rank(&v) || (rank(&c) == rank(&v) && c.last_rx < v.last_rx))
        {
            victim = Some(c);
        }
    }
    (victim.map(|v| v.id), established)
}

struct Client {
    id: u64,
    stream: TcpStream,
    peer: String,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    woff: usize,
    /// Waiting for the socket to take more of `wbuf`.
    want_out: bool,
    hello: bool,
    /// Subscribed to entity states.
    states: bool,
    /// Input left in `rbuf` by a batch that yielded (BATCH_YIELD).
    pending_input: bool,
    /// EOF arrived while input was still pending; closed once it is parsed.
    peer_closed: bool,
    /// Owns the voice pipeline.
    voice: bool,
    /// Order of its SubscribeVoiceAssistant(true), 0 = not subscribed.
    voice_wanted: u64,
    /// Close once `wbuf` is out; further input is ignored.
    close_after_flush: bool,
    connected_at: Instant,
    last_rx: Instant,
    ping_outstanding: bool,
    last_time_req: Instant,
    /// SubscribeLogsRequest level (ESPHome LogLevel), 0 = none.
    log_level: u32,
    proto: Proto,
    noise: Noise,
}

impl Client {
    fn standing(&self) -> Standing {
        Standing {
            id: self.id,
            hello: self.hello,
            voice: self.voice,
            voice_wanted: self.voice_wanted,
            last_rx: self.last_rx,
        }
    }

    fn backlog(&self) -> usize {
        self.wbuf.len() - self.woff
    }

    /// The write buffer for appending, emptied first when everything in it went out.
    fn out(&mut self) -> &mut Vec<u8> {
        if self.woff != 0 && self.woff == self.wbuf.len() {
            self.wbuf.clear();
            self.woff = 0;
        }
        &mut self.wbuf
    }
}

/// One message in the client's framing. Before the Noise handshake finishes nothing can be
/// sent (dropped).
fn append_frame(c: &mut Client, msg_type: u32, msg: &[u8], scratch: &mut Vec<u8>) {
    c.out();
    if c.proto != Proto::Noise {
        frame::append_plain(&mut c.wbuf, msg_type, msg);
        return;
    }
    match c
        .noise
        .append_encrypted(&mut c.wbuf, msg_type, msg, scratch)
    {
        Ok(()) | Err(EncryptError::NotReady) => {}
        Err(EncryptError::TooLarge) => error!(
            "api: message {msg_type} too large for a Noise frame ({} bytes)",
            msg.len()
        ),
        Err(EncryptError::Failed) => error!("api: noise encrypt failed"),
    }
}

/// Finds a client whose socket is ready, starting after the one served last so that every
/// client gets its turn.
fn poll_clients(
    clients: &[Client],
    rr: &mut usize,
    cx: &mut Context<'_>,
) -> Poll<(u64, bool, bool)> {
    let n = clients.len();
    for k in 0..n {
        let i = (*rr + k) % n;
        let c = &clients[i];
        let writable = c.want_out && c.stream.poll_write_ready(cx).is_ready();
        let readable = c.stream.poll_read_ready(cx).is_ready();
        // Input left over from a batch that yielded counts as readable.
        if readable || writable || c.pending_input {
            *rr = i + 1;
            return Poll::Ready((c.id, readable || c.pending_input, writable));
        }
    }
    Poll::Pending
}

enum Wake {
    Timer(Timer),
    Cmd(Cmd),
    Closed,
    Logs,
    Accept(io::Result<(TcpStream, SocketAddr)>),
    Io {
        id: u64,
        readable: bool,
        writable: bool,
    },
}

struct Server {
    id: DeviceIdentity,
    cfg: ApiConfig,
    mac_plain: String,
    shared: Rc<Shared>,
    events: UnboundedSender<ApiEvent>,
    cmds: UnboundedReceiver<Cmd>,
    listener: TcpListener,
    timers: Timers<Timer>,
    /// Log lines come from any thread; the sink only wakes the actor through this.
    log_notify: Arc<Notify>,
    /// All connections in accept order. A connection counts against MAX_CLIENTS only after a
    /// valid HelloRequest (after the Noise handshake when encrypted), and only then may it
    /// evict an established client, so port scanners and garbage cannot knock HA off.
    clients: Vec<Client>,
    next_client: u64,
    /// Round-robin start of `poll_clients`.
    rr: usize,
    key: Key,
    /// Orders voice subscriptions (`Client::voice_wanted`).
    voice_seq: u64,
    /// Run id of the last voice_request_start: the run the voice events belong to.
    run: u64,
    /// Mic audio stopped for this run (backlog).
    audio_overflowed: bool,
    /// Plaintext of the Noise frame being encrypted.
    tx_scratch: Vec<u8>,
    /// Decrypted Noise message being handled.
    rx_scratch: Vec<u8>,
    /// The accept branch is off until Timer::AcceptResume.
    accept_paused: bool,
    /// An accept error was logged; cleared by the next accepted connection.
    accept_failing: bool,
    /// The client whose input is being processed: replies collect in its buffer and leave in
    /// one flush at the end.
    batch: Option<u64>,
    /// Reply bytes produced by the current batch.
    batch_out: usize,
    /// The voice owner changed since the last voice_request_start: audio and stops still
    /// queued for the previous run are dropped instead of reaching the new owner (in C++ the
    /// controller ended the run inside the hook, before anything else was sent).
    voice_stale: bool,
    /// Entity states as last published.
    cache: Cache,
}

impl Server {
    async fn run(mut self) {
        loop {
            // Timers first (see pulsar_core::Timers); commands before socket work.
            let wake = tokio::select! {
                biased;
                t = self.timers.next() => Wake::Timer(t),
                cmd = self.cmds.recv() => cmd.map_or(Wake::Closed, Wake::Cmd),
                () = self.log_notify.notified() => Wake::Logs,
                r = poll_fn(|cx| poll_accept(&self.listener, cx)), if !self.accept_paused => {
                    Wake::Accept(r)
                }
                (id, readable, writable) = poll_fn(|cx| poll_clients(&self.clients, &mut self.rr, cx)) => {
                    Wake::Io { id, readable, writable }
                }
            };
            match wake {
                Wake::Timer(Timer::Tick) => self.tick(),
                Wake::Timer(Timer::KeyReload) => self.reload_key(),
                Wake::Timer(Timer::AcceptResume) => self.accept_paused = false,
                Wake::Timer(Timer::Diag) => self.refresh_diagnostics(true),
                Wake::Cmd(cmd) => self.command(cmd),
                Wake::Closed => break,
                Wake::Logs => self.drain_logs(),
                Wake::Accept(r) => self.accept_queued(r).await,
                Wake::Io {
                    id,
                    readable,
                    writable,
                } => {
                    if writable {
                        self.flush(id);
                    }
                    if readable && self.client(id).is_some() {
                        // Buffered input from a yielded batch is parsed first; the socket
                        // is read again only once rbuf is drained (TCP backpressure).
                        let pending = self.client(id).is_some_and(|c| c.pending_input);
                        let eof = !pending && self.read_into(id);
                        self.process(id, eof);
                    }
                }
            }
        }
        self.teardown();
    }

    // ------------------------------------------------------------ clients

    fn client(&mut self, id: u64) -> Option<&mut Client> {
        self.clients.iter_mut().find(|c| c.id == id)
    }

    fn ready(&self) -> bool {
        self.clients.iter().any(|c| c.hello)
    }

    fn voice_client(&self) -> Option<u64> {
        self.clients
            .iter()
            .find(|c| c.hello && c.voice)
            .map(|c| c.id)
    }

    /// Runs `f` for every client in accept order. `f` may close the client it is given, and
    /// no other.
    fn for_each_client(&mut self, mut f: impl FnMut(&mut Self, u64)) {
        let mut i = 0;
        while let Some(id) = self.clients.get(i).map(|c| c.id) {
            f(self, id);
            if self.clients.get(i).is_some_and(|c| c.id == id) {
                i += 1;
            }
        }
    }

    /// Updates what the handle's getters read, then sends the event.
    fn emit(&self, ev: ApiEvent) {
        self.shared.connected.set(self.ready());
        self.shared
            .voice_subscribed
            .set(self.voice_client().is_some());
        let _ = self.events.send(ev);
    }

    /// The owner changed: what the controller still sends for the old run is dropped until it
    /// starts a new one.
    fn voice_owner_event(&mut self, owner: bool) {
        self.voice_stale = true;
        self.emit(ApiEvent::VoiceSubscribed(owner));
    }

    /// Takes every queued connection in one wake-up, like the C++ accept4 loop. The accept
    /// branch comes before client I/O in the select, so a client that keeps its socket ready
    /// cannot keep HA from reconnecting; in turn accepting cannot starve the clients: the batch
    /// ends at the listen backlog (4) or tokio's per-task budget, whichever comes first (the
    /// budget makes poll_accept report Pending).
    async fn accept_queued(&mut self, mut r: io::Result<(TcpStream, SocketAddr)>) {
        loop {
            match r {
                Ok((stream, addr)) => {
                    self.accept_failing = false;
                    self.on_accept(stream, addr);
                }
                // A connection that failed on its way in (ECONNABORTED, EPROTO, EPERM, …):
                // log it and skip it, as the C++ accept loop did.
                Err(e) if !accept_exhausted(&e) => {
                    warn!("api: accept: {}", os_error_text(&e));
                }
                Err(e) => {
                    // Out of descriptors / memory: the listener stays ready, so polling it again
                    // at once would spin. Established clients keep being served meanwhile.
                    if !self.accept_failing {
                        warn!("api: accept: {}", os_error_text(&e));
                    }
                    self.accept_failing = true;
                    self.accept_paused = true;
                    self.timers.set(Timer::AcceptResume, ACCEPT_RETRY_MS);
                    return;
                }
            }
            match poll_fn(|cx| Poll::Ready(poll_accept(&self.listener, cx))).await {
                Poll::Ready(next) => r = next,
                Poll::Pending => return,
            }
        }
    }

    fn on_accept(&mut self, stream: TcpStream, addr: SocketAddr) {
        if self.clients.iter().filter(|c| !c.hello).count() >= MAX_PENDING
            && let Some(oldest) = self.clients.iter().find(|c| !c.hello).map(|c| c.id)
        {
            self.close_client(oldest, "superseded by newer connections");
        }
        configure_socket(&stream);
        let now = Instant::now();
        let peer = addr.to_string();
        info!("api: connection from {peer}");
        self.next_client += 1;
        self.clients.push(Client {
            id: self.next_client,
            stream,
            peer,
            rbuf: Vec::new(),
            wbuf: Vec::new(),
            woff: 0,
            want_out: false,
            hello: false,
            states: false,
            pending_input: false,
            peer_closed: false,
            voice: false,
            voice_wanted: 0,
            close_after_flush: false,
            connected_at: now,
            last_rx: now,
            ping_outstanding: false,
            last_time_req: now,
            log_level: 0,
            proto: Proto::Unknown,
            noise: Noise::ClientHello,
        });
    }

    fn close_client(&mut self, id: u64, why: &str) {
        let Some(i) = self.clients.iter().position(|c| c.id == id) else {
            return;
        };
        let c = self.clients.remove(i);
        let Client {
            stream,
            peer,
            hello,
            voice,
            log_level,
            ..
        } = c;
        drop(stream);
        let state = if hello { "disconnected" } else { "dropped" };
        info!("api: {peer} {state} ({why})");
        if log_level != 0 {
            self.update_log_sink();
        }
        if !hello {
            return;
        }
        if voice {
            self.voice_owner_event(false);
        }
        self.update_voice_owner();
        if !self.ready() {
            self.emit(ApiEvent::ClientDisconnected);
        }
    }

    /// Hands the pipeline to `voice_choice`'s client; events see false before true on every
    /// hand-over.
    fn update_voice_owner(&mut self) {
        let want = voice_choice(self.clients.iter().map(Client::standing));
        let cur = self.voice_client();
        if cur == want {
            return;
        }
        if let Some(cur) = cur {
            let want_peer = want.and_then(|w| self.client(w)).map(|c| c.peer.clone());
            let Some(c) = self.client(cur) else { return };
            c.voice = false;
            if let Some(want_peer) = want_peer {
                info!("api: voice assistant moves from {} to {want_peer}", c.peer);
            }
            self.voice_owner_event(false);
        }
        // Unlike the C++ hook, the event cannot close clients in between.
        if let Some(c) = want.and_then(|w| self.client(w)) {
            if cur.is_none() {
                info!("api: voice assistant served by {}", c.peer);
            }
            c.voice = true;
            self.voice_owner_event(true);
        }
    }

    fn make_room(&mut self) {
        let (victim, established) = eviction_victim(self.clients.iter().map(Client::standing));
        if established >= MAX_CLIENTS
            && let Some(victim) = victim
        {
            self.close_client(victim, "replaced by a new connection");
        }
    }

    // ------------------------------------------------------------ input

    /// Reads up to READ_PER_WAKEUP bytes into the client's buffer; true on EOF / error. The
    /// socket stays ready when more is pending, so the rest is read on the next wake-up.
    fn read_into(&mut self, id: u64) -> bool {
        let Some(c) = self.client(id) else {
            return true;
        };
        let mut buf = [0u8; 16384];
        let mut got = 0;
        let mut eof = false;
        while got < READ_PER_WAKEUP {
            match c.stream.try_read(&mut buf) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    c.rbuf.extend_from_slice(&buf[..n]);
                    #[cfg(test)]
                    MAX_RBUF.with(|m| m.set(m.get().max(c.rbuf.len())));
                    got += n;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("api: recv: {}", os_error_text(&e));
                    eof = true;
                    break;
                }
            }
        }
        if got > 0 {
            c.last_rx = Instant::now();
            c.ping_outstanding = false;
        }
        eof
    }

    /// Handles what `read_into` collected. Replies collect in the client's buffer and leave in
    /// one flush at the end, or earlier once BATCH_FLUSH bytes are pending (so the backlog
    /// limits in flush still bound memory); parsing stops when the client is closed.
    fn process(&mut self, id: u64, eof: bool) {
        self.batch = Some(id);
        self.batch_out = 0;
        self.parse_input(id);
        self.batch = None;
        self.flush(id);
        let Some(c) = self.client(id) else { return };
        c.peer_closed |= eof;
        if c.peer_closed && !c.pending_input {
            self.close_client(id, "connection closed by peer");
        }
    }

    fn parse_input(&mut self, id: u64) {
        let has_key = self.key.has_key;
        let Some(c) = self.client(id) else { return };
        let buf = std::mem::take(&mut c.rbuf);
        let mut pos = 0;
        let mut yielded = false;
        while pos < buf.len() {
            let Some(c) = self.client(id) else { return };
            if c.close_after_flush {
                break;
            }
            if c.proto == Proto::Unknown {
                match buf[pos] {
                    0x01 => c.proto = Proto::Noise,
                    0x00 if !has_key => c.proto = Proto::Plain,
                    0x00 => {
                        // aioesphomeapi reports "requires encryption".
                        self.noise_reject(id, "Bad indicator byte");
                        break;
                    }
                    _ => {
                        self.close_client(id, "bad frame preamble");
                        return;
                    }
                }
            }
            let used = if c.proto == Proto::Noise {
                self.noise_frame(id, &buf[pos..])
            } else {
                self.plain_frame(id, &buf[pos..])
            };
            if used == 0 {
                // Incomplete, or the client was closed / rejected.
                break;
            }
            pos += used;
            if self.batch_out > BATCH_YIELD {
                yielded = true;
                break;
            }
        }
        let Some(c) = self.client(id) else { return };
        if !c.close_after_flush {
            c.rbuf = buf;
            c.rbuf.drain(..pos);
            if c.rbuf.is_empty() && c.rbuf.capacity() > BUF_SHRINK_ABOVE {
                c.rbuf.shrink_to(BUF_KEEP);
            }
        }
        c.pending_input = yielded && !c.rbuf.is_empty();
    }

    /// Each *_frame returns the bytes consumed, 0 if the frame is incomplete or the client is
    /// gone.
    fn plain_frame(&mut self, id: u64, p: &[u8]) -> usize {
        match frame::parse_plain(p) {
            Plain::Incomplete => 0,
            Plain::BadPreamble => {
                self.close_client(id, "bad frame preamble");
                0
            }
            Plain::MalformedHeader => {
                self.close_client(id, "malformed frame header");
                0
            }
            Plain::TooLarge => {
                self.close_client(id, "frame too large");
                0
            }
            Plain::Frame {
                msg_type,
                msg,
                used,
            } => {
                self.handle(id, msg_type, msg);
                used
            }
        }
    }

    fn noise_frame(&mut self, id: u64, p: &[u8]) -> usize {
        let Some(c) = self.client(id) else { return 0 };
        let in_handshake = c.noise.in_handshake();
        let client_hello = matches!(c.noise, Noise::ClientHello);
        match frame::parse_noise(p, in_handshake) {
            NoiseFrame::Incomplete => 0,
            NoiseFrame::BadIndicator => {
                self.noise_reject(id, "Bad indicator byte");
                0
            }
            NoiseFrame::BadHandshakeLen => {
                self.noise_reject(id, "Bad handshake packet len");
                0
            }
            NoiseFrame::Frame { body, used } => {
                if client_hello {
                    self.noise_start(id, body);
                } else if in_handshake {
                    self.noise_handshake(id, body);
                } else {
                    self.noise_data(id, body);
                }
                used
            }
        }
    }

    fn noise_start(&mut self, id: u64, body: &[u8]) {
        let Some(i) = self.clients.iter().position(|c| c.id == id) else {
            return;
        };
        let c = &mut self.clients[i];
        c.out();
        if let Err(e) = c.noise.start(
            body,
            &self.key.psk,
            &self.id.name,
            &self.mac_plain,
            &mut c.wbuf,
        ) {
            error!("api: noise setup failed ({e})");
            self.close_client(id, "noise setup failed");
        }
    }

    fn noise_handshake(&mut self, id: u64, body: &[u8]) {
        let has_key = self.key.has_key;
        let reject_all = self.key.reject_all;
        let Some(c) = self.client(id) else { return };
        if reject_all {
            warn!(
                "api: {} noise handshake failed: wrong encryption key",
                c.peer
            );
            self.noise_reject(id, "Handshake MAC failure");
            return;
        }
        c.out();
        match c.noise.handshake(body, &mut c.wbuf) {
            Ok(()) => {
                let key = if has_key {
                    "device key"
                } else {
                    "provisioning key"
                };
                info!("api: {} encrypted session ({key})", c.peer);
            }
            Err(HandshakeError::Reject(reason)) => self.noise_reject(id, reason),
            Err(HandshakeError::ReadFailed { mac }) => {
                let why = if mac {
                    "wrong encryption key"
                } else {
                    "protocol error"
                };
                warn!("api: {} noise handshake failed: {why}", c.peer);
                // The exact "Handshake MAC failure" text makes clients report a wrong key.
                let reason = if mac {
                    "Handshake MAC failure"
                } else {
                    "Handshake error"
                };
                self.noise_reject(id, reason);
            }
            Err(HandshakeError::SplitFailed) => self.close_client(id, "noise split failed"),
        }
    }

    fn noise_data(&mut self, id: u64, body: &[u8]) {
        let mut buf = std::mem::take(&mut self.rx_scratch);
        if let Some(c) = self.client(id) {
            match c.noise.decrypt(body, &mut buf) {
                Ok((msg_type, range)) => self.handle(id, msg_type, &buf[range]),
                Err(why) => self.close_client(id, why),
            }
        }
        self.rx_scratch = buf;
    }

    /// Explicit handshake rejection (status byte 0x01 + reason), then close once flushed
    /// (process() flushes after the input batch).
    fn noise_reject(&mut self, id: u64, reason: &str) {
        let Some(c) = self.client(id) else { return };
        info!("api: {} rejected: {reason}", c.peer);
        frame::append_noise_raw(c.out(), &[b"\x01", reason.as_bytes()]);
        c.close_after_flush = true;
    }

    // ------------------------------------------------------------ output

    fn send_msg(&mut self, id: u64, msg_type: u32, w: &Writer) {
        let Some(i) = self.clients.iter().position(|c| c.id == id) else {
            return;
        };
        append_frame(
            &mut self.clients[i],
            msg_type,
            w.data(),
            &mut self.tx_scratch,
        );
        if self.batch == Some(id) {
            self.batch_out += w.data().len();
        }
        // Replies to input go out together when process() flushes after the batch, unless they
        // pile up: requests that each produce a lot (repeated hellos, entity lists) must still
        // meet the backlog limit of flush within one batch.
        if self.batch != Some(id) || self.clients[i].backlog() >= BATCH_FLUSH {
            self.flush(id);
        }
    }

    fn flush(&mut self, id: u64) {
        let Some(c) = self.client(id) else { return };
        while c.woff < c.wbuf.len() {
            match c.stream.try_write(&c.wbuf[c.woff..]) {
                Ok(0) => {
                    warn!("api: send: 0 bytes");
                    self.close_client(id, "write error");
                    return;
                }
                Ok(n) => c.woff += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if c.backlog() > MAX_BACKLOG {
                        self.close_client(id, "client not reading");
                        return;
                    }
                    if c.woff > WBUF_COMPACT && c.woff * 2 > c.wbuf.len() {
                        c.wbuf.drain(..c.woff);
                        c.woff = 0;
                    }
                    c.want_out = true;
                    return;
                }
                Err(e) => {
                    warn!("api: send: {}", os_error_text(&e));
                    self.close_client(id, "write error");
                    return;
                }
            }
        }
        c.wbuf.clear();
        if c.wbuf.capacity() > BUF_SHRINK_ABOVE {
            c.wbuf.shrink_to(BUF_KEEP);
        }
        c.woff = 0;
        c.want_out = false;
        if c.close_after_flush {
            self.close_client(id, "disconnect requested");
        }
    }

    // ------------------------------------------------------------ timers

    fn tick(&mut self) {
        let now = Instant::now();
        self.for_each_client(|s, id| {
            let Some(c) = s.client(id) else { return };
            if !c.hello {
                if now.saturating_duration_since(c.connected_at) > HELLO_TIMEOUT {
                    s.close_client(id, "no hello");
                }
                return;
            }
            let idle = now.saturating_duration_since(c.last_rx);
            if idle > IDLE_DROP {
                s.close_client(id, "keepalive timeout");
                return;
            }
            if idle > IDLE_PING && !c.ping_outstanding {
                c.ping_outstanding = true;
                s.send_msg(id, msg::PING_REQ, &Writer::new());
            }
            if s.client(id)
                .is_some_and(|c| now.saturating_duration_since(c.last_time_req) >= TIME_RESYNC)
            {
                s.request_time(id);
            }
        });
    }

    fn request_time(&mut self, id: u64) {
        let Some(c) = self.client(id) else { return };
        c.last_time_req = Instant::now();
        self.send_msg(id, msg::GET_TIME_REQ, &Writer::new());
    }

    /// NoiseEncryptionSetKeyRequest: an empty key clears it. Like ESPHome, the reply goes out
    /// on the old session, then every connection is closed so that only clients holding the
    /// new key (or plaintext ones, after a clear) get back in.
    fn set_key(&mut self, b64: &[u8]) -> bool {
        match key::set_key(self.cfg.key_path.as_deref(), b64, &self.key) {
            SetKey::Rejected => false,
            SetKey::Unchanged => true,
            SetKey::Changed => {
                self.timers.set(Timer::KeyReload, KEY_RELOAD_MS);
                true
            }
        }
    }

    fn reload_key(&mut self) {
        self.key = key::load_key(self.cfg.key_path.as_deref());
        let txt = txt_records(&self.id, self.cfg.key_path.is_some(), self.key.has_key);
        self.shared.mdns_txt.replace(txt.clone());
        self.emit(ApiEvent::MdnsTxtChanged(txt));
        warn!("api: encryption key changed, disconnecting all clients");
        self.for_each_client(|s, id| {
            let Some(c) = s.client(id) else { return };
            if c.hello {
                c.close_after_flush = true;
                s.send_msg(id, msg::DISCONNECT_REQ, &Writer::new());
            } else {
                s.close_client(id, "encryption key changed");
            }
        });
    }

    // ------------------------------------------------------------ logs

    fn update_log_sink(&self) {
        let want = self
            .clients
            .iter()
            .filter(|c| c.hello)
            .map(|c| c.log_level)
            .max()
            .unwrap_or(0);
        let min = match want {
            0 => return pulsar_core::set_log_sink(None, None),
            5.. => Level::Debug,
            3 | 4 => Level::Info,
            2 => Level::Warn,
            1 => Level::Error,
        };
        pulsar_core::set_log_sink(Some(min), Some(self.log_notify.clone()));
    }

    fn drain_logs(&mut self) {
        for line in pulsar_core::take_log_lines() {
            let level = esphome_level(line.level);
            let mut w = Writer::new();
            w.u32(1, level, true);
            w.str(3, &line.text, true);
            self.for_each_client(|s, id| {
                if s.client(id)
                    .is_some_and(|c| c.hello && c.log_level >= level)
                {
                    s.send_msg(id, msg::SUBSCRIBE_LOGS_RESP, &w);
                }
            });
        }
    }

    // ------------------------------------------------------------ dispatch

    fn handle(&mut self, id: u64, msg_type: u32, p: &[u8]) {
        let mut r = Reader::new(p);
        let Some(c) = self.client(id) else { return };
        if !c.hello
            && !matches!(
                msg_type,
                msg::HELLO_REQ | msg::DISCONNECT_REQ | msg::PING_REQ
            )
        {
            warn!("api: message {msg_type} before hello");
            self.close_client(id, "protocol violation");
            return;
        }
        match msg_type {
            msg::HELLO_REQ => {
                let mut info = String::new();
                let (mut major, mut minor) = (0, 0);
                while let Some(f) = r.next() {
                    match f.num {
                        1 => info = f.str(),
                        2 => major = f.u32(),
                        3 => minor = f.u32(),
                        _ => {}
                    }
                }
                let first = !c.hello;
                let mut w = Writer::new();
                w.u32(1, API_MAJOR, false);
                w.u32(2, API_MINOR, false);
                w.str(3, &format!("pulsar {}", self.id.project_version), true);
                w.str(4, &self.id.name, true);
                if first {
                    self.make_room();
                }
                self.send_msg(id, msg::HELLO_RESP, &w);
                let Some(c) = self.client(id) else { return };
                info!(
                    "api: hello from '{info}' (api {major}.{minor}) at {}",
                    c.peer
                );
                if first {
                    c.hello = true;
                    self.emit(ApiEvent::ClientConnected { client_info: info });
                    self.request_time(id);
                }
            }
            msg::DISCONNECT_REQ => {
                c.close_after_flush = true;
                self.send_msg(id, msg::DISCONNECT_RESP, &Writer::new());
            }
            msg::DISCONNECT_RESP => self.close_client(id, "disconnect acknowledged"),
            msg::PING_REQ => self.send_msg(id, msg::PING_RESP, &Writer::new()),
            msg::PING_RESP => {}
            msg::SUBSCRIBE_LOGS_REQ => {
                let mut level = 0;
                while let Some(f) = r.next() {
                    if f.num == 1 {
                        level = f.u32();
                    }
                }
                info!("api: {} log subscription, level {level}", c.peer);
                c.log_level = level;
                self.update_log_sink();
            }
            msg::GET_TIME_RESP => {
                let mut epoch = 0;
                let mut tz = String::new();
                let mut parsed = None;
                while let Some(f) = r.next() {
                    if f.num == 1 {
                        epoch = f.u32();
                    }
                    if f.num == 2 {
                        tz = f.str();
                    }
                    if f.num == 3 && f.wt == WireType::Len {
                        parsed = Some(f.data);
                    }
                }
                if tz.is_empty()
                    && let Some(parsed) = parsed
                {
                    tz = posix_tz_from_parsed(parsed);
                }
                info!("api: time from HA: epoch={epoch} tz='{tz}'");
                if epoch != 0 {
                    self.emit(ApiEvent::Time {
                        epoch,
                        posix_tz: tz,
                    });
                }
            }
            msg::SUBSCRIBE_VA_REQ => {
                let mut sub = false;
                let mut flags = 0;
                while let Some(f) = r.next() {
                    if f.num == 1 {
                        sub = f.boolean();
                    }
                    if f.num == 2 {
                        flags = f.u32();
                    }
                }
                let what = if sub { "subscribed" } else { "unsubscribed" };
                info!("api: {} voice assistant {what} (flags={flags})", c.peer);
                let seq = if sub {
                    self.voice_seq += 1;
                    self.voice_seq
                } else {
                    0
                };
                if let Some(c) = self.client(id) {
                    c.voice_wanted = seq;
                }
                self.update_voice_owner();
            }
            msg::VA_RESPONSE => {
                // Answers to our requests only come from the pipeline owner.
                if !c.voice {
                    return;
                }
                let mut port = 0;
                let mut failed = false;
                while let Some(f) = r.next() {
                    if f.num == 1 {
                        port = f.u32();
                    }
                    if f.num == 2 {
                        failed = f.boolean();
                    }
                }
                if failed {
                    warn!("api: HA failed to start the voice pipeline");
                    self.emit(ApiEvent::VoiceStartFailed { run: self.run });
                } else {
                    let via = if port != 0 { "UDP" } else { "API" };
                    info!("api: voice pipeline started ({via} audio)");
                }
            }
            msg::NOISE_SET_KEY_REQ => {
                let mut b64: &[u8] = &[];
                while let Some(f) = r.next() {
                    if f.num == 1 {
                        b64 = f.data;
                    }
                }
                let mut w = Writer::new();
                w.boolean(1, self.set_key(key::trim(b64)), false);
                self.send_msg(id, msg::NOISE_SET_KEY_RESP, &w);
            }
            _ => {
                if !self.handle_entities(id, msg_type, &mut r, p.len()) {
                    // Restart: the C++ server could already be gone here.
                    return;
                }
            }
        }
        if r.error() {
            warn!("api: malformed payload in message {msg_type}");
        }
    }

    // ------------------------------------------------------------ voice (handle commands)

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::VoiceRequestStart { run, phrase } => self.voice_request_start(run, &phrase),
            // Stale: meant for the run of the previous owner.
            Cmd::VoiceRequestStop | Cmd::VoiceSendAudio(_) if self.voice_stale => {}
            Cmd::VoiceRequestStop => self.voice_request_stop(),
            Cmd::VoiceSendAudio(pcm) => self.voice_send_audio(&pcm),
            Cmd::VoiceAnnounceFinished(success) => self.voice_announce_finished(success),
            Cmd::SetActiveWakeWords(ids) => self.cfg.active_wake_words = ids,
            Cmd::Publish(p) => self.publish(p),
        }
    }

    /// `phrase` empty: started by the button or to continue a conversation.
    fn voice_request_start(&mut self, run: u64, phrase: &str) {
        self.run = run;
        self.voice_stale = false;
        let Some(vc) = self.voice_client() else {
            warn!("api: voice_request_start without a subscribed client");
            return;
        };
        let mut audio = Writer::new();
        audio.u32(1, 0, false); // noise_suppression_level
        audio.u32(2, 0, false); // auto_gain
        audio.flt(3, 1.0, false); // volume_multiplier
        let mut w = Writer::new();
        w.boolean(1, true, false); // start
        w.u32(3, 0, false); // flags: wake word handled on the device, VAD in HA
        w.msg(4, &audio);
        w.str(5, phrase, false);
        self.audio_overflowed = false;
        self.send_msg(vc, msg::VA_REQUEST, &w);
    }

    fn voice_request_stop(&mut self) {
        let Some(vc) = self.voice_client() else {
            return;
        };
        let mut w = Writer::new();
        w.boolean(1, false, false);
        self.send_msg(vc, msg::VA_REQUEST, &w);
    }

    /// 16 kHz mono S16LE while the pipeline listens.
    fn voice_send_audio(&mut self, pcm: &[i16]) {
        let Some(vc) = self.voice_client() else {
            return;
        };
        if pcm.is_empty() || self.audio_overflowed {
            return;
        }
        // A hole in the utterance would make HA transcribe garbage: end the run instead.
        if self
            .client(vc)
            .is_some_and(|c| c.backlog() > AUDIO_BACKLOG_LIMIT)
        {
            warn!("api: network backlog, ending the voice pipeline");
            self.audio_overflowed = true;
            self.voice_request_stop();
            self.emit(ApiEvent::VoiceAudioOverflow { run: self.run });
            return;
        }
        let mut w = Writer::new();
        w.pcm16(1, pcm);
        self.send_msg(vc, msg::VA_AUDIO, &w);
    }

    fn voice_announce_finished(&mut self, success: bool) {
        let Some(vc) = self.voice_client() else {
            return;
        };
        let mut w = Writer::new();
        w.boolean(1, success, false);
        self.send_msg(vc, msg::VA_ANNOUNCE_FINISHED, &w);
    }

    // ------------------------------------------------------------ teardown

    /// The handles are gone (daemon exit): no events from here.
    fn teardown(&mut self) {
        pulsar_core::set_log_sink(None, None);
        for mut c in std::mem::take(&mut self.clients) {
            if !c.hello {
                continue;
            }
            // Best effort, the runtime is shutting down: with a DisconnectRequest HA treats
            // the close as expected and reconnects quietly once the daemon is back.
            append_frame(&mut c, msg::DISCONNECT_REQ, &[], &mut self.tx_scratch);
            let Ok(mut s) = c.stream.into_std() else {
                continue;
            };
            let _ = s.write(&c.wbuf[c.woff..]);
            let _ = s.shutdown(Shutdown::Write);
            // Unread input at close() makes the kernel send RST, which may discard what was
            // just sent.
            let mut buf = [0u8; 4096];
            while matches!(s.read(&mut buf), Ok(n) if n > 0) {}
        }
    }
}

#[cfg(test)]
mod tests;
