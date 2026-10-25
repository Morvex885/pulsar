//! Zigbee coordinator on pulsar's coordinator firmware (radio/coordinator, a network
//! co-processor): the chip runs the stack and the trust center, the host chooses the network (PAN
//! ID, extended PAN ID, key, channel, coordinator IEEE address, frame counter) and gets every APS
//! frame, ZDO included. Protocol: stock zbhci framing, commands 0x0Axx / 0x8Axx with
//! little-endian fields (radio/coordinator/coordinator.c has the layouts). ZDO addressed to the
//! coordinator itself is answered here.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::time::Duration;

use log::{info, warn};
use pulsar_core::Timers;
use tokio::time::Instant;

use crate::bytes::{get_le16, get_le32, get_le64, put_le16, put_le32, put_le64};
use crate::link::{AckTag, Link, LinkEvent};

const NET_GET: u16 = 0x0A00;
const NET_FORM: u16 = 0x0A01;
const NET_ERASE: u16 = 0x0A02;
const PERMIT_JOIN: u16 = 0x0A03;
const ENDPOINT: u16 = 0x0A04;
const CHANNEL: u16 = 0x0A05;
const ED_SCAN: u16 = 0x0A06;
const DATA_REQ: u16 = 0x0A10;
const NET_STATE: u16 = 0x8A00;
const ED_SCAN_RSP: u16 = 0x8A06;
const DATA_CNF: u16 = 0x8A10;
const DATA_IND: u16 = 0x8A11;
const EVENT: u16 = 0x8A20;

const STATE_FORMING: u8 = 1;
const STATE_UP: u8 = 2;
const MODE_GROUP: u8 = 0x01;
const MODE_NWK: u8 = 0x02;
const MODE_IEEE: u8 = 0x03;
/// zigpy gives up on a confirm after 60 s.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(50);
const SWEEP_MS: u64 = 5000;
/// Network state (frame counter) every 30 s.
const STATE_EVERY_SWEEPS: u32 = 6;
/// The radio's UART buffer holds 256-byte frames.
const MAX_ASDU: usize = 200;

const ZDO_SUCCESS: u8 = 0x00;
const ZDO_DEVICE_NOT_FOUND: u8 = 0x81;
const ZDO_NOT_ACTIVE: u8 = 0x83;
const ZDO_NOT_SUPPORTED: u8 = 0x84;

/// APS NO_ACK: the delivery status of frames the radio lost (restart, no confirm, refused).
pub const STATUS_NO_ACK: u8 = 0xA7;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApsFrame {
    /// NWK addresses (dst: group when `group`); dst >= 0xFFF8 is a broadcast.
    pub src: u16,
    pub dst: u16,
    pub group: bool,
    pub src_ep: u8,
    pub dst_ep: u8,
    pub profile: u16,
    pub cluster: u16,
    pub data: Vec<u8>,
    pub lqi: u8,
    pub rssi: i8,
    /// Sending: deCONZ transmit options (0x04 = APS ACK).
    pub tx_options: u8,
    /// Sending: 0 = default.
    pub radius: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimpleDescriptor {
    pub endpoint: u8,
    pub profile: u16,
    pub device: u16,
    pub version: u8,
    pub in_clusters: Vec<u16>,
    pub out_clusters: Vec<u16>,
}

/// `[endpoint][profile LE][device LE][version][n in][in…][n out][out…]`: the layout of ZDO
/// Simple_Desc_rsp, deCONZ endpoint parameters and the radio's 0x0A04.
pub fn put_descriptor(v: &mut Vec<u8>, d: &SimpleDescriptor) {
    v.push(d.endpoint);
    put_le16(v, d.profile);
    put_le16(v, d.device);
    v.push(d.version);
    for list in [&d.in_clusters, &d.out_clusters] {
        // A count byte: lists never hold more than the radio's 32 clusters.
        v.push(list.len() as u8);
        for &c in list {
            put_le16(v, c);
        }
    }
}

pub fn get_descriptor(p: &[u8]) -> Option<SimpleDescriptor> {
    if p.len() < 7 {
        return None;
    }
    let mut d = SimpleDescriptor {
        endpoint: p[0],
        profile: get_le16(&p[1..]),
        device: get_le16(&p[3..]),
        version: p[5],
        ..SimpleDescriptor::default()
    };
    let mut i = 6;
    for list in [&mut d.in_clusters, &mut d.out_clusters] {
        if i >= p.len() {
            return None;
        }
        let count = usize::from(p[i]);
        i += 1;
        if i + 2 * count > p.len() {
            return None;
        }
        list.extend(
            p[i..i + 2 * count]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c)),
        );
        i += 2 * count;
    }
    Some(d)
}

pub type NetworkKey = [u8; 16];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub formed: bool,
    pub forming: bool,
    pub pan: u16,
    pub nwk: u16,
    pub ext_pan: u64,
    pub ieee: u64,
    pub channel: u8,
    pub update_id: u8,
    pub key: NetworkKey,
    pub key_seq: u8,
    /// Outgoing NWK frame counter.
    pub frame_counter: u32,
}

impl Default for Network {
    fn default() -> Self {
        Network {
            formed: false,
            forming: false,
            pan: 0xFFFF,
            nwk: 0xFFFE,
            ext_pan: 0,
            ieee: 0,
            channel: 0,
            update_id: 0,
            key: [0; 16],
            key_seq: 0,
            frame_counter: 0,
        }
    }
}

/// A network to form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkConfig {
    pub channel: u8,
    /// 0xFFFF: the stack picks.
    pub pan: u16,
    /// 0: the coordinator's IEEE address.
    pub ext_pan: u64,
    /// All zero: the stack picks.
    pub key: NetworkKey,
    /// 0: the radio's own.
    pub ieee: u64,
    pub frame_counter: u32,
    pub update_id: u8,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            channel: 15,
            pan: 0xFFFF,
            ext_pan: 0,
            key: [0; 16],
            ieee: 0,
            frame_counter: 0,
            update_id: 0,
        }
    }
}

/// The coordinator's owner (the deCONZ server and the service). Every hook gets the
/// coordinator back and may call into it, as the C++ hooks called their `coord_` reference.
pub trait Hooks {
    /// An APS indication, or a ZDO answer of the coordinator itself.
    fn received(&mut self, coord: &mut Coordinator, frame: &ApsFrame);
    /// Delivery result of [`Coordinator::send`]: 0 = delivered (APS ACK for unicasts), else a
    /// Zigbee status.
    fn sent(&mut self, coord: &mut Coordinator, handle: u32, status: u8);
    /// `ready()` or `network()` changed.
    fn changed(&mut self, coord: &mut Coordinator);
    /// Result of [`Coordinator::form`]: true once the network is up, false when the radio
    /// refused or failed, or the formation was superseded or cut by a radio restart.
    fn formed(&mut self, coord: &mut Coordinator, ok: bool);
}

struct Pending {
    handle: u32,
    deadline: Instant,
}

#[derive(Clone, Copy)]
struct Scan {
    handle: u32,
    tsn: u8,
    mask: u32,
}

/// Hooks the C++ code posted to the loop: they run after the current dispatch.
enum Posted {
    Sent { handle: u32, status: u8 },
    Received(ApsFrame),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Sweep;

/// What woke the coordinator; hand it back to [`Coordinator::handle`].
pub struct Wake(WakeKind);

enum WakeKind {
    Link(LinkEvent),
    Sweep,
    Posted,
}

pub struct Coordinator {
    link: Link,
    timers: Timers<Sweep>,
    started: bool,
    ready: bool,
    net: Network,
    endpoints: BTreeMap<u8, SimpleDescriptor>,
    /// By the radio's 8-bit handle: 0x8A10 due.
    sent: BTreeMap<u8, Pending>,
    next_handle: u8,
    scan: Option<Scan>,
    /// Formation in progress: `Hooks::formed` due.
    form_pending: bool,
    /// The radio accepted 0x0A01 (acked); the state that follows says how it went.
    form_acked: bool,
    form_config: NetworkConfig,
    sweeps: u32,
    posted: VecDeque<Posted>,
}

impl Coordinator {
    /// `link`: open, the running firmware identified as pulsar's coordinator. Endpoints may be
    /// set before [`Coordinator::start`]; nothing else runs until then. Events the link had
    /// already queued are the coordinator's (as in C++, where `start` took over the link's
    /// callbacks in the same dispatch).
    pub fn new(link: Link) -> Coordinator {
        Coordinator {
            link,
            timers: Timers::new(),
            started: false,
            ready: false,
            net: Network::default(),
            endpoints: BTreeMap::new(),
            sent: BTreeMap::new(),
            next_handle: 0,
            scan: None,
            form_pending: false,
            form_acked: false,
            form_config: NetworkConfig::default(),
            sweeps: 0,
            posted: VecDeque::new(),
        }
    }

    pub fn start(&mut self, hooks: &mut dyn Hooks) {
        self.started = true;
        self.timers.set_repeating(Sweep, SWEEP_MS, SWEEP_MS);
        self.on_boot(hooks);
    }

    /// The radio reported its network state.
    pub fn ready(&self) -> bool {
        self.ready
    }

    pub fn network(&self) -> &Network {
        &self.net
    }

    /// Frames waiting in the link's queue (in flight included).
    pub fn link_queued(&self) -> usize {
        self.link.queued()
    }

    /// Data requests whose 0x8A10 is due.
    pub fn pending(&self) -> usize {
        self.sent.len()
    }

    /// Next thing to handle: a hook posted earlier, a link event or the 5 s sweep. Cancel-safe.
    pub async fn next(&mut self) -> Wake {
        if !self.posted.is_empty() {
            return Wake(WakeKind::Posted);
        }
        tokio::select! {
            biased;
            ev = self.link.next() => Wake(WakeKind::Link(ev)),
            Sweep = self.timers.next() => Wake(WakeKind::Sweep),
        }
    }

    pub fn handle(&mut self, w: Wake, hooks: &mut dyn Hooks) {
        match w.0 {
            WakeKind::Link(LinkEvent::Frame { ty, payload }) => self.on_frame(ty, &payload, hooks),
            WakeKind::Link(LinkEvent::Ack { tag, status }) => self.on_ack(tag, status, hooks),
            WakeKind::Link(LinkEvent::Boot) => self.on_boot(hooks),
            WakeKind::Sweep => self.sweep(),
            WakeKind::Posted => {
                for p in std::mem::take(&mut self.posted) {
                    match p {
                        Posted::Sent { handle, status } => hooks.sent(self, handle, status),
                        Posted::Received(f) => hooks.received(self, &f),
                    }
                }
            }
        }
    }

    /// Queues an APS data request (dst by NWK address or group, or by IEEE address when
    /// `dst_ieee != 0`); the result arrives through `Hooks::sent`. false: it cannot be sent at
    /// all.
    pub fn send(&mut self, handle: u32, mut f: ApsFrame, dst_ieee: u64) -> bool {
        if !self.ready || !self.net.formed || f.data.len() > MAX_ASDU {
            return false;
        }
        let to_self = if dst_ieee != 0 {
            dst_ieee == self.net.ieee
        } else {
            f.dst == self.net.nwk
        };
        if !f.group && to_self {
            if f.profile == 0 && f.dst_ep == 0 && !f.data.is_empty() {
                f.dst = self.net.nwk;
                self.local_zdo(handle, &f);
            } else {
                // Nothing listens on the coordinator's endpoints but the host.
                self.post_sent(handle, 0xC1);
            }
            return true;
        }
        if let Some(old) = self.sent.get(&self.next_handle) {
            self.posted.push_back(Posted::Sent {
                handle: old.handle,
                status: STATUS_NO_ACK,
            });
        }
        let h = self.next_handle;
        self.next_handle = h.wrapping_add(1);
        let mut p = vec![h];
        if f.group {
            p.push(MODE_GROUP);
            put_le16(&mut p, f.dst);
        } else if dst_ieee != 0 {
            p.push(MODE_IEEE);
            put_le64(&mut p, dst_ieee);
            p.push(f.dst_ep);
        } else {
            p.push(MODE_NWK);
            put_le16(&mut p, f.dst);
            p.push(f.dst_ep);
        }
        put_le16(&mut p, f.profile);
        put_le16(&mut p, f.cluster);
        p.push(f.src_ep);
        // deCONZ options: 0x01 APS security, 0x02 NWK security (always on here; APS bit 0x02
        // would turn it off), 0x04 APS ACK, 0x08 fragmentation; the rest are APS tx options as
        // they are.
        p.push(f.tx_options & 0x0D);
        p.push(f.radius);
        // At most MAX_ASDU, checked above.
        p.push(f.data.len() as u8);
        p.extend_from_slice(&f.data);
        self.sent.insert(
            h,
            Pending {
                handle,
                deadline: Instant::now() + CONFIRM_TIMEOUT,
            },
        );
        self.link.send(DATA_REQ, &p, Some(AckTag::Data(h)));
        true
    }

    /// Opens (seconds > 0) or closes joining on the coordinator itself.
    pub fn permit_join(&mut self, seconds: u8) {
        self.link.send(PERMIT_JOIN, &[seconds], None);
    }

    /// One of the coordinator's endpoints, by number (0 and 0xFF are not endpoints).
    pub fn set_endpoint(&mut self, d: &SimpleDescriptor) {
        if d.endpoint == 0 || d.endpoint == 0xFF {
            return;
        }
        self.endpoints.insert(d.endpoint, d.clone());
        if self.started {
            push_endpoint(&mut self.link, d); // else with the others at start
        }
    }

    pub fn remove_endpoint(&mut self, endpoint: u8) {
        if self.endpoints.remove(&endpoint).is_none() {
            return;
        }
        if self.started {
            self.link.send(ENDPOINT, &[endpoint, 0xFF], None);
        }
    }

    /// Erases the network in the radio (devices must join again).
    pub fn factory_reset(&mut self) {
        warn!("zigbee: erasing the network in the radio");
        self.link.send(NET_ERASE, &[], None);
    }

    /// Forms a network (none may exist); `Hooks::formed` reports the result. A formation still
    /// pending is superseded (`formed(false)` for it first).
    pub fn form(&mut self, c: &NetworkConfig, hooks: &mut dyn Hooks) {
        info!(
            "zigbee: forming a network on channel {}, PAN 0x{:04x}, update id {}",
            c.channel, c.pan, c.update_id
        );
        self.finish_form(false, hooks);
        self.form_pending = true;
        self.form_acked = false;
        self.form_config = c.clone();
        let mut p = vec![c.channel];
        put_le16(&mut p, c.pan);
        put_le64(&mut p, c.ext_pan);
        p.extend_from_slice(&c.key);
        put_le64(&mut p, c.ieee);
        put_le32(&mut p, c.frame_counter);
        p.push(c.update_id);
        // Accepted: the state that follows the ACK says whether the formation started.
        self.link.send(NET_FORM, &p, Some(AckTag::Form));
    }

    /// Moves the formed network to another channel (broadcast Mgmt_NWK_Update_req).
    pub fn change_channel(&mut self, channel: u8, update_id: u8) {
        info!("zigbee: moving the network to channel {channel}");
        self.link.send(CHANNEL, &[channel, update_id], None);
    }

    fn set_ready(&mut self, r: bool, hooks: &mut dyn Hooks) {
        if self.ready == r {
            return;
        }
        self.ready = r;
        hooks.changed(self);
    }

    fn post_sent(&mut self, handle: u32, status: u8) {
        self.posted.push_back(Posted::Sent { handle, status });
    }

    fn on_boot(&mut self, hooks: &mut dyn Hooks) {
        // Everything the radio held in RAM is gone, endpoints included.
        for (_, p) in std::mem::take(&mut self.sent) {
            self.post_sent(p.handle, STATUS_NO_ACK);
        }
        if let Some(s) = self.scan.take() {
            self.post_sent(s.handle, STATUS_NO_ACK);
        }
        self.finish_form(false, hooks);
        self.set_ready(false, hooks);
        for d in self.endpoints.values() {
            push_endpoint(&mut self.link, d);
        }
        self.link.send(NET_GET, &[], None);
    }

    fn on_ack(&mut self, tag: AckTag, status: i32, hooks: &mut dyn Hooks) {
        match tag {
            AckTag::Endpoint(ep) if status != 0 => {
                warn!("zigbee: the radio refused endpoint {ep} (status {status})");
            }
            AckTag::Form if status == 0 => self.form_acked = true,
            AckTag::Form => {
                warn!("zigbee: the radio refused to form a network (status {status})");
                self.finish_form(false, hooks);
            }
            AckTag::Data(h) if status != 0 => self.on_confirm(h, STATUS_NO_ACK),
            AckTag::EdScan if status != 0 => {
                if let Some(s) = self.scan.take() {
                    self.post_sent(s.handle, STATUS_NO_ACK);
                }
            }
            _ => {}
        }
    }

    fn finish_form(&mut self, ok: bool, hooks: &mut dyn Hooks) {
        if !self.form_pending {
            return;
        }
        self.form_pending = false;
        hooks.formed(self, ok);
    }

    fn on_confirm(&mut self, radio_handle: u8, status: u8) {
        if let Some(p) = self.sent.remove(&radio_handle) {
            self.post_sent(p.handle, status);
        }
    }

    fn deliver_zdo(&mut self, src: u16, rsp_cluster: u16, payload: Vec<u8>) {
        self.posted.push_back(Posted::Received(ApsFrame {
            src,
            dst: self.net.nwk,
            cluster: rsp_cluster,
            data: payload,
            lqi: 0xFF,
            ..ApsFrame::default()
        }));
    }

    fn local_zdo(&mut self, handle: u32, f: &ApsFrame) {
        let tsn = f.data[0];
        let b = &f.data[1..];
        let me = self.net.nwk;
        let rsp = f.cluster | 0x8000;
        let mut r = vec![tsn, ZDO_SUCCESS];
        let put_addr = |r: &mut Vec<u8>, net: &Network| {
            put_le64(r, net.ieee);
            put_le16(r, me);
        };
        match f.cluster {
            // NWK_addr_req
            0x0000 => {
                if b.len() >= 8 && get_le64(b) == self.net.ieee {
                    put_addr(&mut r, &self.net);
                } else {
                    r[1] = ZDO_DEVICE_NOT_FOUND;
                }
            }
            // IEEE_addr_req
            0x0001 => {
                if b.len() >= 2 && get_le16(b) == me {
                    put_addr(&mut r, &self.net);
                } else {
                    r[1] = ZDO_DEVICE_NOT_FOUND;
                }
            }
            // Node_Desc_req: coordinator, 2.4 GHz, FFD + mains + RX on + allocates addresses,
            // manufacturer 0x140A (the radio's own), buffers 82/128, server mask: primary trust
            // center + network manager, stack compliance revision 22.
            0x0002 => {
                put_le16(&mut r, me);
                r.extend_from_slice(&[
                    0x00, 0x40, 0x8F, 0x0A, 0x14, 0x52, 0x80, 0x00, 0x41, 0x2C, 0x80, 0x00, 0x00,
                ]);
            }
            // Simple_Desc_req
            0x0004 => {
                put_le16(&mut r, me);
                match b.get(2).and_then(|ep| self.endpoints.get(ep)) {
                    Some(d) => {
                        let mut desc = Vec::new();
                        put_descriptor(&mut desc, d);
                        // Small: 8 bytes plus two bytes per cluster, at most 32 + 32.
                        r.push(desc.len() as u8);
                        r.extend_from_slice(&desc);
                    }
                    None => {
                        r[1] = ZDO_NOT_ACTIVE;
                        r.push(0);
                    }
                }
            }
            // Active_EP_req
            0x0005 => {
                put_le16(&mut r, me);
                // At most 254 endpoints (0 and 0xFF are refused).
                r.push(self.endpoints.len() as u8);
                r.extend(self.endpoints.keys());
            }
            // Mgmt_Permit_Joining_req
            0x0036 => {
                if let Some(&seconds) = b.first() {
                    self.permit_join(seconds);
                }
            }
            // Mgmt_NWK_Update_req: energy scan of the coordinator itself
            0x0038 => {
                if b.len() >= 5 && b[4] > 5 {
                    // channel change / manager update: no response
                    self.post_sent(handle, ZDO_SUCCESS);
                    return;
                }
                if b.len() < 5 {
                    r[1] = ZDO_NOT_SUPPORTED;
                } else {
                    return self.energy_scan(handle, tsn, get_le32(b) & 0x07FF_F800, b[4]);
                }
            }
            _ => r[1] = ZDO_NOT_SUPPORTED,
        }
        self.post_sent(handle, ZDO_SUCCESS);
        self.deliver_zdo(me, rsp, r);
    }

    fn energy_scan(&mut self, handle: u32, tsn: u8, mask: u32, exponent: u8) {
        if self.scan.is_some() {
            self.post_sent(handle, ZDO_SUCCESS);
            self.deliver_zdo(self.net.nwk, 0x8038, vec![tsn, ZDO_NOT_SUPPORTED]);
            return;
        }
        self.scan = Some(Scan { handle, tsn, mask });
        let mut p = Vec::new();
        put_le32(&mut p, mask);
        p.push(exponent);
        // aBaseSuperframeDuration * (2^n + 1) per channel; n <= 5 here.
        let ms = 1536 * ((1u64 << exponent) + 1) / 100;
        // The radio is off the network channel meanwhile: hold other frames back.
        self.link.send_and_hold(
            ED_SCAN,
            &p,
            Some(AckTag::EdScan),
            ED_SCAN_RSP,
            u64::from(mask.count_ones()) * ms + 3000,
        );
    }

    fn on_scan(&mut self, p: &[u8]) {
        let Some(s) = self.scan.take() else {
            return;
        };
        // [count] count x [channel][energy 0..255], channels ascending as in the mask ->
        // Mgmt_NWK_Update_notify: channels, transmissions, failures, energies.
        let mut r = vec![s.tsn, ZDO_SUCCESS];
        put_le32(&mut r, s.mask);
        put_le16(&mut r, 0);
        put_le16(&mut r, 0);
        let count = match p.first() {
            Some(&c) => usize::from(c).min((p.len() - 1) / 2),
            None => 0,
        };
        // At most 255: the radio's own count byte.
        r.push(count as u8);
        let mut measured = String::new();
        for i in 0..count {
            r.push(p[2 + 2 * i]);
            let _ = write!(measured, " {}:{}", p[1 + 2 * i], p[2 + 2 * i]);
        }
        info!("zigbee: energy scan (channel:energy){measured}");
        self.post_sent(s.handle, ZDO_SUCCESS);
        self.deliver_zdo(self.net.nwk, 0x8038, r);
    }

    fn on_state(&mut self, p: &[u8], hooks: &mut dyn Hooks) {
        // [state][status][channel][PAN][ext PAN 8][IEEE 8][NWK][update id][key 16][key seq]
        // [frame counter 4]
        if p.len() < 45 {
            return;
        }
        let mut key = [0u8; 16];
        key.copy_from_slice(&p[24..40]);
        let net = Network {
            formed: p[0] == STATE_UP,
            forming: p[0] == STATE_FORMING,
            channel: p[2],
            pan: get_le16(&p[3..]),
            ext_pan: get_le64(&p[5..]),
            ieee: get_le64(&p[13..]),
            nwk: get_le16(&p[21..]),
            update_id: p[23],
            key,
            key_seq: p[40],
            frame_counter: get_le32(&p[41..]),
        };
        let news = net.formed != self.net.formed
            || net.forming != self.net.forming
            || net.pan != self.net.pan
            || net.channel != self.net.channel
            || !self.ready;
        if news {
            let what = if net.formed {
                "on its network"
            } else if net.forming {
                "forming a network"
            } else {
                "has no network"
            };
            info!(
                "zigbee: radio {what}, PAN 0x{:04x}, channel {} (status {})",
                net.pan, net.channel, p[1]
            );
        }
        let (formed, forming, pan, channel) = (net.formed, net.forming, net.pan, net.channel);
        self.net = net;
        if self.form_pending && self.form_acked && !forming {
            if formed && self.form_config.pan != 0xFFFF && pan != self.form_config.pan {
                warn!(
                    "zigbee: PAN ID 0x{:04x} was taken on channel {channel}, the network got 0x{pan:04x}",
                    self.form_config.pan
                );
            }
            self.finish_form(formed, hooks);
        }
        if !self.ready {
            return self.set_ready(true, hooks);
        }
        if news {
            hooks.changed(self);
        }
    }

    fn on_indication(&mut self, p: &[u8], hooks: &mut dyn Hooks) {
        // [dst mode][dst][dst ep][src mode][src 2|8][src ep][profile][cluster][lqi][rssi]
        // [security][length][asdu]
        if p.len() < 5 {
            return;
        }
        let mut f = ApsFrame {
            group: p[0] == MODE_GROUP,
            dst: get_le16(&p[1..]),
            dst_ep: p[3],
            ..ApsFrame::default()
        };
        let mut i = 5;
        if p[4] == MODE_IEEE {
            f.src = 0xFFFE; // NWK unknown; deCONZ indications carry a NWK source
            i += 8;
        } else {
            if p.len() < i + 2 {
                return;
            }
            f.src = get_le16(&p[i..]);
            i += 2;
        }
        if p.len() < i + 9 {
            return;
        }
        f.src_ep = p[i];
        f.profile = get_le16(&p[i + 1..]);
        f.cluster = get_le16(&p[i + 3..]);
        f.lqi = p[i + 5];
        f.rssi = p[i + 6] as i8;
        let len = usize::from(p[i + 8]);
        i += 9;
        if p.len() < i + len {
            return;
        }
        f.data = p[i..i + len].to_vec();
        hooks.received(self, &f);
    }

    fn on_event(&self, p: &[u8]) {
        match p.first() {
            Some(1) if p.len() >= 20 => info!(
                "zigbee: device {:016x} joined as 0x{:04x} (status {})",
                get_le64(&p[3..]),
                get_le16(&p[1..]),
                p[19]
            ),
            Some(2) if p.len() >= 10 => info!(
                "zigbee: device {:016x} left{}",
                get_le64(&p[1..]),
                if p[9] != 0 { " (rejoining)" } else { "" }
            ),
            Some(3) if p.len() >= 2 => {
                if p[1] != 0 {
                    info!("zigbee: joining permitted for {} s", p[1]);
                } else {
                    info!("zigbee: joining closed");
                }
            }
            _ => {}
        }
    }

    fn on_frame(&mut self, ty: u16, p: &[u8], hooks: &mut dyn Hooks) {
        match ty {
            NET_STATE => self.on_state(p, hooks),
            DATA_CNF if p.len() >= 2 => self.on_confirm(p[0], p[1]),
            DATA_IND => self.on_indication(p, hooks),
            EVENT => self.on_event(p),
            ED_SCAN_RSP => self.on_scan(p),
            _ => {}
        }
    }

    fn sweep(&mut self) {
        let now = Instant::now();
        let posted = &mut self.posted;
        self.sent.retain(|_, p| {
            if p.deadline > now {
                return true;
            }
            posted.push_back(Posted::Sent {
                handle: p.handle,
                status: STATUS_NO_ACK,
            });
            false
        });
        // The radio reports its state on changes only; the frame counter hosts back up moves
        // on.
        self.sweeps += 1;
        if self.sweeps.is_multiple_of(STATE_EVERY_SWEEPS) && self.ready && self.scan.is_none() {
            self.link.send(NET_GET, &[], None);
        }
    }
}

fn push_endpoint(link: &mut Link, d: &SimpleDescriptor) {
    let mut p = Vec::new();
    put_descriptor(&mut p, d);
    link.send(ENDPOINT, &p, Some(AckTag::Endpoint(d.endpoint)));
}

#[cfg(test)]
mod tests;
