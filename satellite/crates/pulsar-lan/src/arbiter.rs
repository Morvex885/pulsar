//! Wake word arbitration between pulsar stations on one LAN: every station that hears the wake
//! word broadcasts a claim with its level (UDP) and waits a short window for its peers' claims;
//! only the best claim answers. Yandex does the same in its cloud (TSpotterValidation /
//! multiactivation in maind), which is unavailable here.

#[cfg(test)]
mod tests;
mod wire;

use std::cell::Cell;
use std::io::{self, Read as _};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::rc::Rc;

use log::{debug, info, warn};
use pulsar_core::{Timers, actors};
use socket2::{Domain, SockRef, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;

use crate::errstr;

/// Spotter confirmations of one utterance on stations a few metres apart land within ~100 ms of
/// each other; the window adds Wi-Fi latency on top. It delays the earcon by as much.
const WINDOW_MS: i64 = 250;
/// A peer may confirm (and claim) before we do.
const LOOKBACK_MS: i64 = 500;
/// Broadcast over Wi-Fi is sent at a basic rate without ACKs or retries.
const RESEND_MS: i64 = 40;
const COPIES: i32 = 3;
const KEEP_MS: i64 = 2000;
/// Copies of one claim arrive within this of each other.
const DEDUP_MS: i64 = RESEND_MS * COPIES as i64 * 2;
/// recv buffer of the C++ arbiter; 255 bytes and more are not a claim.
const RX_BUF: usize = 256;
/// Queued datagrams handled before a timer fires.
const DRAIN_MAX: usize = 64;
/// Peer claims kept at most (one per station; more only from a flood of names).
const MAX_RECENT: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct Claim {
    /// DeviceIdentity::name of the sender.
    pub station: String,
    /// Wake word id: "alisa" / "yandex".
    pub wake: String,
    /// The sender is answering / announcing / ringing a timer: keeps the turn.
    pub busy: bool,
    /// AudioFrontend level, 0.1 dB steps.
    pub level_db: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArbiterEvent {
    /// `won`: this station answers. `best`: the winning claim (ours when won). `peers`: other
    /// stations whose claim for the same wake word counted.
    Decided {
        claim_id: u64,
        won: bool,
        best: Claim,
        peers: usize,
    },
}

enum Cmd {
    Claim {
        id: u64,
        wake: String,
        level_db: f32,
        busy: bool,
        decide: bool,
    },
    Cancel,
}

#[derive(Clone)]
enum Link {
    Actor(UnboundedSender<Cmd>),
    /// No socket: the station answers alone.
    Alone {
        station: Rc<str>,
        events: UnboundedSender<ArbiterEvent>,
    },
}

#[derive(Clone)]
pub struct ArbiterHandle {
    next_id: Rc<Cell<u64>>,
    link: Link,
}

impl ArbiterHandle {
    /// Whether the socket opened in `spawn`; false = every claim wins alone.
    pub fn ok(&self) -> bool {
        matches!(self.link, Link::Actor(_))
    }

    /// Broadcasts a claim (3 copies, 40 ms apart), replacing any pending one. With `decide`,
    /// `Decided` with the returned id follows after the window; without, the claim only keeps
    /// the peers quiet (busy claim while a timer rings). Ids grow from 1.
    pub fn claim(&self, wake: &str, level_db: f32, busy: bool, decide: bool) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        match &self.link {
            Link::Actor(cmds) => {
                let _ = cmds.send(Cmd::Claim {
                    id,
                    wake: wake.to_owned(),
                    level_db,
                    busy,
                    decide,
                });
            }
            Link::Alone { station, events } => {
                if decide {
                    let best = Claim {
                        station: station.to_string(),
                        wake: wake.to_owned(),
                        busy,
                        level_db,
                    };
                    let _ = events.send(ArbiterEvent::Decided {
                        claim_id: id,
                        won: true,
                        best,
                        peers: 0,
                    });
                }
            }
        }
        id
    }

    /// Drops the pending decision and the copies not yet sent. A `Decided` the actor already
    /// queued (and, without a socket, the one `claim` itself queued) may still arrive after
    /// this call: the caller forgets the claim id on cancel and matches events by id.
    pub fn cancel(&self) {
        if let Link::Actor(cmds) = &self.link {
            let _ = cmds.send(Cmd::Cancel);
        }
    }
}

/// Listens for claims on UDP `port` as `station`. If the socket cannot be opened (logged as a
/// warning), `ok()` is false and every deciding claim wins at once with no peers.
pub fn spawn(station: String, port: u16) -> (ArbiterHandle, UnboundedReceiver<ArbiterEvent>) {
    start(station, port, None)
}

/// `spawn` plus a channel of every peer claim accepted (lan_probe).
pub fn spawn_traced(
    station: String,
    port: u16,
) -> (
    ArbiterHandle,
    UnboundedReceiver<ArbiterEvent>,
    UnboundedReceiver<Claim>,
) {
    let (tx, rx) = unbounded_channel();
    let (handle, events) = start(station, port, Some(tx));
    (handle, events, rx)
}

fn start(
    station: String,
    port: u16,
    trace: Option<UnboundedSender<Claim>>,
) -> (ArbiterHandle, UnboundedReceiver<ArbiterEvent>) {
    let sock = match open(port) {
        Ok(sock) => sock,
        Err((what, e)) => {
            warn!(
                "arbiter: {what}: {}; this station answers without arbitration",
                errstr(&e)
            );
            let (events, events_rx) = unbounded_channel();
            let link = Link::Alone {
                station: station.into(),
                events,
            };
            let next_id = Rc::new(Cell::new(0));
            return (ArbiterHandle { next_id, link }, events_rx);
        }
    };
    info!("arbiter: listening on udp :{port} as '{station}'");
    launch(
        station,
        sock,
        SocketAddrV4::new(Ipv4Addr::BROADCAST, port),
        trace,
    )
}

/// Starts the actor on an open socket; claims go to `dest`.
fn launch(
    station: String,
    sock: UdpSocket,
    dest: SocketAddrV4,
    trace: Option<UnboundedSender<Claim>>,
) -> (ArbiterHandle, UnboundedReceiver<ArbiterEvent>) {
    let (events, events_rx) = unbounded_channel();
    let arbiter = Arbiter::new(station, sock, dest, events, trace);
    let (cmds, cmds_rx) = unbounded_channel();
    actors::spawn_actor("arbiter", arbiter.run(cmds_rx));
    let next_id = Rc::new(Cell::new(0));
    let link = Link::Actor(cmds);
    (ArbiterHandle { next_id, link }, events_rx)
}

fn open(port: u16) -> Result<UdpSocket, (String, io::Error)> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM.nonblocking(), None)
        .map_err(|e| ("socket".to_owned(), e))?;
    // SO_REUSEADDR: a second binary on the same station (a probe on another API port) also
    // hears the claims.
    sock.set_reuse_address(true)
        .map_err(|e| ("SO_REUSEADDR".to_owned(), e))?;
    sock.set_broadcast(true)
        .map_err(|e| ("SO_BROADCAST".to_owned(), e))?;
    sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())
        .map_err(|e| (format!("bind :{port}"), e))?;
    UdpSocket::from_std(sock.into()).map_err(|e| ("socket".to_owned(), e))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Timer {
    Resend,
    Decide,
}

struct Received {
    claim: Claim,
    at_ms: i64,
}

struct Arbiter {
    station: String,
    sock: UdpSocket,
    /// 255.255.255.255:<port>
    dest: SocketAddrV4,
    /// One entry per peer station: its latest claim.
    recent: Vec<Received>,
    mine: Claim,
    /// `mine` as sent; None when too long for the C++ arbiter's 128-byte buffer (never sent).
    datagram: Option<String>,
    claim_ms: i64,
    copies_left: i32,
    /// Claim id awaiting its decision.
    pending: Option<u64>,
    timers: Timers<Timer>,
    epoch: Instant,
    events: UnboundedSender<ArbiterEvent>,
    trace: Option<UnboundedSender<Claim>>,
}

impl Arbiter {
    fn new(
        station: String,
        sock: UdpSocket,
        dest: SocketAddrV4,
        events: UnboundedSender<ArbiterEvent>,
        trace: Option<UnboundedSender<Claim>>,
    ) -> Self {
        Self {
            mine: Claim {
                station: station.clone(),
                wake: String::new(),
                busy: false,
                level_db: -100.0,
            },
            station,
            sock,
            dest,
            recent: Vec::new(),
            datagram: None,
            claim_ms: 0,
            copies_left: 0,
            pending: None,
            timers: Timers::new(),
            epoch: Instant::now(),
            events,
            trace,
        }
    }

    async fn run(mut self, mut cmds: UnboundedReceiver<Cmd>) {
        let mut buf = [0u8; RX_BUF];
        loop {
            tokio::select! {
                biased;
                key = self.timers.next() => self.on_timer(key),
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.on_cmd(cmd),
                    None => break,
                },
                r = self.sock.recv(&mut buf) => self.on_datagram(r, &buf),
            }
        }
    }

    fn on_timer(&mut self, key: Timer) {
        // The C++ loop served ready sockets before due timers: a claim already queued must
        // count before our next copy and before the decision.
        self.drain();
        match key {
            Timer::Resend => self.resend(),
            Timer::Decide => self.decide(),
        }
    }

    /// Handles the datagrams already queued on the socket (at most `DRAIN_MAX`). A plain
    /// recv: tokio's readiness may not have seen them yet.
    fn drain(&mut self) {
        let mut buf = [0u8; RX_BUF];
        for _ in 0..DRAIN_MAX {
            let r = (&*SockRef::from(&self.sock)).read(&mut buf);
            match r {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                r => self.on_datagram(r, &buf),
            }
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Claim {
                id,
                wake,
                level_db,
                busy,
                decide,
            } => self.claim(id, wake, level_db, busy, decide),
            Cmd::Cancel => self.cancel(),
        }
    }

    /// Milliseconds on the monotonic clock (tokio's, so tests can pause it).
    fn now_ms(&self) -> i64 {
        (Instant::now() - self.epoch).as_millis() as i64
    }

    fn claim(&mut self, id: u64, wake: String, level_db: f32, busy: bool, decide: bool) {
        self.cancel();
        self.mine = Claim {
            station: self.station.clone(),
            wake,
            busy,
            level_db,
        };
        self.datagram = wire::format(&self.mine);
        self.claim_ms = self.now_ms();
        self.send();
        self.copies_left = COPIES - 1;
        self.timers
            .set_repeating(Timer::Resend, RESEND_MS as u64, RESEND_MS as u64);
        if decide {
            self.pending = Some(id);
            self.timers.set(Timer::Decide, WINDOW_MS as u64);
        }
    }

    fn cancel(&mut self) {
        self.timers.cancel(Timer::Resend);
        self.timers.cancel(Timer::Decide);
        self.pending = None;
    }

    fn resend(&mut self) {
        self.send();
        self.copies_left -= 1;
        if self.copies_left <= 0 {
            self.timers.cancel(Timer::Resend);
        }
    }

    fn send(&self) {
        let Some(d) = &self.datagram else { return };
        // A plain sendto: tokio's try_send_to drops datagrams until the reactor has first seen
        // the socket writable.
        if let Err(e) = SockRef::from(&self.sock).send_to(d.as_bytes(), &self.dest.into()) {
            debug!("arbiter: sendto: {}", errstr(&e));
        }
    }

    fn on_datagram(&mut self, r: io::Result<usize>, buf: &[u8]) {
        let n = match r {
            Ok(n) => n,
            Err(e) => {
                if e.kind() != io::ErrorKind::Interrupted {
                    debug!("arbiter: recv: {}", errstr(&e));
                }
                return;
            }
        };
        let Some(c) = accept(&buf[..n], &self.station) else {
            return;
        };
        let now = self.now_ms();
        let resent = self.recent.iter().any(|r| {
            r.claim.station == c.station
                && r.claim.wake == c.wake
                && r.claim.busy == c.busy
                && r.claim.level_db == c.level_db
                && now - r.at_ms <= DEDUP_MS
        });
        if resent {
            return;
        }
        self.recent
            .retain(|r| now - r.at_ms <= KEEP_MS && r.claim.station != c.station);
        info!(
            "arbiter: claim from {} wake={} busy={} level={}",
            c.station,
            c.wake,
            u8::from(c.busy),
            wire::level_text(c.level_db)
        );
        if let Some(trace) = &self.trace {
            let _ = trace.send(c.clone());
        }
        self.recent.push(Received {
            claim: c,
            at_ms: now,
        });
        // Bounds a flood of distinct station names; the oldest claim goes first.
        if self.recent.len() > MAX_RECENT {
            self.recent.remove(0);
        }
    }

    fn decide(&mut self) {
        let Some(claim_id) = self.pending.take() else {
            return;
        };
        let (won, best, peers) = decision(&self.mine, self.claim_ms, &self.recent);
        let _ = self.events.send(ArbiterEvent::Decided {
            claim_id,
            won,
            best: best.clone(),
            peers,
        });
    }
}

/// A received datagram (cut to the receive buffer) as a peer's claim: a datagram that fills the
/// buffer may have been cut, and our own broadcasts loop back.
fn accept(datagram: &[u8], own_station: &str) -> Option<Claim> {
    if datagram.len() >= RX_BUF - 1 {
        return None;
    }
    wire::parse(datagram).filter(|c| c.station != own_station)
}

/// Busy beats idle, then the higher level, then the larger station name.
fn beats(a: &Claim, b: &Claim) -> bool {
    if a.busy != b.busy {
        return a.busy;
    }
    if a.level_db != b.level_db {
        return a.level_db > b.level_db;
    }
    a.station > b.station
}

/// Decision over the claims of the same wake word received up to the lookback before ours.
fn decision<'a>(
    mine: &'a Claim,
    claim_ms: i64,
    recent: &'a [Received],
) -> (bool, &'a Claim, usize) {
    let mut best = mine;
    let mut won = true;
    let mut peers = 0;
    // A peer that claimed more than a window before us decided before our claim existed: it
    // (or whoever beat it) answers already, so we yield whatever our level.
    let mut early: Option<&Claim> = None;
    for r in recent {
        if r.at_ms < claim_ms - LOOKBACK_MS || r.claim.wake != mine.wake {
            continue;
        }
        peers += 1;
        if r.at_ms < claim_ms - WINDOW_MS {
            if early.is_none_or(|e| beats(&r.claim, e)) {
                early = Some(&r.claim);
            }
        } else if beats(&r.claim, best) {
            best = &r.claim;
            won = false;
        }
    }
    if let Some(e) = early {
        best = e;
        won = false;
    }
    (won, best, peers)
}
