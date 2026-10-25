//! Minimal mDNS (RFC 6762/6763) responder advertising DNS-SD service instances over IPv4 under
//! one host name. Answers PTR/SRV/TXT/A/ANY queries for each service, its instance and
//! "<host>.local", lists every service type in "_services._dns-sd._udp.local", announces on
//! start, on `add_service`/`set_txt` and when the address or link changes (rtnetlink, with a
//! 30 s poll as fallback), and sends goodbyes when the last handle is dropped.

mod dns;
#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::fd::{AsFd, OwnedFd};
use std::rc::Rc;

use log::{debug, error, info, warn};
use pulsar_core::{Timers, actors, net};
use socket2::{Domain, SockRef, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::errstr;
use crate::sys::{self, MreqOpt};
use dns::{Dest, GROUP, MAX_SERVICES, MDNS_PORT, Records, Service};

/// RFC 6762 8.3: at least two announcements one second apart; three (0, +1 s, +3 s) also cover
/// a Wi-Fi link that is still settling after a reconnect.
const ANNOUNCE_AGAIN_MS: [u64; 2] = [1000, 3000];
/// Fallback for missed netlink events; announces only on an actual change.
const POLL_MS: u64 = 30_000;
/// A burst of netlink events (DHCP renew, reconnect) is handled once, after it settles.
const SETTLE_MS: u64 = 1500;
const RX_BUF: usize = 9000;
const NETLINK_BUF: usize = 8192;

pub struct MdnsService {
    /// e.g. "_esphomelib._tcp"
    pub service: String,
    /// Instance label without dots, e.g. the host label.
    pub instance: String,
    pub port: u16,
    /// "key=value" strings.
    pub txt: Vec<String>,
}

/// How a query was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Source port other than 5353: straight to the sender, question and id echoed.
    Legacy,
    /// QU bit: to the sender.
    Unicast,
    Multicast,
}

/// A question the responder answered (`spawn_traced`, for lan_probe).
#[derive(Clone, Debug)]
pub struct Answered {
    pub from: SocketAddrV4,
    /// Lowercase, dotted.
    pub name: String,
    pub qtype: u16,
    pub reply: Reply,
}

enum Cmd {
    AddService(MdnsService),
    SetTxt(String, Vec<String>),
}

#[derive(Clone)]
pub struct MdnsHandle {
    cmds: UnboundedSender<Cmd>,
    ip: Rc<Cell<Ipv4Addr>>,
}

impl MdnsHandle {
    /// Advertises another service on the same host (A record). Services added right after
    /// `spawn`, before the actor first runs, go out with the start announcement (C++
    /// `add_service` before `start()`); later ones are announced at once. At most 7 services.
    pub fn add_service(&self, s: MdnsService) {
        let _ = self.cmds.send(Cmd::AddService(s));
    }

    /// Replaces the TXT strings of a service added earlier (e.g. "_esphomelib._tcp") and
    /// announces it.
    pub fn set_txt(&self, service: &str, txt: Vec<String>) {
        let _ = self.cmds.send(Cmd::SetTxt(service.to_owned(), txt));
    }

    /// The IPv4 address being advertised: chosen in `spawn`, then kept up to date by the actor
    /// (netlink, 30 s poll).
    pub fn ipv4(&self) -> Option<Ipv4Addr> {
        let ip = self.ip.get();
        (!ip.is_unspecified()).then_some(ip)
    }
}

/// Opens the responder for "<host>.local". When another responder holds 5353 without
/// SO_REUSEADDR/SO_REUSEPORT, it falls back to an ephemeral port: announcements still go out,
/// queries are not heard. Err (logged) if no socket could be bound.
pub fn spawn(host: String) -> Result<MdnsHandle, String> {
    start(host, None)
}

/// `spawn` plus a channel of every question answered (lan_probe).
pub fn spawn_traced(host: String) -> Result<(MdnsHandle, UnboundedReceiver<Answered>), String> {
    let (tx, rx) = unbounded_channel();
    start(host, Some(tx)).map(|h| (h, rx))
}

fn start(host: String, trace: Option<UnboundedSender<Answered>>) -> Result<MdnsHandle, String> {
    let sock = open_socket()?;
    let ip = Rc::new(Cell::new(Ipv4Addr::UNSPECIFIED));
    let mut r = Responder {
        records: Records::new(&host),
        sock,
        ip: ip.clone(),
        ifindex: 0,
        ifname: String::new(),
        join_failed: false,
        link_flags: Vec::new(),
        change_forced: false,
        timers: Timers::new(),
        trace,
    };
    r.refresh_iface();
    r.join_group();
    let groups = (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR) as u32;
    let netlink = match sys::open_netlink(groups) {
        Ok(fd) => Some(fd),
        Err(e) => {
            warn!(
                "mdns: rtnetlink unavailable ({}); relying on the 30 s poll",
                errstr(&e)
            );
            None
        }
    };
    let (tx, rx) = unbounded_channel();
    actors::spawn_actor("mdns", r.run(rx, netlink));
    Ok(MdnsHandle { cmds: tx, ip })
}

fn fail(what: &str, e: &io::Error) -> String {
    let msg = format!("mdns: {what}: {}", errstr(e));
    error!("{msg}");
    msg
}

fn open_socket() -> Result<UdpSocket, String> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM.nonblocking(), None)
        .map_err(|e| fail("socket", &e))?;
    // Shared with any other responder on the station (and lan_probe next to the daemon).
    let _ = sock.set_reuse_address(true);
    let _ = sock.set_reuse_port(true);
    let _ = sock.set_multicast_ttl_v4(255);
    let _ = sock.set_multicast_loop_v4(true);
    let any = |port| SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into();
    if let Err(e) = sock.bind(&any(MDNS_PORT)) {
        warn!("mdns: bind :5353 failed ({}); announcing only", errstr(&e));
        sock.bind(&any(0)).map_err(|e| fail("bind", &e))?;
    }
    UdpSocket::from_std(sock.into()).map_err(|e| fail("socket", &e))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    AnnounceAgain1,
    AnnounceAgain2,
    Poll,
    Settled,
}

/// One rtnetlink message that may matter.
#[derive(Debug, PartialEq, Eq)]
enum LinkEvent {
    /// An IPv4 address was added or removed.
    Ipv4Addr,
    /// Link state of an interface: its IFF_UP | IFF_RUNNING bits, 0 once deleted.
    Link { index: i32, flags: u32 },
}

const UP_RUNNING: u32 = (libc::IFF_UP | libc::IFF_RUNNING) as u32;

struct Responder {
    records: Records,
    sock: UdpSocket,
    /// Read by the handles (`ipv4`); always equal to `records.ip`.
    ip: Rc<Cell<Ipv4Addr>>,
    ifindex: u32,
    ifname: String,
    join_failed: bool,
    /// ifindex -> last IFF_UP | IFF_RUNNING bits seen.
    link_flags: Vec<(i32, u32)>,
    change_forced: bool,
    timers: Timers<Timer>,
    trace: Option<UnboundedSender<Answered>>,
}

impl Responder {
    async fn run(mut self, mut cmds: UnboundedReceiver<Cmd>, netlink: Option<AsyncFd<OwnedFd>>) {
        while let Ok(cmd) = cmds.try_recv() {
            self.apply(cmd, false);
        }
        self.schedule_announcements();
        self.timers.set_repeating(Timer::Poll, POLL_MS, POLL_MS);
        for s in &self.records.services {
            info!("mdns: advertising {} port {}", s.instance_fqdn, s.port);
        }
        let mut buf = vec![0u8; RX_BUF];
        let mut nl_buf = vec![0u8; NETLINK_BUF];
        let mut nl_events = Vec::new();
        loop {
            tokio::select! {
                biased;
                key = self.timers.next() => self.on_timer(key),
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.apply(cmd, true),
                    None => break,
                },
                overflow = read_netlink(netlink.as_ref(), &mut nl_buf, &mut nl_events) => {
                    self.on_netlink(&nl_events, overflow);
                }
                r = self.sock.recv_from(&mut buf) => self.on_datagram(r, &buf),
            }
        }
        self.announce(true);
    }

    /// `started` = after the start announcement (C++ `fd_ >= 0`).
    fn apply(&mut self, cmd: Cmd, started: bool) {
        match cmd {
            Cmd::AddService(s) => {
                if self.records.services.len() >= MAX_SERVICES {
                    error!("mdns: too many services, {} not advertised", s.service);
                    return;
                }
                let svc = Service::new(&s.service, &s.instance, s.port, s.txt);
                if started {
                    info!("mdns: advertising {} port {}", svc.instance_fqdn, svc.port);
                }
                self.records.services.push(svc);
                if started {
                    self.schedule_announcements();
                }
            }
            Cmd::SetTxt(service, txt) => {
                let found = self
                    .records
                    .services
                    .iter_mut()
                    .find(|s| s.type_fqdn.strip_suffix(".local") == Some(service.as_str()));
                if let Some(s) = found {
                    s.txt = txt;
                    if started {
                        self.schedule_announcements();
                    }
                }
            }
        }
    }

    fn on_timer(&mut self, key: Timer) {
        match key {
            Timer::AnnounceAgain1 | Timer::AnnounceAgain2 => self.announce(false),
            Timer::Poll => self.check_network(false),
            Timer::Settled => {
                let force = std::mem::take(&mut self.change_forced);
                self.check_network(force);
            }
        }
    }

    fn schedule_announcements(&mut self) {
        self.announce(false);
        self.timers.set(Timer::AnnounceAgain1, ANNOUNCE_AGAIN_MS[0]);
        self.timers.set(Timer::AnnounceAgain2, ANNOUNCE_AGAIN_MS[1]);
    }

    fn announce(&self, goodbye: bool) {
        if let Some(pkt) = self.records.announcement(goodbye) {
            self.send_to(&pkt, SocketAddrV4::new(GROUP, MDNS_PORT));
        }
    }

    fn send_to(&self, pkt: &[u8], to: SocketAddrV4) {
        // A plain sendto: tokio's try_send_to drops datagrams until the reactor has first seen
        // the socket writable, which would lose the start announcement.
        if let Err(e) = SockRef::from(&self.sock).send_to(pkt, &to.into()) {
            debug!("mdns: sendto: {}", errstr(&e));
        }
    }

    fn on_datagram(&self, r: io::Result<(usize, SocketAddr)>, buf: &[u8]) {
        let (n, from) = match r {
            Ok(v) => v,
            Err(e) => {
                if e.kind() != io::ErrorKind::Interrupted {
                    debug!("mdns: recv: {}", errstr(&e));
                }
                return;
            }
        };
        let SocketAddr::V4(from) = from else { return };
        let trace = self.trace.as_ref();
        for resp in self.records.answer(&buf[..n], from.port(), trace.is_some()) {
            let to = match resp.dest {
                Dest::Sender => from,
                Dest::Group => SocketAddrV4::new(GROUP, MDNS_PORT),
            };
            self.send_to(&resp.packet, to);
            if let Some(trace) = trace {
                for (name, qtype) in resp.questions {
                    let _ = trace.send(Answered {
                        from,
                        name,
                        qtype,
                        reply: resp.reply,
                    });
                }
            }
        }
    }

    /// Re-reads the interface choice; true if the interface or the address changed. A failed
    /// getifaddrs changes nothing.
    fn refresh_iface(&mut self) -> bool {
        let changed;
        let Ok(best) = net::best_interface_checked() else {
            return false;
        };
        match best {
            Some(best) => {
                changed = best.addr != self.records.ip || best.index != self.ifindex;
                if changed {
                    info!("mdns: using {} {}", best.name, best.addr);
                }
                self.records.ip = best.addr;
                self.ifindex = best.index;
                self.ifname = best.name;
            }
            None => {
                changed = !self.records.ip.is_unspecified();
                if changed {
                    info!("mdns: no usable IPv4 address");
                }
                self.records.ip = Ipv4Addr::UNSPECIFIED;
                self.ifindex = 0;
                self.ifname.clear();
            }
        }
        self.ip.set(self.records.ip);
        changed
    }

    /// Re-asserted on every check: after a Wi-Fi reconnect the driver may have dropped the
    /// membership (or recreated wlan0) while the address stayed the same; EADDRINUSE = still
    /// joined.
    fn join_group(&mut self) {
        if self.ifindex == 0 {
            return;
        }
        let fd = self.sock.as_fd();
        let (ip, idx) = (self.records.ip, self.ifindex);
        let failed = match sys::set_mreqn(fd, MreqOpt::AddMembership, GROUP, ip, idx) {
            Ok(()) => false,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => false,
            Err(e) => {
                if !self.join_failed {
                    warn!("mdns: IP_ADD_MEMBERSHIP on {}: {}", self.ifname, errstr(&e));
                }
                true
            }
        };
        if !failed && self.join_failed {
            info!("mdns: joined the multicast group on {}", self.ifname);
        }
        self.join_failed = failed;
        if let Err(e) = sys::set_mreqn(fd, MreqOpt::MulticastIf, GROUP, ip, idx) {
            debug!("mdns: IP_MULTICAST_IF: {}", errstr(&e));
        }
    }

    /// Re-reads the interface, re-asserts the multicast membership and, on a change or when
    /// forced (a link came back up), announces again.
    fn check_network(&mut self, force_announce: bool) {
        let (old_idx, old_ip) = (self.ifindex, self.records.ip);
        let changed = self.refresh_iface();
        if changed && old_idx != 0 {
            let fd = self.sock.as_fd();
            let _ = sys::set_mreqn(fd, MreqOpt::DropMembership, GROUP, old_ip, old_idx);
        }
        self.join_group();
        if changed || force_announce {
            self.schedule_announcements();
        }
    }

    /// Address changes always count. Link messages also carry wireless events (scans, roaming
    /// noise), so only a change of the up/running state counts.
    fn on_netlink(&mut self, events: &[LinkEvent], overflow: bool) {
        // Overflow: messages were lost, something happened; recheck.
        let (mut relevant, mut link_up) = (overflow, overflow);
        for ev in events {
            match *ev {
                LinkEvent::Ipv4Addr => relevant = true,
                LinkEvent::Link { index, flags } => {
                    match self.link_flags.iter_mut().find(|(i, _)| *i == index) {
                        None => {
                            self.link_flags.push((index, flags));
                            relevant = true;
                        }
                        Some(seen) if seen.1 != flags => {
                            seen.1 = flags;
                            relevant = true;
                        }
                        Some(_) => {}
                    }
                    if flags == UP_RUNNING {
                        link_up = true;
                    }
                }
            }
        }
        if !relevant {
            return;
        }
        self.change_forced |= link_up;
        if !self.timers.is_set(Timer::Settled) {
            self.timers.set(Timer::Settled, SETTLE_MS);
        }
    }
}

/// Drains the rtnetlink socket into `events`; true when the kernel dropped messages (ENOBUFS).
/// Pending forever without a socket.
async fn read_netlink(
    nl: Option<&AsyncFd<OwnedFd>>,
    buf: &mut [u8],
    events: &mut Vec<LinkEvent>,
) -> bool {
    events.clear();
    let Some(nl) = nl else {
        return std::future::pending().await;
    };
    let Ok(mut guard) = nl.readable().await else {
        return std::future::pending().await;
    };
    loop {
        match guard.try_io(|fd| sys::recv(fd.get_ref().as_fd(), buf)) {
            Ok(Ok(n)) => parse_netlink(&buf[..n], events),
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(e)) => return e.raw_os_error() == Some(libc::ENOBUFS),
            Err(_would_block) => return false,
        }
    }
}

/// NLMSG_HDRLEN: struct nlmsghdr (len u32, type u16, flags u16, seq u32, pid u32), already a
/// multiple of NLMSG_ALIGNTO (4).
const NLMSG_HDRLEN: usize = 16;
/// Offsets read from struct ifinfomsg: ifi_index at 4, ifi_flags at 8, 16 bytes in all.
const _: () = {
    assert!(size_of::<libc::nlmsghdr>() == NLMSG_HDRLEN);
    assert!(std::mem::offset_of!(libc::ifinfomsg, ifi_index) == 4);
    assert!(std::mem::offset_of!(libc::ifinfomsg, ifi_flags) == 8);
    assert!(size_of::<libc::ifinfomsg>() == 16);
};

/// Appends the address and link messages of one netlink datagram (struct nlmsghdr, then
/// ifaddrmsg / ifinfomsg, native byte order).
fn parse_netlink(mut left: &[u8], events: &mut Vec<LinkEvent>) {
    let ne32 = |b: &[u8], at: usize| u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    while left.len() >= NLMSG_HDRLEN {
        let len = ne32(left, 0) as usize;
        if len < NLMSG_HDRLEN || len > left.len() {
            break;
        }
        let ty = u16::from_ne_bytes([left[4], left[5]]);
        let data = &left[NLMSG_HDRLEN..len];
        match ty {
            libc::RTM_NEWADDR | libc::RTM_DELADDR => {
                if data.first() == Some(&(libc::AF_INET as u8)) {
                    events.push(LinkEvent::Ipv4Addr);
                }
            }
            libc::RTM_NEWLINK | libc::RTM_DELLINK if data.len() >= 16 => {
                let index = ne32(data, 4) as i32;
                let flags = if ty == libc::RTM_DELLINK {
                    0
                } else {
                    ne32(data, 8) & UP_RUNNING
                };
                events.push(LinkEvent::Link { index, flags });
            }
            _ => {}
        }
        let aligned = (len + 3) & !3;
        left = &left[aligned.min(left.len())..];
    }
}
