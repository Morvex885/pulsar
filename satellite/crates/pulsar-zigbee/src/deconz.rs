//! The Midi's Zigbee radio offered to Home Assistant (ZHA) and Zigbee2MQTT as a network
//! coordinator speaking the deCONZ serial protocol (ConBee/RaspBee) over TCP: ZHA (zigpy-deconz)
//! connects to socket://<ip>:<port>, Zigbee2MQTT to tcp://<ip>:<port> with adapter "deconz". The
//! port is advertised over mDNS as _zigbee-coordinator._tcp (radio_type=deconz), which ZHA
//! discovers by itself.
//!
//! Frames are SLIP (0xC0 ends, 0xDB escapes) with a 16-bit two's complement checksum, LE, after
//! the frame: `[command][seq][status][frame length LE16]...`. One client at a time; a new
//! connection replaces the previous one.
//!
//! The host chooses the network: PAN ID, extended PAN ID, key, channel, coordinator IEEE address
//! and frame counter are written before switching the network state to CONNECTED; parameters the
//! host does not write keep the radio's current values. Only the global trust center link key is
//! supported (no install codes).
//!
//! The server is driven by its owner (the Zigbee service): `w = server.next().await` followed by
//! `server.handle(w, &mut coord)`, and it is the coordinator's [`Hooks`].

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use log::{debug, error, info, warn};
use socket2::{Domain, SockRef, Socket, Type};
use tokio::net::{TcpListener, TcpStream};

use crate::bytes::{get_le16, get_le32, get_le64, put_le16, put_le32, put_le64};
use crate::coordinator::{
    ApsFrame, Coordinator, Hooks, NetworkConfig, NetworkKey, SimpleDescriptor, get_descriptor,
    put_descriptor,
};
use crate::os_error_text;

const END: u8 = 0xC0;
const ESC: u8 = 0xDB;
const ESC_END: u8 = 0xDC;
const ESC_ESC: u8 = 0xDD;

// Commands.
const APS_DATA_CONFIRM: u8 = 0x04;
const DEVICE_STATE: u8 = 0x07;
const CHANGE_NETWORK_STATE: u8 = 0x08;
const READ_PARAMETER: u8 = 0x0A;
const WRITE_PARAMETER: u8 = 0x0B;
const VERSION: u8 = 0x0D;
const DEVICE_STATE_CHANGED: u8 = 0x0E;
const APS_DATA_REQUEST: u8 = 0x12;
const APS_DATA_INDICATION: u8 = 0x17;
// Status.
const SUCCESS: u8 = 0;
const FAILURE: u8 = 1;
const UNSUPPORTED: u8 = 4;
const INVALID_VALUE: u8 = 7;
// Network states and device state flags (in the device state byte, above the 2-bit state).
const OFFLINE: u8 = 0;
const JOINING: u8 = 1;
const CONNECTED: u8 = 2;
const FLAG_CONFIRM: u8 = 0x04;
const FLAG_INDICATION: u8 = 0x08;
const FLAG_FREE_SLOTS: u8 = 0x20;
// Parameters.
const MAC_ADDRESS: u8 = 0x01;
const NWK_PAN_ID: u8 = 0x05;
const NWK_ADDRESS: u8 = 0x07;
const NWK_EXT_PAN_ID: u8 = 0x08;
const DESIGNED_COORDINATOR: u8 = 0x09;
const CHANNEL_MASK: u8 = 0x0A;
const APS_EXT_PAN_ID: u8 = 0x0B;
const TRUST_CENTER_ADDRESS: u8 = 0x0E;
const SECURITY_MODE: u8 = 0x10;
const CONFIGURE_ENDPOINT: u8 = 0x13;
const PREDEFINED_PAN_ID: u8 = 0x15;
const NETWORK_KEY: u8 = 0x18;
const LINK_KEY: u8 = 0x19;
const CURRENT_CHANNEL: u8 = 0x1C;
const PERMIT_JOIN: u8 = 0x21;
const PROTOCOL_VERSION: u8 = 0x22;
const NWK_UPDATE_ID: u8 = 0x24;
const WATCHDOG_TTL: u8 = 0x26;
const FRAME_COUNTER: u8 = 0x27;
const ZDP_RESPONSE_HANDLING: u8 = 0x28;

/// ConBee III (platform 0x09) firmware 0x26780900: zigpy-deconz then scans energy with ZDO
/// Mgmt_NWK_Update_req to the coordinator (answered by the coordinator) and takes the values as
/// they are. Protocol version 0x0106 keeps zigpy off the commands this radio has no equivalent
/// for (update_neighbor, watchdog TTL, source routes).
const FIRMWARE_VERSION: u32 = 0x2678_0900;
const PROTOCOL: u16 = 0x0106;
const MAX_QUEUED: usize = 64;
/// Sends awaiting confirm before "no free slots".
const MAX_IN_FLIGHT: usize = 8;
/// SLIP bytes kept of one frame.
const MAX_SLIP: usize = 1024;
/// Replies waiting for a client that does not read: beyond this it is dropped (the radio's
/// traffic must not pile up in the daemon's memory).
const OUT_MAX: usize = 64 << 10;
/// Reads per wake-up; more input waits for the next one, after the radio's events.
const READS_PER_WAKE: usize = 4;
/// Frames waiting in the link's queue beyond which APS requests are refused.
const LINK_QUEUE_MAX: usize = 16;
/// Accepting is paused this long after an accept error for want of descriptors or memory: the
/// listener stays ready, so retrying at once would spin.
const ACCEPT_RETRY_MS: u64 = 500;
const SECURITY_ONLY_TCLK: u8 = 0x03;
const TC_LINK_KEY: [u8; 16] = *b"ZigBeeAlliance09";

pub const ENDPOINT_SLOTS: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Request {
    req_id: u8,
    /// deCONZ address mode: 1 group, 2 NWK, 3 IEEE.
    dst_mode: u8,
    /// NWK, group or IEEE.
    dst_addr: u64,
    dst_ep: u8,
    src_ep: u8,
}

#[derive(Clone, Copy, Debug)]
struct Confirm {
    req: Request,
    status: u8,
}

/// Forming a new network: wait for the old one to be erased, then form; JOINING meanwhile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Formation {
    None,
    AwaitErased,
    Forming,
}

/// What woke the server; hand it back to [`DeconzServer::handle`].
pub struct Wake(WakeKind);

enum WakeKind {
    Accept(io::Result<(TcpStream, SocketAddr)>),
    Readable,
    Writable,
    AcceptResume,
}

pub struct DeconzServer {
    port: u16,
    listener: Option<TcpListener>,
    /// Accepting stopped until then after EMFILE & co.
    accept_paused_until: Option<tokio::time::Instant>,
    /// An accept error was logged; cleared by the next accepted connection.
    accept_failing: bool,
    /// The client's socket (None in tests, which read `out` themselves).
    stream: Option<TcpStream>,
    connected: bool,
    /// SLIP bytes since the last 0xC0.
    inbuf: Vec<u8>,
    out: Vec<u8>,
    /// Per client: confirms of a previous client are dropped.
    generation: u32,
    /// Device state last sent to the client.
    last_state: u8,

    // Host view of the network.
    /// The host's change_network_state: CONNECTED (true) / OFFLINE.
    online: bool,
    /// The host wrote network parameters the radio does not have.
    new_network: bool,
    formation: Formation,
    /// The network to form, fixed before the erase.
    pending: NetworkConfig,
    /// Written by the host; 0 = none.
    channel_mask: u32,
    update_id: u8,
    // Written by the host for the next network; None: the current value.
    pan: Option<u16>,
    ext_pan: Option<u64>,
    ieee: Option<u64>,
    key: Option<NetworkKey>,
    frame_counter: u32,
    /// deCONZ endpoint slots: zigpy-deconz and zigbee-herdsman write the coordinator's endpoints
    /// by slot; the coordinator gets the endpoints they hold.
    slots: [Option<SimpleDescriptor>; ENDPOINT_SLOTS],
    indications: VecDeque<ApsFrame>,
    confirms: VecDeque<Confirm>,
    /// Coordinator handle -> request.
    requests: BTreeMap<u32, Request>,
}

fn simple(ep: u8, profile: u16, device: u16, i: &[u16], o: &[u16]) -> SimpleDescriptor {
    SimpleDescriptor {
        endpoint: ep,
        profile,
        device,
        version: 0,
        in_clusters: i.to_vec(),
        out_clusters: o.to_vec(),
    }
}

/// Bit `ch` of a channel mask; channels past 31 are in no mask.
fn bit(ch: u8) -> u32 {
    1u32.checked_shl(u32::from(ch)).unwrap_or(0)
}

impl DeconzServer {
    /// A server with a ConBee's default endpoint slots; [`DeconzServer::attach`] gives them to
    /// the coordinator.
    pub fn new(port: u16) -> DeconzServer {
        DeconzServer {
            port,
            listener: None,
            accept_paused_until: None,
            accept_failing: false,
            stream: None,
            connected: false,
            inbuf: Vec::new(),
            out: Vec::new(),
            generation: 0,
            last_state: 0xFF,
            online: true,
            new_network: false,
            formation: Formation::None,
            pending: NetworkConfig::default(),
            channel_mask: 0,
            update_id: 0,
            pan: None,
            ext_pan: None,
            ieee: None,
            key: None,
            frame_counter: 0,
            indications: VecDeque::new(),
            confirms: VecDeque::new(),
            requests: BTreeMap::new(),
            // A ConBee's slots, until the host writes its own.
            slots: [
                Some(simple(1, 0x0104, 0x0005, &[0x0000], &[])), // HA
                Some(simple(2, 0xC05E, 0x0840, &[0x0000], &[])), // ZLL
                Some(simple(0xF2, 0xA1E0, 0x0061, &[0x0021], &[0x0021])), // Green Power
                None,
            ],
        }
    }

    /// Sets the coordinator's endpoints to the slots' (before [`Coordinator::start`]).
    pub fn attach(&self, coord: &mut Coordinator) {
        for d in self.slots.iter().flatten() {
            coord.set_endpoint(d);
        }
    }

    fn set_slot(&mut self, coord: &mut Coordinator, slot: usize, d: &SimpleDescriptor) {
        // A slot taking another endpoint number drops the old endpoint unless another slot holds
        // it (zigbee-herdsman writes endpoint 0xF2 into slot 1).
        if let Some(old) = self.slots[slot].as_ref().map(|s| s.endpoint)
            && old != d.endpoint
        {
            self.slots[slot] = None;
            if !self.slots.iter().flatten().any(|s| s.endpoint == old) {
                coord.remove_endpoint(old);
            }
        }
        self.slots[slot] = Some(d.clone());
        coord.set_endpoint(d);
    }

    /// Listens on the port (any address, backlog 2). Errors are logged.
    pub fn start(&mut self) -> bool {
        let Ok(sock) = Socket::new(Domain::IPV4, Type::STREAM.nonblocking(), None) else {
            return false;
        };
        let _ = sock.set_reuse_address(true);
        let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, self.port));
        if let Err(e) = sock.bind(&addr.into()).and_then(|()| sock.listen(2)) {
            error!("zigbee: bind/listen :{}: {}", self.port, os_error_text(&e));
            return false;
        }
        match TcpListener::from_std(sock.into()) {
            Ok(l) => self.listener = Some(l),
            Err(e) => {
                error!("zigbee: bind/listen :{}: {}", self.port, os_error_text(&e));
                return false;
            }
        }
        info!("zigbee: deCONZ coordinator on :{}", self.port);
        true
    }

    /// A connection, client input or room to write. Cancel-safe.
    pub async fn next(&mut self) -> Wake {
        let listener = self.listener.as_ref();
        let stream = self.stream.as_ref();
        let want_write = !self.out.is_empty();
        let paused = self.accept_paused_until;
        tokio::select! {
            biased;
            () = async { tokio::time::sleep_until(paused.unwrap()).await }, if paused.is_some() => {
                Wake(WakeKind::AcceptResume)
            }
            _ = async { stream.unwrap().writable().await }, if stream.is_some() && want_write => {
                Wake(WakeKind::Writable)
            }
            r = async { listener.unwrap().accept().await }, if listener.is_some() && paused.is_none() => {
                Wake(WakeKind::Accept(r))
            }
            _ = async { stream.unwrap().readable().await }, if stream.is_some() => {
                Wake(WakeKind::Readable)
            }
        }
    }

    pub fn handle(&mut self, w: Wake, coord: &mut Coordinator) {
        match w.0 {
            WakeKind::Accept(Ok((s, addr))) => {
                self.accept_failing = false;
                self.on_accept(coord, s, addr);
            }
            WakeKind::Accept(Err(e)) => {
                let exhausted = matches!(
                    e.raw_os_error(),
                    Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
                );
                if !exhausted {
                    return; // that connection only
                }
                if !self.accept_failing {
                    warn!("zigbee: accept: {}", os_error_text(&e));
                }
                self.accept_failing = true;
                self.accept_paused_until = Some(
                    tokio::time::Instant::now() + std::time::Duration::from_millis(ACCEPT_RETRY_MS),
                );
            }
            WakeKind::AcceptResume => self.accept_paused_until = None,
            WakeKind::Readable => self.on_readable(coord),
            WakeKind::Writable => {}
        }
        // Replies of the whole batch go out together (and hooks' notifications with the next
        // wake-up, whose write branch is on while output waits).
        self.flush();
    }

    fn on_accept(&mut self, coord: &Coordinator, s: TcpStream, addr: SocketAddr) {
        // Until the radio has reported its network (start-up, or after it restarted), no host is
        // served: one told "no network" would form one, which erases the radio. zigpy and
        // herdsman retry a refused connection; error statuses would break zigpy-deconz (it
        // asserts UNSUPPORTED).
        if !coord.ready() {
            return;
        }
        if self.connected {
            info!("zigbee: new client replaces the previous one");
            self.close_client();
        }
        let _ = s.set_nodelay(true);
        let _ = SockRef::from(&s).set_keepalive(true);
        info!("zigbee: client {} connected", addr.ip());
        self.begin_client(Some(s));
    }

    fn begin_client(&mut self, stream: Option<TcpStream>) {
        self.stream = stream;
        self.connected = true;
        self.generation = self.generation.wrapping_add(1);
        self.online = true;
        // Network parameters written by a previous client are not this one's (a formation it
        // started goes on with `pending`).
        self.new_network = false;
        self.pan = None;
        self.ext_pan = None;
        self.ieee = None;
        self.key = None;
        self.frame_counter = 0;
        self.channel_mask = 0;
        self.update_id = 0;
        self.last_state = 0xFF;
        self.inbuf.clear();
        self.out.clear();
        self.indications.clear();
        self.confirms.clear();
    }

    /// A client without a socket: the test reads [`DeconzServer::take_output`].
    #[cfg(test)]
    pub(crate) fn connect_test_client(&mut self) {
        self.begin_client(None);
    }

    #[cfg(test)]
    pub(crate) fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    #[cfg(test)]
    pub(crate) fn has_client(&self) -> bool {
        self.connected
    }

    fn close_client(&mut self) {
        if !self.connected {
            return;
        }
        self.connected = false; // a write error inside flush() must not come back here
        // What was already answered goes out first (C++ wrote every reply at once).
        self.flush();
        if self.stream.take().is_some() {
            self.out.clear();
        } // else a test client: it reads what is left with take_output
        self.inbuf.clear();
        self.indications.clear();
        self.confirms.clear();
    }

    fn on_readable(&mut self, coord: &mut Coordinator) {
        let mut buf = [0u8; 1024];
        for _ in 0..READS_PER_WAKE {
            let Some(s) = &self.stream else {
                return;
            };
            match s.try_read(&mut buf) {
                Ok(0) => {
                    info!("zigbee: client disconnected");
                    return self.close_client();
                }
                Ok(n) => self.on_bytes(coord, &buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    info!("zigbee: client disconnected");
                    return self.close_client();
                }
            }
            if !self.connected {
                return;
            }
        }
    }

    /// Client bytes: SLIP frames end at 0xC0.
    pub(crate) fn on_bytes(&mut self, coord: &mut Coordinator, bytes: &[u8]) {
        for &b in bytes {
            if b == END {
                let frame = std::mem::take(&mut self.inbuf);
                if !frame.is_empty() {
                    self.on_slip(coord, &frame);
                }
                if !self.connected {
                    return;
                }
                // Keep the allocation for the next frame.
                self.inbuf = frame;
                self.inbuf.clear();
            } else if self.inbuf.len() < MAX_SLIP {
                self.inbuf.push(b);
            }
        }
    }

    fn flush(&mut self) {
        while !self.out.is_empty() {
            let Some(s) = &self.stream else {
                return;
            };
            match s.try_write(&self.out) {
                Ok(n @ 1..) => {
                    self.out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                _ => return self.close_client(),
            }
        }
    }

    fn on_slip(&mut self, coord: &mut Coordinator, raw: &[u8]) {
        let mut f = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            let mut b = raw[i];
            if b == ESC && i + 1 < raw.len() {
                i += 1;
                b = match raw[i] {
                    ESC_END => END,
                    ESC_ESC => ESC,
                    other => other,
                };
            }
            f.push(b);
            i += 1;
        }
        // Too short or a bad checksum: noise, or the EZSP/ZNP probes ZHA sends to unknown radios.
        if f.len() < 7 {
            return;
        }
        let body = f.len() - 2;
        if checksum(&f[..body]) != get_le16(&f[body..]) {
            return;
        }
        self.command(coord, f[0], f[1], &f[5..body]);
    }

    fn reply(&mut self, cmd: u8, seq: u8, status: u8, body: &[u8], payload_length: bool) {
        let extra = if payload_length { 2 } else { 0 };
        let mut f = vec![cmd, seq, status];
        // deCONZ frames are short (a 200-byte ASDU at most).
        put_le16(&mut f, (5 + extra + body.len()) as u16);
        if payload_length {
            put_le16(&mut f, body.len() as u16);
        }
        f.extend_from_slice(body);
        let sum = checksum(&f);
        put_le16(&mut f, sum);
        self.out.push(END);
        for b in f {
            match b {
                END => self.out.extend_from_slice(&[ESC, ESC_END]),
                ESC => self.out.extend_from_slice(&[ESC, ESC_ESC]),
                _ => self.out.push(b),
            }
        }
        self.out.push(END);
        if self.out.len() > OUT_MAX {
            self.flush();
        }
        if self.out.len() > OUT_MAX {
            warn!("zigbee: client not reading, dropping it");
            self.close_client();
        }
    }

    fn network_state(&self, coord: &Coordinator) -> u8 {
        if !self.online {
            return OFFLINE;
        }
        if self.formation != Formation::None {
            return JOINING;
        }
        if coord.ready() && coord.network().formed {
            CONNECTED
        } else {
            OFFLINE
        }
    }

    fn device_state(&self, coord: &Coordinator) -> u8 {
        let mut s = self.network_state(coord);
        if !self.confirms.is_empty() {
            s |= FLAG_CONFIRM;
        }
        if !self.indications.is_empty() {
            s |= FLAG_INDICATION;
        }
        if coord.pending() < MAX_IN_FLIGHT && coord.link_queued() <= LINK_QUEUE_MAX {
            s |= FLAG_FREE_SLOTS;
        }
        s
    }

    fn notify_state(&mut self, coord: &Coordinator) {
        if !self.connected {
            return;
        }
        let s = self.device_state(coord);
        if s == self.last_state {
            return;
        }
        self.last_state = s;
        // seq 0: zigpy numbers its requests 1..255, so this never matches one of them.
        self.reply(DEVICE_STATE_CHANGED, 0, SUCCESS, &[s, 0], false);
    }

    /// `p`: after [command][seq][status][frame length]; commands with a payload length start
    /// with it.
    fn command(&mut self, coord: &mut Coordinator, cmd: u8, seq: u8, p: &[u8]) {
        match cmd {
            VERSION => self.reply(cmd, seq, SUCCESS, &FIRMWARE_VERSION.to_le_bytes(), false),
            DEVICE_STATE => {
                self.last_state = self.device_state(coord);
                let s = self.last_state;
                self.reply(cmd, seq, SUCCESS, &[s, 0, 0], false);
            }
            CHANGE_NETWORK_STATE => {
                if let Some(&target) = p.first() {
                    self.change_network_state(coord, seq, target);
                }
            }
            READ_PARAMETER if p.len() >= 3 => self.read_parameter(coord, seq, &p[2..]),
            WRITE_PARAMETER if p.len() >= 3 => self.write_parameter(coord, seq, &p[2..]),
            APS_DATA_REQUEST if p.len() >= 2 => {
                self.aps_data_request(coord, seq, &p[2..], usize::from(get_le16(p)));
            }
            APS_DATA_INDICATION => {
                let Some(f) = self.indications.pop_front() else {
                    return self.reply(cmd, seq, FAILURE, &[], true);
                };
                self.last_state = self.device_state(coord);
                let mut b = vec![self.last_state];
                b.push(if f.group { 0x01 } else { 0x02 }); // group / NWK
                put_le16(&mut b, f.dst);
                b.push(f.dst_ep);
                b.push(0x02);
                put_le16(&mut b, f.src);
                b.push(f.src_ep);
                put_le16(&mut b, f.profile);
                put_le16(&mut b, f.cluster);
                // At most the radio's 0xF9-byte frame.
                put_le16(&mut b, f.data.len() as u16);
                b.extend_from_slice(&f.data);
                b.extend_from_slice(&[0, 0, f.lqi, 0, 0, 0, 0, f.rssi as u8]);
                self.reply(cmd, seq, SUCCESS, &b, true);
            }
            APS_DATA_CONFIRM => {
                let Some(c) = self.confirms.pop_front() else {
                    return self.reply(cmd, seq, FAILURE, &[], true);
                };
                self.last_state = self.device_state(coord);
                let mut b = vec![self.last_state, c.req.req_id, c.req.dst_mode];
                if c.req.dst_mode == 0x03 {
                    put_le64(&mut b, c.req.dst_addr);
                } else {
                    put_le16(&mut b, c.req.dst_addr as u16);
                }
                if c.req.dst_mode != 0x01 {
                    b.push(c.req.dst_ep);
                }
                b.extend_from_slice(&[c.req.src_ep, c.status, 0, 0, 0, 0]);
                self.reply(cmd, seq, SUCCESS, &b, true);
            }
            READ_PARAMETER | WRITE_PARAMETER | APS_DATA_REQUEST => {}
            _ => debug!("zigbee: deCONZ command 0x{cmd:02x} not supported"),
        }
    }

    fn read_parameter(&mut self, coord: &Coordinator, seq: u8, p: &[u8]) {
        let id = p[0];
        let net = coord.network();
        let formed = coord.ready() && net.formed;
        let mut v = vec![id];
        let mut status = SUCCESS;
        match id {
            MAC_ADDRESS | TRUST_CENTER_ADDRESS => put_le64(&mut v, net.ieee),
            NWK_PAN_ID => put_le16(&mut v, if formed { net.pan } else { 0xFFFF }),
            NWK_ADDRESS => put_le16(&mut v, if formed { net.nwk } else { 0xFFFF }),
            NWK_EXT_PAN_ID | APS_EXT_PAN_ID => {
                put_le64(&mut v, if formed { net.ext_pan } else { !0 });
            }
            DESIGNED_COORDINATOR | PREDEFINED_PAN_ID => v.push(1),
            CHANNEL_MASK => {
                let mask = if self.channel_mask != 0 {
                    self.channel_mask
                } else if formed {
                    bit(net.channel)
                } else {
                    0
                };
                put_le32(&mut v, mask);
            }
            SECURITY_MODE => v.push(SECURITY_ONLY_TCLK),
            CONFIGURE_ENDPOINT => {
                let slot = p.get(1).map_or(0xFF, |&s| usize::from(s));
                if slot >= ENDPOINT_SLOTS {
                    status = UNSUPPORTED;
                } else {
                    v.push(slot as u8);
                    put_descriptor(&mut v, &self.slots[slot].clone().unwrap_or_default());
                }
            }
            NETWORK_KEY => {
                v.push(p.get(1).copied().unwrap_or(0));
                if formed {
                    v.extend_from_slice(&net.key);
                } else {
                    v.extend_from_slice(&[0xFF; 16]);
                }
            }
            LINK_KEY => {
                if p.len() < 9 {
                    status = INVALID_VALUE;
                } else {
                    v.extend_from_slice(&p[1..9]);
                    v.extend_from_slice(&TC_LINK_KEY);
                }
            }
            CURRENT_CHANNEL => v.push(if formed { net.channel } else { 0 }),
            PERMIT_JOIN => v.push(0),
            PROTOCOL_VERSION => put_le16(&mut v, PROTOCOL),
            NWK_UPDATE_ID => v.push(if formed {
                net.update_id
            } else {
                self.update_id
            }),
            FRAME_COUNTER => put_le32(&mut v, net.frame_counter),
            ZDP_RESPONSE_HANDLING => put_le16(&mut v, 0),
            _ => status = UNSUPPORTED,
        }
        if status != SUCCESS {
            v.truncate(1);
        }
        self.reply(READ_PARAMETER, seq, status, &v, true);
    }

    fn write_parameter(&mut self, coord: &mut Coordinator, seq: u8, p: &[u8]) {
        let id = p[0];
        let v = &p[1..];
        let (formed, net) = {
            let net = coord.network();
            (coord.ready() && net.formed, net.clone())
        };
        let mut status = SUCCESS;
        let need = |k: usize, status: &mut u8| {
            if v.len() >= k {
                return true;
            }
            *status = INVALID_VALUE;
            false
        };
        match id {
            MAC_ADDRESS => {
                if need(8, &mut status) {
                    let ieee = get_le64(v);
                    self.ieee = Some(ieee);
                    if ieee != net.ieee {
                        self.new_network = true;
                    }
                }
            }
            NWK_ADDRESS => {
                if need(2, &mut status) && get_le16(v) != 0x0000 {
                    status = UNSUPPORTED;
                }
            }
            DESIGNED_COORDINATOR => {
                if need(1, &mut status) && v[0] != 1 {
                    status = UNSUPPORTED;
                }
            }
            FRAME_COUNTER => {
                if need(4, &mut status) {
                    self.frame_counter = get_le32(v);
                }
            }
            NWK_PAN_ID => {
                if need(2, &mut status) {
                    let pan = get_le16(v);
                    self.pan = Some(pan);
                    if !formed || pan != net.pan {
                        self.new_network = true;
                    }
                }
            }
            NWK_EXT_PAN_ID | APS_EXT_PAN_ID => {
                if need(8, &mut status) {
                    let ext = get_le64(v);
                    self.ext_pan = Some(ext);
                    if !formed || ext != net.ext_pan {
                        self.new_network = true;
                    }
                }
            }
            // [index][key]
            NETWORK_KEY => {
                if need(17, &mut status) {
                    let mut key = [0u8; 16];
                    key.copy_from_slice(&v[1..17]);
                    self.key = Some(key);
                    if !formed || key != net.key {
                        self.new_network = true;
                    }
                }
            }
            CHANNEL_MASK => {
                if need(4, &mut status) {
                    self.channel_mask = get_le32(v);
                }
            }
            NWK_UPDATE_ID => {
                if need(1, &mut status) {
                    self.update_id = v[0];
                }
            }
            // Only the global trust center link key; no install-code keys.
            LINK_KEY => {
                if need(24, &mut status) && v[8..24] != TC_LINK_KEY {
                    status = UNSUPPORTED;
                }
            }
            CONFIGURE_ENDPOINT => {
                let d = if need(2, &mut status) && usize::from(v[0]) < ENDPOINT_SLOTS {
                    get_descriptor(&v[1..])
                } else {
                    None
                };
                match d {
                    Some(d) => self.set_slot(coord, usize::from(v[0]), &d),
                    None if status == SUCCESS => status = INVALID_VALUE,
                    None => {}
                }
            }
            PERMIT_JOIN => {
                if need(1, &mut status) {
                    coord.permit_join(v[0]);
                }
            }
            TRUST_CENTER_ADDRESS
            | SECURITY_MODE
            | PREDEFINED_PAN_ID
            | WATCHDOG_TTL
            | ZDP_RESPONSE_HANDLING => {}
            _ => status = UNSUPPORTED,
        }
        self.reply(WRITE_PARAMETER, seq, status, &[id], true);
    }

    /// The network the host asked for, from the values it wrote over the radio's current ones.
    fn next_network(&self, coord: &Coordinator) -> NetworkConfig {
        let net = coord.network();
        NetworkConfig {
            channel: self.form_channel(),
            pan: self
                .pan
                .unwrap_or(if net.formed { net.pan } else { 0xFFFF }),
            ext_pan: self
                .ext_pan
                .unwrap_or(if net.formed { net.ext_pan } else { 0 }),
            key: self
                .key
                .unwrap_or(if net.formed { net.key } else { [0; 16] }),
            // Hosts read it before forming and keep it in backups.
            ieee: self.ieee.unwrap_or(net.ieee),
            // The erase restores the counter last saved to NV, behind the running one: devices
            // that keep the key would drop frames until it catches up. 1024: the stack's own NV
            // margin.
            frame_counter: self.frame_counter.max(if net.formed {
                net.frame_counter.wrapping_add(1024)
            } else {
                0
            }),
            update_id: self.update_id,
        }
    }

    fn form_channel(&self) -> u8 {
        (11..=26)
            .find(|&ch| self.channel_mask & bit(ch) != 0)
            .unwrap_or(15)
    }

    fn change_network_state(&mut self, coord: &mut Coordinator, seq: u8, target: u8) {
        let (formed, channel) = (coord.network().formed, coord.network().channel);
        if target == OFFLINE {
            self.online = false;
        } else if target == CONNECTED {
            self.online = true;
            // zigpy forms a network by writing its parameters, then OFFLINE -> CONNECTED; a
            // channel mask of 0 (with PAN ID 0xFFFF) only erases the old one. The network to
            // form is fixed now: the erase takes the current values with it.
            if self.new_network || !formed {
                self.pending = self.next_network(coord);
                if formed {
                    coord.factory_reset();
                    self.formation = if self.channel_mask != 0 {
                        Formation::AwaitErased
                    } else {
                        Formation::None
                    };
                } else if self.channel_mask != 0 {
                    self.formation = Formation::AwaitErased; // already erased: forms on the next step
                }
                self.new_network = false;
            } else if self.channel_mask.count_ones() == 1 && self.channel_mask & bit(channel) == 0 {
                let ch = self.form_channel();
                coord.change_channel(ch, self.update_id);
            }
        }
        self.reply(CHANGE_NETWORK_STATE, seq, SUCCESS, &[target], false);
        self.on_changed(coord);
    }

    /// `p`: [req id][flags][dst mode][dst][dst ep unless group][profile][cluster][src ep]
    /// [asdu len][asdu][tx options][radius][relays if flags & 2]; `declared`: the payload length
    /// the frame states (may differ from `p.len()`).
    fn aps_data_request(&mut self, coord: &mut Coordinator, seq: u8, p: &[u8], declared: usize) {
        let n = p.len();
        let fail = |s: &mut Self, coord: &Coordinator, status: u8, req_id: u8| {
            let st = s.device_state(coord);
            s.reply(APS_DATA_REQUEST, seq, status, &[st, req_id], true);
        };
        if n < 3 {
            return fail(self, coord, INVALID_VALUE, 0);
        }
        let mut r = Request {
            req_id: p[0],
            dst_mode: p[2],
            ..Request::default()
        };
        let mut i = 3;
        // zigbee-herdsman sends NWK_addr_req with NWK mode but the IEEE address in place of the
        // NWK one (the payload is 6 bytes longer than it declares): sent as the broadcast it is
        // meant to be.
        let herdsman_nwk_addr = r.dst_mode == 0x02 && n == declared + 6;
        if herdsman_nwk_addr {
            if n < i + 8 {
                return fail(self, coord, INVALID_VALUE, r.req_id);
            }
            r.dst_addr = 0xFFFD;
            i += 8;
        } else if r.dst_mode == 0x01 || r.dst_mode == 0x02 {
            if n < i + 2 {
                return fail(self, coord, INVALID_VALUE, r.req_id);
            }
            r.dst_addr = u64::from(get_le16(&p[i..]));
            i += 2;
        } else if r.dst_mode == 0x03 {
            if n < i + 8 {
                return fail(self, coord, INVALID_VALUE, r.req_id);
            }
            r.dst_addr = get_le64(&p[i..]);
            i += 8;
        } else {
            return fail(self, coord, INVALID_VALUE, r.req_id);
        }
        if r.dst_mode != 0x01 {
            if n < i + 1 {
                return fail(self, coord, INVALID_VALUE, r.req_id);
            }
            r.dst_ep = p[i];
            i += 1;
        }
        if n < i + 7 {
            return fail(self, coord, INVALID_VALUE, r.req_id);
        }
        let mut f = ApsFrame {
            profile: get_le16(&p[i..]),
            cluster: get_le16(&p[i + 2..]),
            src_ep: p[i + 4],
            ..ApsFrame::default()
        };
        r.src_ep = f.src_ep;
        let len = usize::from(get_le16(&p[i + 5..]));
        i += 7;
        if n < i + len {
            return fail(self, coord, INVALID_VALUE, r.req_id);
        }
        f.data = p[i..i + len].to_vec();
        i += len;
        if n >= i + 2 {
            f.tx_options = p[i];
            f.radius = p[i + 1];
        }
        f.dst_ep = r.dst_ep;
        f.group = r.dst_mode == 0x01;
        f.dst = if r.dst_mode == 0x03 {
            0
        } else {
            r.dst_addr as u16
        };
        // A host ignoring the "free slots" flag must not fill the radio's queue (or the daemon's
        // memory) with requests.
        if coord.pending() >= MAX_IN_FLIGHT || coord.link_queued() > LINK_QUEUE_MAX {
            return fail(self, coord, FAILURE, r.req_id);
        }
        let handle = self.generation << 8 | u32::from(r.req_id);
        self.requests.insert(handle, r);
        let ieee = if r.dst_mode == 0x03 { r.dst_addr } else { 0 };
        if !coord.send(handle, f, ieee) {
            self.requests.remove(&handle);
            return fail(self, coord, FAILURE, r.req_id);
        }
        let st = self.device_state(coord);
        self.reply(APS_DATA_REQUEST, seq, SUCCESS, &[st, r.req_id], true);
    }

    fn on_changed(&mut self, coord: &mut Coordinator) {
        if coord.ready() && self.formation == Formation::AwaitErased && !coord.network().formed {
            let cfg = self.pending.clone();
            // Forming is set after form(): form() first supersedes a formation still pending,
            // whose formed(false) resets the state. (C++ set it before, so a superseded
            // formation left the host in NONE while the new one ran.)
            coord.form(&cfg, self);
            self.formation = Formation::Forming;
        }
        // The radio restarted outside a formation: drop the host, it reconnects once the state
        // is known.
        if !coord.ready() && self.formation == Formation::None && self.connected {
            info!("zigbee: radio restarting, dropping the client");
            self.close_client();
        }
        self.notify_state(coord);
    }
}

impl Hooks for DeconzServer {
    fn received(&mut self, coord: &mut Coordinator, f: &ApsFrame) {
        if !self.connected || !self.online {
            return;
        }
        if self.indications.len() >= MAX_QUEUED {
            self.indications.pop_front();
        }
        self.indications.push_back(f.clone());
        self.notify_state(coord);
    }

    fn sent(&mut self, coord: &mut Coordinator, handle: u32, status: u8) {
        let Some(r) = self.requests.remove(&handle) else {
            return;
        };
        if handle >> 8 != self.generation || !self.connected {
            return; // a previous client's request
        }
        if self.confirms.len() >= MAX_QUEUED {
            self.confirms.pop_front();
        }
        self.confirms.push_back(Confirm { req: r, status });
        self.notify_state(coord);
    }

    fn changed(&mut self, coord: &mut Coordinator) {
        self.on_changed(coord);
    }

    fn formed(&mut self, coord: &mut Coordinator, ok: bool) {
        if !ok {
            warn!("zigbee: the radio failed to form the network");
        }
        self.formation = Formation::None;
        self.notify_state(coord);
    }
}

/// deCONZ's 16-bit two's complement of the byte sum.
fn checksum(f: &[u8]) -> u16 {
    let sum = f.iter().fold(0u16, |s, &b| s.wrapping_add(u16::from(b)));
    (!sum).wrapping_add(1)
}

#[cfg(test)]
mod tests;
