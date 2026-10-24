use std::time::Duration;

use tokio::time::Instant;

use super::dns::{A, Kind, bit, put_name, read_name};
use super::*;

const GOLDEN: &str = include_str!("../../tests/golden/mdns.txt");

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

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// The responder of the golden file (see its header).
fn reference(with_ip: bool) -> Records {
    let mut r = Records::new("pulsar-test");
    r.services.push(Service::new(
        "_esphomelib._tcp",
        "pulsar-test",
        6053,
        strings(&[
            "friendly_name=Pulsar Test",
            "version=2025.5.0",
            "mac=aabbccddeeff",
            "platform=pulsar",
            "board=cucumber",
            "network=wifi",
            "api_encryption=Noise_NNpsk0_25519_ChaChaPoly_SHA256",
        ]),
    ));
    r.services.push(Service::new(
        "_sendspin._tcp",
        "pulsar-test",
        8928,
        strings(&["path=/sendspin", "name=Pulsar Test"]),
    ));
    r.services
        .push(Service::new("_pulsartest._tcp", "Probe", 1, Vec::new()));
    if with_ip {
        r.ip = Ipv4Addr::new(192, 168, 1, 50);
    }
    r
}

/// Long labels and TXT strings, seven services.
fn limits() -> Records {
    let host = "h".repeat(70);
    let mut r = Records::new(&host);
    let long_txt = vec!["a".repeat(300), String::new(), "k=v".to_owned()];
    r.services.push(Service::new("_x._tcp", &host, 7, long_txt));
    r.services.push(Service::new(
        "_s1._tcp",
        &"i".repeat(64),
        11,
        strings(&["one"]),
    ));
    for i in 2..=6u16 {
        let svc = format!("_s{i}._tcp");
        r.services
            .push(Service::new(&svc, "inst", 10 + i, Vec::new()));
    }
    r.ip = Ipv4Addr::new(10, 0, 0, 1);
    r
}

#[test]
fn response_packets_match_cxx() {
    let (r, no_ip, r2) = (reference(true), reference(false), limits());
    let mut question = Vec::new();
    put_name(&mut question, b"_esphomelib._tcp.local");
    question.extend_from_slice(&[0, 12, 0, 1]);
    let builds = golden("build");
    assert_eq!(builds.len(), 9);
    for f in builds {
        let got = match f[1] {
            "announce" => r.build(0, r.all(), 0, None, false, &[]),
            "goodbye" => r.build(0, r.all(), 0, None, true, &[]),
            "ptr_additional" => r.build(
                0,
                bit(1, Kind::Ptr),
                bit(1, Kind::Srv) | bit(1, Kind::Txt) | A,
                None,
                false,
                &[],
            ),
            "overlap" => r.build(
                0,
                bit(0, Kind::Srv) | A,
                A | bit(0, Kind::Txt),
                None,
                false,
                &[],
            ),
            "legacy_capped" => r.build(
                0xbeef,
                bit(0, Kind::Ptr) | bit(0, Kind::Txt) | A,
                bit(0, Kind::Srv),
                Some(10),
                false,
                &question,
            ),
            "enum" => r.build(
                0,
                bit(0, Kind::Enum) | bit(1, Kind::Enum) | bit(2, Kind::Enum),
                0,
                None,
                false,
                &[],
            ),
            "noip_announce" => no_ip.build(0, no_ip.all(), 0, None, false, &[]),
            "noip_goodbye" => no_ip.build(0, no_ip.all(), 0, None, true, &[]),
            "r2_announce" => r2.build(0, r2.all(), 0, None, false, &[]),
            other => panic!("unknown case {other}"),
        };
        assert_eq!(hex(&got), f[2], "{}", f[1]);
    }
}

#[test]
fn announcements_need_an_address_goodbyes_do_not() {
    let builds: Vec<_> = golden("build");
    let want = |case: &str| {
        let f = builds.iter().find(|f| f[1] == case).unwrap();
        Some(f[2].to_owned())
    };
    let (r, no_ip) = (reference(true), reference(false));
    assert_eq!(r.announcement(false).map(|p| hex(&p)), want("announce"));
    assert_eq!(r.announcement(true).map(|p| hex(&p)), want("goodbye"));
    assert_eq!(no_ip.announcement(false), None);
    assert_eq!(
        no_ip.announcement(true).map(|p| hex(&p)),
        want("noip_goodbye")
    );
}

struct QueryCase {
    name: &'static str,
    from: SocketAddrV4,
    packet: Vec<u8>,
    replies: Vec<(String, String)>,
}

fn query_cases() -> Vec<QueryCase> {
    let mut cases: Vec<QueryCase> = Vec::new();
    for l in GOLDEN.lines() {
        let f: Vec<_> = l.split(' ').collect();
        match f[0] {
            "query" => cases.push(QueryCase {
                name: f[1],
                from: f[2].parse().unwrap(),
                packet: unhex(f[3]),
                replies: Vec::new(),
            }),
            "reply" => {
                let case = cases.last_mut().unwrap();
                assert_eq!(case.name, f[1]);
                case.replies.push((f[2].to_owned(), f[3].to_owned()));
            }
            _ => {}
        }
    }
    cases
}

fn destination(dest: &Dest, from: SocketAddrV4) -> String {
    match dest {
        Dest::Sender => from.to_string(),
        Dest::Group => SocketAddrV4::new(GROUP, MDNS_PORT).to_string(),
    }
}

/// Legacy, QU, ANY, case, enumeration, compression (pointer loop, 31 and 32 hops), malformed
/// and unanswerable questions: the same packets to the same destinations as C++.
#[test]
fn query_replies_match_cxx() {
    let cases = query_cases();
    assert_eq!(cases.len(), 32);
    let (r, no_ip) = (reference(true), reference(false));
    for c in &cases {
        let rec = if c.name.starts_with("noip_") {
            &no_ip
        } else {
            &r
        };
        let got: Vec<_> = rec
            .answer(&c.packet, c.from.port(), false)
            .iter()
            .map(|resp| (destination(&resp.dest, c.from), hex(&resp.packet)))
            .collect();
        assert_eq!(got, c.replies, "{}", c.name);
    }
}

#[test]
fn answered_questions_are_reported_with_their_reply() {
    let cases = query_cases();
    let packet = |name: &str| &cases.iter().find(|c| c.name == name).unwrap().packet;
    let r = reference(true);
    let traced = |name: &str, port: u16| -> Vec<(Reply, Vec<(String, u16)>)> {
        r.answer(packet(name), port, true)
            .into_iter()
            .map(|resp| (resp.reply, resp.questions))
            .collect()
    };
    let q = |name: &str, qtype: u16| (name.to_owned(), qtype);
    assert_eq!(
        traced("mixed", 5353),
        vec![
            (Reply::Unicast, vec![q("_esphomelib._tcp.local", 12)]),
            (
                Reply::Multicast,
                vec![
                    q("_sendspin._tcp.local", 12),
                    q("probe._pulsartest._tcp.local", 16)
                ]
            ),
        ]
    );
    assert_eq!(
        traced("legacy_two", 40000),
        vec![
            (
                Reply::Legacy,
                vec![q("pulsar-test._esphomelib._tcp.local", 33)]
            ),
            (Reply::Legacy, vec![q("pulsar-test.local", 1)]),
        ]
    );
    let untraced = r.answer(packet("mixed"), 5353, false);
    assert!(untraced.iter().all(|resp| resp.questions.is_empty()));
}

#[test]
fn short_datagrams_and_responses_are_ignored() {
    let r = reference(true);
    assert!(r.answer(&[0; 11], 5353, false).is_empty());
    assert!(r.answer(&[], 40000, false).is_empty());
}

#[test]
fn names_are_lowercased_and_followed_through_pointers() {
    // Header, then "Ab.Local" at 12 and a pointer to it at 22.
    let mut p = vec![0u8; 12];
    p.extend_from_slice(b"\x02Ab\x05Local\x00\xc0\x0c\x00\x01");
    let mut name = Vec::new();
    let mut pos = 12;
    assert!(read_name(&p, &mut pos, &mut name));
    assert_eq!((name.as_slice(), pos), (&b"ab.local"[..], 22));
    assert!(read_name(&p, &mut pos, &mut name));
    assert_eq!((name.as_slice(), pos), (&b"ab.local"[..], 24));
    // Bytes outside ASCII are kept.
    let p = b"\x03\xc3\xa9X\x00";
    let mut pos = 0;
    assert!(read_name(p, &mut pos, &mut name));
    assert_eq!(name, b"\xc3\xa9x");
}

#[test]
fn names_longer_than_255_bytes_fail() {
    // 3 labels of 63 bytes + one of 61 + root = 255 bytes on the wire; one of 62 = 256.
    let wire = |last: usize| {
        let mut p = Vec::new();
        for _ in 0..3 {
            p.push(63);
            p.extend_from_slice(&[b'a'; 63]);
        }
        p.push(last as u8);
        p.extend(std::iter::repeat_n(b'a', last));
        p.push(0);
        p
    };
    let (mut pos, mut name) = (0, Vec::new());
    assert!(read_name(&wire(61), &mut pos, &mut name));
    assert_eq!((pos, name.len()), (255, 253));
    let mut pos = 0;
    assert!(!read_name(&wire(62), &mut pos, &mut name));
}

#[test]
fn labels_are_cut_at_63_bytes() {
    let mut o = Vec::new();
    put_name(&mut o, format!("{}.b", "a".repeat(70)).as_bytes());
    assert_eq!(o.len(), 1 + 63 + 1 + 1 + 1);
    assert_eq!((o[0], o[64], o[65]), (63, 1, b'b'));
}

fn responder() -> Responder {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    Responder {
        // No address: nothing is announced from a test.
        records: Records::new("pulsar-test"),
        sock: UdpSocket::from_std(sock).unwrap(),
        ip: Rc::new(Cell::new(Ipv4Addr::UNSPECIFIED)),
        ifindex: 0,
        ifname: String::new(),
        join_failed: false,
        link_flags: Vec::new(),
        change_forced: false,
        timers: Timers::new(),
        trace: None,
    }
}

fn service(n: usize) -> MdnsService {
    MdnsService {
        service: format!("_s{n}._tcp"),
        instance: "pulsar-test".to_owned(),
        port: 1000 + n as u16,
        txt: vec![format!("n={n}")],
    }
}

#[tokio::test]
async fn at_most_seven_services() {
    let mut r = responder();
    for n in 0..9 {
        r.apply(Cmd::AddService(service(n)), false);
    }
    let names: Vec<_> = r.records.services.iter().map(|s| &s.type_fqdn).collect();
    assert_eq!(names.len(), MAX_SERVICES);
    assert_eq!(names[6], "_s6._tcp.local");
    assert_eq!(
        r.records.services[6].instance_fqdn,
        "pulsar-test._s6._tcp.local"
    );
}

#[tokio::test]
async fn set_txt_replaces_the_service_txt() {
    let mut r = responder();
    r.apply(Cmd::AddService(service(1)), false);
    r.apply(Cmd::AddService(service(2)), false);
    r.apply(Cmd::SetTxt("_s2._tcp".into(), vec!["x=1".into()]), false);
    r.apply(Cmd::SetTxt("_s9._tcp".into(), vec!["y=1".into()]), false);
    r.apply(
        Cmd::SetTxt("_s1._tcp.local".into(), vec!["z=1".into()]),
        false,
    );
    let txt: Vec<_> = r.records.services.iter().map(|s| s.txt.clone()).collect();
    assert_eq!(txt, vec![vec!["n=1".to_owned()], vec!["x=1".to_owned()]]);
    // Before the start announcement nothing is scheduled; after it, every change re-announces.
    assert!(!r.timers.is_set(Timer::AnnounceAgain1));
    r.apply(Cmd::SetTxt("_s9._tcp".into(), Vec::new()), true);
    assert!(!r.timers.is_set(Timer::AnnounceAgain1));
    r.apply(Cmd::SetTxt("_s1._tcp".into(), Vec::new()), true);
    assert!(r.timers.is_set(Timer::AnnounceAgain1) && r.timers.is_set(Timer::AnnounceAgain2));
    r.timers.cancel(Timer::AnnounceAgain1);
    r.apply(Cmd::AddService(service(3)), true);
    assert!(r.timers.is_set(Timer::AnnounceAgain1));
}

#[tokio::test(start_paused = true)]
async fn announcements_repeat_after_1_and_3_s() {
    let mut r = responder();
    let start = Instant::now();
    r.schedule_announcements();
    tokio::time::advance(Duration::from_millis(500)).await;
    // A change meanwhile restarts the series.
    r.schedule_announcements();
    assert_eq!(r.timers.next().await, Timer::AnnounceAgain1);
    assert_eq!(Instant::now() - start, Duration::from_millis(1500));
    assert_eq!(r.timers.next().await, Timer::AnnounceAgain2);
    assert_eq!(Instant::now() - start, Duration::from_millis(3500));
    assert!(!r.timers.is_set(Timer::AnnounceAgain1) && !r.timers.is_set(Timer::AnnounceAgain2));
}

#[tokio::test(start_paused = true)]
async fn network_changes_settle_once() {
    let mut r = responder();
    let start = Instant::now();
    // First sight of a link that is up: a change that forces an announcement.
    r.on_netlink(
        &[LinkEvent::Link {
            index: 3,
            flags: UP_RUNNING,
        }],
        false,
    );
    assert!(r.timers.is_set(Timer::Settled) && r.change_forced);
    // Further events of the burst do not push the check back.
    tokio::time::advance(Duration::from_millis(1000)).await;
    r.on_netlink(&[LinkEvent::Ipv4Addr], false);
    assert_eq!(r.timers.next().await, Timer::Settled);
    assert_eq!(Instant::now() - start, Duration::from_millis(1500));
    r.change_forced = false;

    // Same state again (wireless noise): nothing to do.
    r.on_netlink(
        &[LinkEvent::Link {
            index: 3,
            flags: UP_RUNNING,
        }],
        false,
    );
    assert!(!r.timers.is_set(Timer::Settled));
    // Link went down: a change, but no forced announcement.
    r.on_netlink(&[LinkEvent::Link { index: 3, flags: 1 }], false);
    assert!(r.timers.is_set(Timer::Settled) && !r.change_forced);
    r.timers.cancel(Timer::Settled);
    // Lost messages count as a change with the link up.
    r.on_netlink(&[], true);
    assert!(r.timers.is_set(Timer::Settled) && r.change_forced);
    r.timers.cancel(Timer::Settled);
    r.change_forced = false;
    r.on_netlink(&[], false);
    assert!(!r.timers.is_set(Timer::Settled));
}

fn nlmsg(ty: u16, payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&(16 + payload.len() as u32).to_ne_bytes());
    m.extend_from_slice(&ty.to_ne_bytes());
    m.extend_from_slice(&[0; 10]); // flags, seq, pid
    m.extend_from_slice(payload);
    while m.len() % 4 != 0 {
        m.push(0);
    }
    m
}

fn ifinfo(index: i32, flags: u32) -> Vec<u8> {
    let mut p = vec![0, 0, 1, 0]; // family, pad, type
    p.extend_from_slice(&index.to_ne_bytes());
    p.extend_from_slice(&flags.to_ne_bytes());
    p.extend_from_slice(&0u32.to_ne_bytes()); // change
    p
}

#[test]
fn netlink_messages() {
    let up = (libc::IFF_UP | libc::IFF_RUNNING | libc::IFF_BROADCAST | libc::IFF_MULTICAST) as u32;
    let mut buf = Vec::new();
    buf.extend(nlmsg(
        libc::RTM_NEWADDR,
        &[libc::AF_INET as u8, 24, 0, 0, 3, 0],
    ));
    buf.extend(nlmsg(
        libc::RTM_DELADDR,
        &[libc::AF_INET6 as u8, 64, 0, 0, 3],
    ));
    buf.extend(nlmsg(libc::RTM_NEWLINK, &ifinfo(3, up)));
    buf.extend(nlmsg(libc::RTM_DELLINK, &ifinfo(4, up)));
    buf.extend(nlmsg(libc::RTM_NEWLINK, &[0; 8])); // too short for ifinfomsg
    buf.extend(nlmsg(libc::RTM_NEWROUTE, &[0; 12]));
    buf.extend(nlmsg(
        libc::RTM_DELADDR,
        &[libc::AF_INET as u8, 24, 0, 0, 3],
    ));
    // A header claiming more than the datagram holds ends the parse.
    let mut cut = nlmsg(libc::RTM_NEWADDR, &[libc::AF_INET as u8, 24, 0, 0]);
    cut[0] = 200;
    buf.extend(cut);
    let mut events = Vec::new();
    parse_netlink(&buf, &mut events);
    assert_eq!(
        events,
        vec![
            LinkEvent::Ipv4Addr,
            LinkEvent::Link {
                index: 3,
                flags: UP_RUNNING
            },
            LinkEvent::Link { index: 4, flags: 0 },
            LinkEvent::Ipv4Addr,
        ]
    );
}

#[test]
fn handle_reports_no_address_as_none() {
    let (cmds, _rx) = unbounded_channel();
    let h = MdnsHandle {
        cmds,
        ip: Rc::new(Cell::new(Ipv4Addr::UNSPECIFIED)),
    };
    assert_eq!(h.ipv4(), None);
    h.ip.set(Ipv4Addr::new(192, 168, 1, 50));
    assert_eq!(h.ipv4(), Some(Ipv4Addr::new(192, 168, 1, 50)));
}
