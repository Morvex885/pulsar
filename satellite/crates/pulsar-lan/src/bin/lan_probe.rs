//! Probe of the LAN services next to a running daemon: use alternative arbitration ports and the
//! `_pulsartest._tcp` service type only (docs/stations.md).
//!
//!   lan_probe mdns <host> <service> <port> <seconds> [txt...]
//!       responder for <host>.local with one service; prints every question it answered
//!   lan_probe claim <station> <port> <wake> <level> [busy]
//!       claims at once, prints the decision
//!   lan_probe claim-on-peer <station> <port> <wake> <level> [timeout_s=60] [delay_ms=0]
//!       claims as soon as another station's claim arrives (same 250 ms window without
//!       synchronising the starts), or delay_ms later (300 = the early-claim rule: the peer's
//!       claim is then older than the window but within the 500 ms lookback); prints the decision
//!   lan_probe listen <port> <seconds>
//!       prints every claim received
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

use pulsar_core::actors;
use pulsar_lan::arbiter::{self, ArbiterEvent, Claim};
use pulsar_lan::mdns::{self, MdnsService, Reply};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Instant, sleep, timeout};

const USAGE: &str = "usage:
  lan_probe mdns <host> <service> <port> <seconds> [txt...]
  lan_probe claim <station> <port> <wake> <level> [busy]
  lan_probe claim-on-peer <station> <port> <wake> <level> [timeout_s=60] [delay_ms=0]
  lan_probe listen <port> <seconds>";

/// A decision comes 250 ms after the claim.
const DECISION_WAIT: Duration = Duration::from_secs(5);
/// Peer claims are still printed this long after the decision.
const PEERS_AFTER: Duration = Duration::from_secs(1);

/// println! that ignores a closed stdout (println! would abort on EPIPE).
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

enum Mode {
    Mdns {
        host: String,
        service: String,
        port: u16,
        seconds: u64,
        txt: Vec<String>,
    },
    Claim {
        station: String,
        port: u16,
        wake: String,
        level: f32,
        busy: bool,
    },
    ClaimOnPeer {
        station: String,
        port: u16,
        wake: String,
        level: f32,
        timeout_s: u64,
        delay_ms: u64,
    },
    Listen {
        port: u16,
        seconds: u64,
    },
}

fn parse_args(a: &[String]) -> Option<Mode> {
    let s = |i: usize| a.get(i).cloned();
    let num = |i: usize| a.get(i)?.parse().ok();
    match (a.first()?.as_str(), a.len()) {
        ("mdns", 5..) => Some(Mode::Mdns {
            host: s(1)?,
            service: s(2)?,
            port: a[3].parse().ok()?,
            seconds: num(4)?,
            txt: a[5..].to_vec(),
        }),
        ("claim", 5..=6) => Some(Mode::Claim {
            station: s(1)?,
            port: a[2].parse().ok()?,
            wake: s(3)?,
            level: a[4].parse().ok()?,
            busy: match a.get(5).map(String::as_str) {
                None | Some("0") => false,
                Some("1" | "busy") => true,
                Some(_) => return None,
            },
        }),
        ("claim-on-peer", 5..=7) => Some(Mode::ClaimOnPeer {
            station: s(1)?,
            port: a[2].parse().ok()?,
            wake: s(3)?,
            level: a[4].parse().ok()?,
            timeout_s: if a.len() > 5 { num(5)? } else { 60 },
            delay_ms: if a.len() > 6 { num(6)? } else { 0 },
        }),
        ("listen", 3) => Some(Mode::Listen {
            port: a[1].parse().ok()?,
            seconds: num(2)?,
        }),
        _ => None,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(mode) = parse_args(&args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    pulsar_core::init_logger();
    pulsar_core::add_own_target(module_path!());
    // As the daemon (ARCHITECTURE "Execution model" 1).
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&rt, run(mode));
    // The responder's goodbye goes out when its actor stops.
    local.block_on(&rt, actors::join_all(Duration::from_secs(3)));
    {
        let _rt = rt.enter();
        drop(local);
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    ExitCode::from(code)
}

async fn run(mode: Mode) -> u8 {
    match mode {
        Mode::Mdns {
            host,
            service,
            port,
            seconds,
            txt,
        } => run_mdns(host, service, port, seconds, txt).await,
        Mode::Claim {
            station,
            port,
            wake,
            level,
            busy,
        } => {
            let (h, mut events, mut claims) = arbiter::spawn_traced(station.clone(), port);
            let id = h.claim(&wake, level, busy, true);
            let claimed = Instant::now();
            say!(
                "claim {id}: {station} wake={wake} busy={} level={level:.1}",
                u8::from(busy)
            );
            let decided = report_decision(&mut events, &mut claims, claimed).await;
            if decided && h.ok() { 0 } else { 1 }
        }
        Mode::ClaimOnPeer {
            station,
            port,
            wake,
            level,
            timeout_s,
            delay_ms,
        } => {
            let (h, mut events, mut claims) = arbiter::spawn_traced(station.clone(), port);
            if !h.ok() {
                return 1;
            }
            say!("waiting {timeout_s} s for a peer claim on udp :{port}");
            let peer = timeout(Duration::from_secs(timeout_s), claims.recv()).await;
            let Ok(Some(peer)) = peer else {
                say!("no peer claim");
                return 1;
            };
            if delay_ms > 0 {
                sleep(Duration::from_millis(delay_ms)).await;
            }
            let id = h.claim(&wake, level, false, true);
            let claimed = Instant::now();
            say!("peer {}", describe(&peer));
            say!("claim {id} after {delay_ms} ms: {station} wake={wake} busy=0 level={level:.1}");
            if report_decision(&mut events, &mut claims, claimed).await {
                0
            } else {
                1
            }
        }
        Mode::Listen { port, seconds } => {
            let (h, _events, mut claims) = arbiter::spawn_traced(String::new(), port);
            if !h.ok() {
                return 1;
            }
            say!("listening on udp :{port} for {seconds} s");
            let start = Instant::now();
            let n = print_for(&mut claims, seconds, |c| {
                say!("+{} ms {}", start.elapsed().as_millis(), describe(&c));
            })
            .await;
            say!("{n} claims");
            0
        }
    }
}

async fn run_mdns(host: String, service: String, port: u16, seconds: u64, txt: Vec<String>) -> u8 {
    let Ok((h, mut answered)) = mdns::spawn_traced(host.clone()) else {
        return 1;
    };
    h.add_service(MdnsService {
        service,
        instance: host,
        port,
        txt,
    });
    match h.ipv4() {
        Some(ip) => say!("mdns: advertising address {ip} for {seconds} s"),
        None => say!("mdns: no usable IPv4 address yet; answering for {seconds} s"),
    }
    let n = print_for(&mut answered, seconds, |a| {
        let reply = match a.reply {
            Reply::Legacy => "legacy unicast",
            Reply::Unicast => "unicast (QU)",
            Reply::Multicast => "multicast",
        };
        let qtype = qtype_name(a.qtype);
        say!("answered {} {qtype} from {}: {reply}", a.name, a.from);
    })
    .await;
    say!("mdns: {n} questions answered, sending goodbye");
    0
}

fn qtype_name(t: u16) -> String {
    match t {
        1 => "A".into(),
        12 => "PTR".into(),
        16 => "TXT".into(),
        33 => "SRV".into(),
        255 => "ANY".into(),
        t => format!("TYPE{t}"),
    }
}

fn describe(c: &Claim) -> String {
    format!(
        "{} wake={} busy={} level={:.1}",
        c.station,
        c.wake,
        u8::from(c.busy),
        c.level_db
    )
}

/// Prints the decision and every peer claim received until 1 s after it, with its time
/// relative to our claim: a peer that missed the window shows up late instead of not at all.
async fn report_decision(
    events: &mut UnboundedReceiver<ArbiterEvent>,
    claims: &mut UnboundedReceiver<Claim>,
    claimed: Instant,
) -> bool {
    let mut decided = false;
    let end = sleep(DECISION_WAIT);
    tokio::pin!(end);
    loop {
        tokio::select! {
            biased;
            Some(c) = claims.recv() => {
                let ms = claimed.elapsed().as_millis();
                say!("peer +{ms} ms after own claim: {}", describe(&c));
            }
            Some(ArbiterEvent::Decided { claim_id, won, best, peers }) = events.recv(),
                if !decided =>
            {
                let ms = claimed.elapsed().as_millis();
                say!(
                    "Decided claim {claim_id} (+{ms} ms): {} best={} peers={peers}",
                    if won { "won" } else { "lost" },
                    describe(&best)
                );
                decided = true;
                end.as_mut().reset(Instant::now() + PEERS_AFTER);
            }
            () = &mut end => break,
        }
    }
    if !decided {
        say!("no decision");
    }
    decided
}

/// Prints every item of `rx` until `seconds` pass or SIGINT / SIGTERM arrives; returns how many.
async fn print_for<T>(
    rx: &mut UnboundedReceiver<T>,
    seconds: u64,
    mut print: impl FnMut(T),
) -> usize {
    let mut term = signal(SignalKind::terminate()).ok();
    let mut int = signal(SignalKind::interrupt()).ok();
    let deadline = sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    let mut n = 0;
    loop {
        tokio::select! {
            () = &mut deadline => break,
            Some(()) = next_signal(&mut term) => break,
            Some(()) = next_signal(&mut int) => break,
            Some(item) = rx.recv() => {
                n += 1;
                print(item);
            }
        }
    }
    n
}

/// Pending forever when the handler could not be installed.
async fn next_signal(s: &mut Option<Signal>) -> Option<()> {
    match s {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}
