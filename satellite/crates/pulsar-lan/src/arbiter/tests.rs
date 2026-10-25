use std::net::SocketAddr;
use std::time::Duration;

use tokio::task::LocalSet;

use super::*;

const GOLDEN: &str = include_str!("../../tests/golden/arbiter.txt");

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn golden(kind: &str) -> Vec<Vec<&'static str>> {
    GOLDEN
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split(' ').collect::<Vec<_>>())
        .filter(|f| f[0] == kind)
        .collect()
}

fn level(bits: &str) -> f32 {
    f32::from_bits(u32::from_str_radix(bits, 16).unwrap())
}

fn claim(station: &str, wake: &str, busy: bool, level_db: f32) -> Claim {
    Claim {
        station: station.to_owned(),
        wake: wake.to_owned(),
        busy,
        level_db,
    }
}

#[test]
fn datagrams_match_cxx() {
    let lines = golden("format");
    assert_eq!(lines.len(), 9);
    for f in lines {
        let c = claim(f[1], f[2], f[3] == "1", level(f[4]));
        let want = (f[5] != "-").then(|| f[5].to_owned());
        assert_eq!(wire::format(&c).map(|d| hex(d.as_bytes())), want, "{f:?}");
    }
}

/// %.1f on quarter steps (exact ties round to even), every 0.1 dB step and near-ties.
#[test]
fn level_text_matches_cxx() {
    let lines = golden("level");
    assert_eq!(lines.len(), 1412);
    for f in lines {
        let v = level(f[1]);
        assert_eq!(wire::level_text(v), f[2], "{v}");
        let d = wire::format(&claim("s", "w", false, v)).unwrap();
        assert_eq!(d, format!("pulsar-wake/1 s w 0 {}", f[2]));
    }
}

/// Field widths, white space, numbers, range, magic, own station, NUL bytes and size.
#[test]
fn parsing_matches_cxx() {
    let lines = golden("parse");
    assert_eq!(lines.len(), 65);
    for f in lines {
        let d = unhex(f[1]);
        let got = accept(&d[..d.len().min(RX_BUF)], "pulsar-self");
        let want = (f[2] != "-").then(|| claim(f[2], f[3], f[4] == "1", level(f[5])));
        assert_eq!(got, want, "{:?}", String::from_utf8_lossy(&d));
        if let Some(c) = got {
            assert_eq!(c.level_db.to_bits(), level(f[5]).to_bits());
        }
    }
}

#[test]
fn hexadecimal_levels_are_not_taken() {
    for d in ["pulsar-wake/1 a w 0 -0x14", "pulsar-wake/1 a w 0 0X1p-1"] {
        assert_eq!(wire::parse(d.as_bytes()), None, "{d}");
    }
    let d = wire::parse(b"pulsar-wake/1 a w 0 -00x1").unwrap();
    assert_eq!(d.level_db.to_bits(), (-0.0f32).to_bits());
}

/// The arbiter actor driven step by step on paused time; its claims go to a loopback socket.
struct Rig {
    a: Arbiter,
    events: UnboundedReceiver<ArbiterEvent>,
    trace: UnboundedReceiver<Claim>,
    wire: std::net::UdpSocket,
    /// The arbiter's own socket.
    addr: SocketAddrV4,
    start: Instant,
}

fn loopback() -> (std::net::UdpSocket, SocketAddrV4) {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_nonblocking(true).unwrap();
    let SocketAddr::V4(addr) = s.local_addr().unwrap() else {
        unreachable!()
    };
    (s, addr)
}

impl Rig {
    fn new(station: &str) -> Self {
        let (wire, dest) = loopback();
        let (sock, addr) = loopback();
        let (events_tx, events) = unbounded_channel();
        let (trace_tx, trace) = unbounded_channel();
        let sock = UdpSocket::from_std(sock).unwrap();
        let a = Arbiter::new(station.to_owned(), sock, dest, events_tx, Some(trace_tx));
        Self {
            a,
            events,
            trace,
            wire,
            addr,
            start: Instant::now(),
        }
    }

    fn claim(&mut self, id: u64, level_db: f32, busy: bool, decide: bool) {
        self.a.on_cmd(Cmd::Claim {
            id,
            wake: "alisa".to_owned(),
            level_db,
            busy,
            decide,
        });
    }

    fn peer(&mut self, station: &str, wake: &str, busy: bool, level_db: f32) {
        let d = wire::format(&claim(station, wake, busy, level_db)).unwrap();
        self.a.on_datagram(Ok(d.len()), d.as_bytes());
    }

    /// Lets `ms` pass, firing the actor's timers on the way.
    async fn run_for(&mut self, ms: u64) {
        let until = Instant::now() + Duration::from_millis(ms);
        loop {
            tokio::select! {
                biased;
                key = self.a.timers.next() => self.a.on_timer(key),
                () = tokio::time::sleep_until(until) => return,
            }
        }
    }

    fn sent(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut b = [0u8; 512];
        while let Ok(n) = self.wire.recv(&mut b) {
            out.push(String::from_utf8_lossy(&b[..n]).into_owned());
        }
        out
    }

    fn decided(&mut self) -> Option<(u64, bool, String, usize)> {
        self.events.try_recv().ok().map(|e| match e {
            ArbiterEvent::Decided {
                claim_id,
                won,
                best,
                peers,
            } => (claim_id, won, best.station, peers),
        })
    }

    fn traced(&mut self) -> Vec<Claim> {
        std::iter::from_fn(|| self.trace.try_recv().ok()).collect()
    }

    fn ms(&self) -> u128 {
        (Instant::now() - self.start).as_millis()
    }
}

#[tokio::test(start_paused = true)]
async fn three_copies_then_the_decision_after_the_window() {
    let mut r = Rig::new("st-me");
    r.claim(7, -20.0, false, true);
    assert_eq!(r.sent(), ["pulsar-wake/1 st-me alisa 0 -20.0"]);
    r.run_for(40).await;
    assert_eq!(r.sent().len(), 1);
    r.run_for(40).await;
    assert_eq!(r.sent().len(), 1);
    r.run_for(169).await;
    assert!(r.sent().is_empty());
    assert_eq!(r.decided(), None);
    r.run_for(1).await;
    assert_eq!(r.ms(), 250);
    assert_eq!(r.decided(), Some((7, true, "st-me".into(), 0)));
    r.run_for(1000).await;
    assert!(r.sent().is_empty());
    assert_eq!(r.decided(), None);
}

#[tokio::test(start_paused = true)]
async fn level_decides_within_the_window() {
    let mut r = Rig::new("st-me");
    r.claim(1, -20.0, false, true);
    r.run_for(10).await;
    r.peer("st-b", "alisa", false, -30.0);
    r.run_for(240).await;
    assert_eq!(r.decided(), Some((1, true, "st-me".into(), 1)));

    // st-b's claim, 240 ms before this one, still counts; the louder st-c wins.
    r.claim(2, -20.0, false, true);
    r.run_for(100).await;
    r.peer("st-c", "alisa", false, -10.0);
    r.run_for(150).await;
    assert_eq!(r.decided(), Some((2, false, "st-c".into(), 2)));
}

#[tokio::test(start_paused = true)]
async fn busy_beats_a_louder_idle_claim() {
    let mut r = Rig::new("st-me");
    r.claim(1, -10.0, false, true);
    r.peer("st-b", "alisa", true, -50.0);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((1, false, "st-b".into(), 1)));

    let mut r = Rig::new("st-me");
    r.claim(1, -50.0, true, true);
    r.peer("st-b", "alisa", false, -10.0);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((1, true, "st-me".into(), 1)));
}

#[tokio::test(start_paused = true)]
async fn equal_claims_go_to_the_larger_station_name() {
    let mut r = Rig::new("st-m");
    r.claim(1, -20.0, false, true);
    r.peer("st-a", "alisa", false, -20.0);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((1, true, "st-m".into(), 1)));
    r.claim(2, -20.0, false, true);
    r.peer("st-n", "alisa", false, -20.0);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((2, false, "st-n".into(), 2)));
}

#[tokio::test(start_paused = true)]
async fn a_claim_older_than_the_window_wins_outright() {
    let mut r = Rig::new("st-me");
    r.peer("st-b", "alisa", false, -30.0);
    r.run_for(251).await;
    r.claim(1, -10.0, true, true);
    r.peer("st-c", "alisa", false, -5.0);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((1, false, "st-b".into(), 2)));

    // Exactly one window before ours: still within it, the louder claim wins.
    let mut r = Rig::new("st-me");
    r.peer("st-b", "alisa", false, -30.0);
    r.run_for(250).await;
    r.claim(1, -10.0, false, true);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((1, true, "st-me".into(), 1)));
}

#[tokio::test(start_paused = true)]
async fn early_claims_are_ranked_among_themselves() {
    let mut r = Rig::new("st-me");
    r.peer("st-b", "alisa", false, -30.0);
    r.peer("st-c", "alisa", true, -40.0);
    r.peer("st-d", "alisa", false, -20.0);
    r.run_for(300).await;
    r.claim(1, -1.0, true, true);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((1, false, "st-c".into(), 3)));
}

#[tokio::test(start_paused = true)]
async fn lookback_and_wake_word_select_the_peers() {
    let mut r = Rig::new("st-me");
    r.peer("st-b", "alisa", false, -30.0);
    r.run_for(500).await;
    r.claim(1, -10.0, false, true);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((1, false, "st-b".into(), 1)));

    let mut r = Rig::new("st-me");
    r.peer("st-b", "alisa", false, -30.0);
    r.run_for(501).await;
    r.claim(1, -10.0, false, true);
    r.peer("st-c", "yandex", true, -1.0);
    r.run_for(250).await;
    assert_eq!(r.decided(), Some((1, true, "st-me".into(), 0)));
}

#[tokio::test(start_paused = true)]
async fn resent_copies_count_once() {
    let mut r = Rig::new("st-me");
    for _ in 0..3 {
        r.peer("st-b", "alisa", false, -30.0);
        r.run_for(40).await;
    }
    assert_eq!(r.traced(), [claim("st-b", "alisa", false, -30.0)]);
    // The same claim 240 ms after the first copy is a copy; 241 ms after, a new claim.
    r.run_for(120).await;
    r.peer("st-b", "alisa", false, -30.0);
    assert!(r.traced().is_empty());
    r.run_for(1).await;
    r.peer("st-b", "alisa", false, -30.0);
    assert_eq!(r.traced().len(), 1);
    // A different claim of the same station replaces the earlier one at once.
    r.peer("st-b", "alisa", true, -30.0);
    assert_eq!(r.traced(), [claim("st-b", "alisa", true, -30.0)]);
    assert_eq!(r.a.recent.len(), 1);
    assert_eq!(r.a.recent[0].at_ms, 241);
}

#[tokio::test(start_paused = true)]
async fn claims_are_kept_2000_ms() {
    let mut r = Rig::new("st-me");
    r.peer("st-b", "alisa", false, -30.0);
    r.run_for(2000).await;
    r.peer("st-c", "alisa", false, -30.0);
    assert_eq!(r.a.recent.len(), 2);
    r.run_for(1).await;
    r.peer("st-d", "alisa", false, -30.0);
    let stations: Vec<_> =
        r.a.recent
            .iter()
            .map(|x| x.claim.station.as_str())
            .collect();
    assert_eq!(stations, ["st-c", "st-d"]);
}

#[tokio::test(start_paused = true)]
async fn claims_queued_on_the_socket_count_before_the_decision() {
    let mut r = Rig::new("st-me");
    r.claim(1, -20.0, false, true);
    r.run_for(200).await;
    // Arrives on the socket; only the timer path sees it (no on_datagram call here).
    let d = wire::format(&claim("st-b", "alisa", false, -10.0)).unwrap();
    r.wire.send_to(d.as_bytes(), r.addr).unwrap();
    r.run_for(50).await;
    assert_eq!(r.decided(), Some((1, false, "st-b".into(), 1)));
}

#[tokio::test(start_paused = true)]
async fn at_most_32_peer_claims_are_kept() {
    let mut r = Rig::new("st-me");
    for i in 0..40 {
        r.peer(&format!("st-{i:02}"), "alisa", false, -30.0);
    }
    let stations: Vec<_> = r.a.recent.iter().map(|x| x.claim.station.clone()).collect();
    assert_eq!(stations.len(), 32);
    assert_eq!(
        (stations[0].as_str(), stations[31].as_str()),
        ("st-08", "st-39")
    );
}

#[tokio::test(start_paused = true)]
async fn own_claims_looping_back_are_ignored() {
    let mut r = Rig::new("st-me");
    r.claim(1, -50.0, false, true);
    r.peer("st-me", "alisa", true, -1.0);
    r.run_for(250).await;
    assert!(r.traced().is_empty());
    assert_eq!(r.decided(), Some((1, true, "st-me".into(), 0)));
}

#[tokio::test(start_paused = true)]
async fn cancel_drops_the_decision_and_the_remaining_copies() {
    let mut r = Rig::new("st-me");
    r.claim(1, -20.0, false, true);
    r.run_for(40).await;
    r.a.on_cmd(Cmd::Cancel);
    r.run_for(1000).await;
    assert_eq!(r.sent().len(), 2);
    assert_eq!(r.decided(), None);
}

#[tokio::test(start_paused = true)]
async fn claim_without_decision_only_broadcasts() {
    let mut r = Rig::new("st-me");
    r.claim(1, -20.0, true, false);
    r.run_for(1000).await;
    assert_eq!(r.sent(), ["pulsar-wake/1 st-me alisa 1 -20.0"; 3]);
    assert_eq!(r.decided(), None);
}

#[tokio::test(start_paused = true)]
async fn a_new_claim_replaces_the_pending_one() {
    let mut r = Rig::new("st-me");
    r.claim(1, -20.0, false, true);
    r.run_for(100).await;
    r.claim(2, -25.0, false, true);
    r.run_for(249).await;
    assert_eq!(r.decided(), None);
    r.run_for(1).await;
    assert_eq!(r.decided(), Some((2, true, "st-me".into(), 0)));
    assert_eq!(r.ms(), 350);
    let sent = r.sent();
    assert_eq!(sent.len(), 6);
    assert!(sent[3..].iter().all(|d| d.ends_with(" -25.0")));
}

#[tokio::test(start_paused = true)]
async fn too_long_a_claim_is_not_sent() {
    let mut r = Rig::new(&"x".repeat(100));
    r.claim(1, -20.0, false, true);
    r.run_for(250).await;
    assert!(r.sent().is_empty());
    assert_eq!(r.decided().map(|d| d.1), Some(true));
}

#[tokio::test(start_paused = true)]
async fn handle_numbers_claims_and_reports_decisions() {
    LocalSet::new()
        .run_until(async {
            let (wire, dest) = loopback();
            let (sock, _) = loopback();
            let sock = UdpSocket::from_std(sock).unwrap();
            let (h, mut events) = launch("st-me".to_owned(), sock, dest, None);
            assert!(h.ok());
            let start = Instant::now();
            assert_eq!(h.clone().claim("alisa", -20.0, false, true), 1);
            let ArbiterEvent::Decided {
                claim_id,
                won,
                best,
                peers,
            } = events.recv().await.unwrap();
            assert_eq!(Instant::now() - start, Duration::from_millis(250));
            assert_eq!(
                (claim_id, won, best, peers),
                (1, true, claim("st-me", "alisa", false, -20.0), 0)
            );
            assert_eq!(h.claim("alisa", -20.0, true, false), 2);
            let quiet = tokio::time::timeout(Duration::from_secs(1), events.recv()).await;
            assert!(quiet.is_err());
            assert_eq!(h.claim("yandex", -20.0, false, true), 3);
            h.cancel();
            let quiet = tokio::time::timeout(Duration::from_secs(1), events.recv()).await;
            assert!(quiet.is_err());
            let mut b = [0u8; 128];
            let copies = std::iter::from_fn(|| wire.recv(&mut b).ok()).count();
            assert_eq!(copies, 3 + 3 + 1);
            drop(h);
            assert!(actors::join_all(Duration::from_secs(1)).await);
        })
        .await;
}

#[tokio::test]
async fn without_a_socket_every_claim_wins_alone() {
    LocalSet::new()
        .run_until(async {
            // Held without SO_REUSEADDR: the arbiter cannot bind the port.
            let holder = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            let port = holder.local_addr().unwrap().port();
            let (h, mut events) = spawn("st-me".to_owned(), port);
            assert!(!h.ok());
            assert_eq!(h.claim("alisa", -20.0, true, true), 1);
            assert_eq!(
                events.try_recv().unwrap(),
                ArbiterEvent::Decided {
                    claim_id: 1,
                    won: true,
                    best: claim("st-me", "alisa", true, -20.0),
                    peers: 0,
                }
            );
            assert_eq!(h.claim("alisa", -20.0, true, false), 2);
            h.cancel();
            assert!(events.try_recv().is_err());
        })
        .await;
}
