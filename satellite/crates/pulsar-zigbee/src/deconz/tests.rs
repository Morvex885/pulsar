use std::time::Duration;

use super::*;
use crate::coordinator::Network;
use crate::link::{BOOT_IND, LinkEvent};
use crate::test_util::{Radio, absent_lines, link_pair, poll_once};

/// The coordinator firmware as far as the server needs it: ACKs everything, answers 0x0A00,
/// erases on 0x0A02 and forms on 0x0A01 (forming, then up with the requested parameters).
struct Ncp {
    radio: Radio,
    net: Network,
    /// Payloads of 0x0A01 / 0x0A02 / 0x0A03 / 0x0A04 / 0x0A05 / 0x0A10 seen, in order.
    seen: Vec<(u16, Vec<u8>)>,
    /// 0x0A02 erases without reporting the new state.
    hold_erase: bool,
    /// 0x0A01 reports "forming" and stops there.
    hold_form: bool,
}

impl Ncp {
    fn state(&mut self, st: u8) {
        let n = &self.net;
        let mut v = vec![st, 0, n.channel];
        put_le16(&mut v, n.pan);
        put_le64(&mut v, n.ext_pan);
        put_le64(&mut v, n.ieee);
        put_le16(&mut v, n.nwk);
        v.push(n.update_id);
        v.extend_from_slice(&n.key);
        v.push(n.key_seq);
        put_le32(&mut v, n.frame_counter);
        self.radio.send(0x8A00, &v);
    }

    fn step(&mut self) -> bool {
        let Some((ty, p)) = self.radio.read_frame() else {
            return false;
        };
        self.radio.ack(ty, 0);
        match ty {
            0x0A00 => self.state(if self.net.formed { 2 } else { 0 }),
            0x0A02 => {
                self.seen.push((ty, p));
                self.net = Network {
                    ieee: self.net.ieee,
                    channel: self.net.channel,
                    ..Network::default()
                };
                if !self.hold_erase {
                    self.state(0);
                }
            }
            0x0A01 => {
                self.seen.push((ty, p.clone()));
                self.net.channel = p[0];
                self.net.pan = if get_le16(&p[1..]) == 0xFFFF {
                    0x1234
                } else {
                    get_le16(&p[1..])
                };
                self.net.ext_pan = get_le64(&p[3..]);
                self.net.key.copy_from_slice(&p[11..27]);
                if get_le64(&p[27..]) != 0 {
                    self.net.ieee = get_le64(&p[27..]);
                }
                self.net.frame_counter = get_le32(&p[35..]);
                self.net.update_id = p[39];
                self.net.nwk = 0;
                self.state(1);
                if !self.hold_form {
                    self.net.formed = true;
                    self.state(2);
                }
            }
            _ => self.seen.push((ty, p)),
        }
        true
    }
}

const IEEE: u64 = 0x0012_4B00_0102_0304;

struct Bench {
    coord: Coordinator,
    server: DeconzServer,
    ncp: Ncp,
    seq: u8,
}

/// One reply: command, sequence, status, the bytes after the 5-byte header.
type Reply = (u8, u8, u8, Vec<u8>);

impl Bench {
    /// Coordinator on a formed network (PAN 0x1A62, channel 15), a client connected.
    async fn new(formed: bool) -> Bench {
        let (mut link, radio) = link_pair(&absent_lines());
        assert_eq!(link.next().await, LinkEvent::Boot);
        let mut coord = Coordinator::new(link);
        let mut server = DeconzServer::new(0);
        server.attach(&mut coord);
        let mut net = Network {
            ieee: IEEE,
            channel: 15,
            ..Network::default()
        };
        if formed {
            net = Network {
                formed: true,
                pan: 0x1A62,
                nwk: 0,
                ext_pan: 0xDDDD_DDDD_DDDD_DDDD,
                key: [7; 16],
                frame_counter: 5000,
                update_id: 3,
                ..net
            };
        }
        coord.start(&mut server);
        let mut b = Bench {
            coord,
            server,
            ncp: Ncp {
                radio,
                net,
                seen: Vec::new(),
                hold_erase: false,
                hold_form: false,
            },
            seq: 0,
        };
        b.settle().await;
        assert!(b.coord.ready());
        b.ncp.seen.clear();
        b.server.connect_test_client();
        b
    }

    fn step(&mut self) -> bool {
        if let Some(w) = poll_once(self.coord.next()) {
            self.coord.handle(w, &mut self.server);
            return true;
        }
        self.ncp.step()
    }

    async fn settle(&mut self) {
        loop {
            if self.step() {
                continue;
            }
            tokio::task::yield_now().await;
            if !self.step() {
                break;
            }
        }
    }

    fn replies(&mut self) -> Vec<Reply> {
        decode(&self.server.take_output())
    }

    async fn req(&mut self, cmd: u8, payload: &[u8]) -> Vec<Reply> {
        self.seq = self.seq.wrapping_add(1);
        let bytes = encode(cmd, self.seq, payload);
        self.server.on_bytes(&mut self.coord, &bytes);
        self.settle().await;
        self.replies()
    }

    /// One reply to `cmd` with this sequence number, among possible state notifications.
    async fn ask(&mut self, cmd: u8, payload: &[u8]) -> (u8, Vec<u8>) {
        let rs = self.req(cmd, payload).await;
        let seq = self.seq;
        let mine: Vec<_> = rs
            .into_iter()
            .filter(|r| r.0 == cmd && r.1 == seq)
            .collect();
        assert_eq!(mine.len(), 1, "{mine:?}");
        let (_, _, status, body) = mine.into_iter().next().unwrap();
        (status, body)
    }

    async fn read(&mut self, id: u8, extra: &[u8]) -> (u8, Vec<u8>) {
        let mut p = Vec::new();
        put_le16(&mut p, 1 + extra.len() as u16);
        p.push(id);
        p.extend_from_slice(extra);
        let (st, body) = self.ask(READ_PARAMETER, &p).await;
        // [payload length][id][value]
        assert_eq!(usize::from(get_le16(&body)), body.len() - 2);
        assert_eq!(body[2], id);
        (st, body[3..].to_vec())
    }

    async fn write(&mut self, id: u8, value: &[u8]) -> u8 {
        let mut p = Vec::new();
        put_le16(&mut p, 1 + value.len() as u16);
        p.push(id);
        p.extend_from_slice(value);
        let (st, body) = self.ask(WRITE_PARAMETER, &p).await;
        assert_eq!(body, [1, 0, id]);
        st
    }
}

fn encode(cmd: u8, seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![cmd, seq, 0];
    put_le16(&mut f, 5 + payload.len() as u16);
    f.extend_from_slice(payload);
    let sum = checksum(&f);
    put_le16(&mut f, sum);
    let mut out = vec![END];
    for b in f {
        match b {
            END => out.extend_from_slice(&[ESC, ESC_END]),
            ESC => out.extend_from_slice(&[ESC, ESC_ESC]),
            _ => out.push(b),
        }
    }
    out.push(END);
    out
}

fn decode(bytes: &[u8]) -> Vec<Reply> {
    let mut out = Vec::new();
    for raw in bytes.split(|&b| b == END).filter(|r| !r.is_empty()) {
        let mut f = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == ESC {
                i += 1;
                f.push(if raw[i] == ESC_END { END } else { ESC });
            } else {
                f.push(raw[i]);
            }
            i += 1;
        }
        let n = f.len() - 2;
        assert_eq!(checksum(&f[..n]), get_le16(&f[n..]), "reply checksum");
        assert_eq!(usize::from(get_le16(&f[3..])), n, "frame length field");
        out.push((f[0], f[1], f[2], f[5..n].to_vec()));
    }
    out
}

#[test]
fn checksum_is_twos_complement_of_the_sum() {
    assert_eq!(checksum(&[0x0D, 0x01, 0x00, 0x09, 0x00]), 0xFFE9);
    assert_eq!(checksum(&[]), 0);
    assert_eq!(
        checksum(&[0xFF; 300]).wrapping_add((300u32 * 0xFF) as u16),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn slip_framing_escapes_and_rejects_noise() {
    let mut b = Bench::new(true).await;
    // VERSION: [0D][seq][00][09 00][version LE32] + checksum.
    let r = b.req(VERSION, &[0, 0, 0, 0]).await;
    assert_eq!(r, [(VERSION, 1, SUCCESS, vec![0x00, 0x09, 0x78, 0x26])]);
    // Exact bytes, framing included.
    b.server
        .on_bytes(&mut b.coord, &encode(VERSION, 2, &[0, 0, 0, 0]));
    assert_eq!(
        b.server.take_output(),
        [
            0xC0, 0x0D, 0x02, 0x00, 0x09, 0x00, 0x00, 0x09, 0x78, 0x26, 0x41, 0xFF, 0xC0
        ]
    );
    // Sequence numbers 0xC0 / 0xDB travel escaped both ways.
    let mut f = encode(VERSION, 0xC0, &[0, 0, 0, 0]);
    b.server.on_bytes(&mut b.coord, &f);
    assert_eq!(b.replies()[0].1, 0xC0);
    f = encode(VERSION, 0xDB, &[0, 0, 0, 0]);
    b.server.on_bytes(&mut b.coord, &f);
    assert_eq!(b.replies()[0].1, 0xDB);
    // Bad checksum, too short, empty frames: nothing.
    let mut bad = encode(VERSION, 3, &[0, 0, 0, 0]);
    let n = bad.len();
    bad[n - 2] ^= 1;
    b.server.on_bytes(&mut b.coord, &bad);
    b.server
        .on_bytes(&mut b.coord, &[0xC0, 0x0D, 0x01, 0x00, 0x05, 0xC0, 0xC0]);
    assert!(b.server.take_output().is_empty());
    // A frame split across reads; oversized garbage is cut at 1024 bytes and dropped.
    let f = encode(DEVICE_STATE, 4, &[0, 0, 0]);
    b.server.on_bytes(&mut b.coord, &f[..3]);
    b.server.on_bytes(&mut b.coord, &f[3..]);
    assert_eq!(b.replies(), [(DEVICE_STATE, 4, SUCCESS, vec![0x22, 0, 0])]);
    b.server.on_bytes(&mut b.coord, &[0x55; 3000]);
    b.server.on_bytes(&mut b.coord, &[0xC0]);
    assert!(b.server.take_output().is_empty());
    // Unknown commands get no answer.
    assert!(b.req(0x20, &[0, 0]).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn read_parameters_formed() {
    let mut b = Bench::new(true).await;
    let le = |v: u64, n: usize| v.to_le_bytes()[..n].to_vec();
    assert_eq!(b.read(MAC_ADDRESS, &[]).await, (SUCCESS, le(IEEE, 8)));
    assert_eq!(b.read(NWK_PAN_ID, &[]).await, (SUCCESS, le(0x1A62, 2)));
    assert_eq!(b.read(NWK_ADDRESS, &[]).await, (SUCCESS, le(0, 2)));
    let ext = le(0xDDDD_DDDD_DDDD_DDDD, 8);
    assert_eq!(b.read(NWK_EXT_PAN_ID, &[]).await, (SUCCESS, ext.clone()));
    assert_eq!(b.read(APS_EXT_PAN_ID, &[]).await, (SUCCESS, ext));
    assert_eq!(b.read(DESIGNED_COORDINATOR, &[]).await, (SUCCESS, vec![1]));
    assert_eq!(b.read(CHANNEL_MASK, &[]).await, (SUCCESS, le(1 << 15, 4)));
    assert_eq!(
        b.read(TRUST_CENTER_ADDRESS, &[]).await,
        (SUCCESS, le(IEEE, 8))
    );
    assert_eq!(b.read(SECURITY_MODE, &[]).await, (SUCCESS, vec![3]));
    let mut slot0 = vec![0];
    put_descriptor(&mut slot0, &simple(1, 0x0104, 0x0005, &[0x0000], &[]));
    assert_eq!(b.read(CONFIGURE_ENDPOINT, &[0]).await, (SUCCESS, slot0));
    let mut slot3 = vec![3];
    put_descriptor(&mut slot3, &SimpleDescriptor::default());
    assert_eq!(b.read(CONFIGURE_ENDPOINT, &[3]).await, (SUCCESS, slot3));
    assert_eq!(
        b.read(CONFIGURE_ENDPOINT, &[4]).await,
        (UNSUPPORTED, vec![])
    );
    assert_eq!(b.read(CONFIGURE_ENDPOINT, &[]).await, (UNSUPPORTED, vec![]));
    assert_eq!(b.read(PREDEFINED_PAN_ID, &[]).await, (SUCCESS, vec![1]));
    let mut key = vec![0];
    key.extend_from_slice(&[7; 16]);
    assert_eq!(b.read(NETWORK_KEY, &[0]).await, (SUCCESS, key));
    let ieee8 = [1, 2, 3, 4, 5, 6, 7, 8];
    let mut lk = ieee8.to_vec();
    lk.extend_from_slice(b"ZigBeeAlliance09");
    assert_eq!(b.read(LINK_KEY, &ieee8).await, (SUCCESS, lk));
    assert_eq!(b.read(LINK_KEY, &ieee8[..7]).await, (INVALID_VALUE, vec![]));
    assert_eq!(b.read(CURRENT_CHANNEL, &[]).await, (SUCCESS, vec![15]));
    assert_eq!(b.read(PERMIT_JOIN, &[]).await, (SUCCESS, vec![0]));
    assert_eq!(
        b.read(PROTOCOL_VERSION, &[]).await,
        (SUCCESS, le(0x0106, 2))
    );
    assert_eq!(b.read(NWK_UPDATE_ID, &[]).await, (SUCCESS, vec![3]));
    assert_eq!(b.read(FRAME_COUNTER, &[]).await, (SUCCESS, le(5000, 4)));
    assert_eq!(
        b.read(ZDP_RESPONSE_HANDLING, &[]).await,
        (SUCCESS, le(0, 2))
    );
    for id in [WATCHDOG_TTL, 0x02, 0x99] {
        assert_eq!(b.read(id, &[]).await, (UNSUPPORTED, vec![]), "0x{id:02x}");
    }
    // Shorter than [length][id]: no answer.
    assert!(b.req(READ_PARAMETER, &[1, 0]).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn read_parameters_without_network() {
    let mut b = Bench::new(false).await;
    let le = |v: u64, n: usize| v.to_le_bytes()[..n].to_vec();
    assert_eq!(b.read(MAC_ADDRESS, &[]).await, (SUCCESS, le(IEEE, 8)));
    assert_eq!(b.read(NWK_PAN_ID, &[]).await, (SUCCESS, le(0xFFFF, 2)));
    assert_eq!(b.read(NWK_ADDRESS, &[]).await, (SUCCESS, le(0xFFFF, 2)));
    assert_eq!(b.read(NWK_EXT_PAN_ID, &[]).await, (SUCCESS, vec![0xFF; 8]));
    assert_eq!(b.read(CHANNEL_MASK, &[]).await, (SUCCESS, le(0, 4)));
    let mut key = vec![5];
    key.extend_from_slice(&[0xFF; 16]);
    assert_eq!(b.read(NETWORK_KEY, &[5]).await, (SUCCESS, key));
    assert_eq!(b.read(CURRENT_CHANNEL, &[]).await, (SUCCESS, vec![0]));
    // Values the host wrote come back where the radio has none.
    assert_eq!(b.write(CHANNEL_MASK, &le(1 << 20, 4)).await, SUCCESS);
    assert_eq!(b.read(CHANNEL_MASK, &[]).await, (SUCCESS, le(1 << 20, 4)));
    assert_eq!(b.write(NWK_UPDATE_ID, &[9]).await, SUCCESS);
    assert_eq!(b.read(NWK_UPDATE_ID, &[]).await, (SUCCESS, vec![9]));
    // Device state: offline, free slots.
    assert_eq!(
        b.ask(DEVICE_STATE, &[0, 0, 0]).await,
        (SUCCESS, vec![0x20, 0, 0])
    );
}

#[tokio::test(start_paused = true)]
async fn write_parameters_table() {
    let mut b = Bench::new(true).await;
    let le = |v: u64, n: usize| v.to_le_bytes()[..n].to_vec();
    assert_eq!(b.write(MAC_ADDRESS, &le(IEEE, 8)).await, SUCCESS);
    assert_eq!(b.write(MAC_ADDRESS, &le(IEEE, 7)).await, INVALID_VALUE);
    assert_eq!(b.write(NWK_ADDRESS, &[0, 0]).await, SUCCESS);
    assert_eq!(b.write(NWK_ADDRESS, &[1, 0]).await, UNSUPPORTED);
    assert_eq!(b.write(NWK_ADDRESS, &[0]).await, INVALID_VALUE);
    assert_eq!(b.write(DESIGNED_COORDINATOR, &[1]).await, SUCCESS);
    assert_eq!(b.write(DESIGNED_COORDINATOR, &[0]).await, UNSUPPORTED);
    assert_eq!(b.write(FRAME_COUNTER, &le(9, 4)).await, SUCCESS);
    assert_eq!(b.write(NWK_PAN_ID, &le(0x1A62, 2)).await, SUCCESS);
    assert_eq!(
        b.write(NWK_EXT_PAN_ID, &le(0xDDDD_DDDD_DDDD_DDDD, 8)).await,
        SUCCESS
    );
    assert_eq!(b.write(APS_EXT_PAN_ID, &le(1, 7)).await, INVALID_VALUE);
    let mut key = vec![0];
    key.extend_from_slice(&[7; 16]);
    assert_eq!(b.write(NETWORK_KEY, &key).await, SUCCESS);
    assert_eq!(b.write(NETWORK_KEY, &key[..16]).await, INVALID_VALUE);
    assert_eq!(b.write(CHANNEL_MASK, &le(1 << 15, 4)).await, SUCCESS);
    assert_eq!(b.write(NWK_UPDATE_ID, &[3]).await, SUCCESS);
    let mut lk = vec![0xFF; 8];
    lk.extend_from_slice(b"ZigBeeAlliance09");
    assert_eq!(b.write(LINK_KEY, &lk).await, SUCCESS);
    lk[8] = b'z';
    assert_eq!(b.write(LINK_KEY, &lk).await, UNSUPPORTED);
    assert_eq!(b.write(LINK_KEY, &lk[..23]).await, INVALID_VALUE);
    for id in [
        TRUST_CENTER_ADDRESS,
        SECURITY_MODE,
        PREDEFINED_PAN_ID,
        WATCHDOG_TTL,
        ZDP_RESPONSE_HANDLING,
    ] {
        assert_eq!(b.write(id, &[1, 2, 3]).await, SUCCESS, "0x{id:02x}");
    }
    assert_eq!(b.write(0x99, &[1]).await, UNSUPPORTED);
    assert_eq!(b.write(CURRENT_CHANNEL, &[11]).await, UNSUPPORTED);
    // The radio already has all of these: CONNECTED forms nothing.
    assert!(b.ncp.seen.is_empty());
    assert_eq!(
        b.ask(CHANGE_NETWORK_STATE, &[CONNECTED]).await,
        (SUCCESS, vec![CONNECTED])
    );
    assert!(b.ncp.seen.is_empty(), "{:?}", b.ncp.seen);
    // Permit join goes to the radio.
    assert_eq!(b.write(PERMIT_JOIN, &[60]).await, SUCCESS);
    assert_eq!(b.write(PERMIT_JOIN, &[]).await, INVALID_VALUE);
    assert_eq!(b.ncp.seen, [(0x0A03, vec![60])]);
}

#[tokio::test(start_paused = true)]
async fn endpoint_slots() {
    let mut b = Bench::new(true).await;
    // zigbee-herdsman writes endpoint 0xF2 into slot 1: the ZLL endpoint 2 goes, 0xF2 stays
    // (slot 2 still holds it) and is set again.
    let d = simple(0xF2, 0xA1E0, 0x0061, &[], &[0x0021]);
    let mut v = vec![1];
    put_descriptor(&mut v, &d);
    assert_eq!(b.write(CONFIGURE_ENDPOINT, &v).await, SUCCESS);
    let mut want_ep = Vec::new();
    put_descriptor(&mut want_ep, &d);
    assert_eq!(b.ncp.seen, [(0x0A04, vec![2, 0xFF]), (0x0A04, want_ep)]);
    b.ncp.seen.clear();
    // Slot 2 moves to endpoint 3: 0xF2 is still in slot 1, nothing removed.
    let d3 = simple(3, 0x0104, 0x0100, &[0x0006], &[]);
    let mut v = vec![2];
    put_descriptor(&mut v, &d3);
    assert_eq!(b.write(CONFIGURE_ENDPOINT, &v).await, SUCCESS);
    assert_eq!(b.ncp.seen.len(), 1);
    assert_eq!(b.ncp.seen[0].1[0], 3);
    // Bad slot or descriptor.
    v[0] = 4;
    assert_eq!(b.write(CONFIGURE_ENDPOINT, &v).await, INVALID_VALUE);
    assert_eq!(b.write(CONFIGURE_ENDPOINT, &[0, 1, 2]).await, INVALID_VALUE);
    assert_eq!(b.write(CONFIGURE_ENDPOINT, &[0]).await, INVALID_VALUE);
}

/// APS data request payload after [payload length]: req id, flags, dst mode, dst...
fn aps(req_id: u8, mode: u8, dst: &[u8], rest: &[u8]) -> Vec<u8> {
    let mut v = vec![req_id, 0, mode];
    v.extend_from_slice(dst);
    v.extend_from_slice(rest);
    v
}

/// [dst ep][profile][cluster][src ep][len][asdu][tx options][radius]
const ONOFF: [u8; 12] = [
    1, 0x04, 0x01, 0x06, 0x00, 1, 0x02, 0x00, 0xAA, 0xBB, 0x04, 0x05,
];

impl Bench {
    async fn aps_request(&mut self, body: &[u8], declared: usize) -> (u8, Vec<u8>) {
        let mut p = Vec::new();
        put_le16(&mut p, declared as u16);
        p.extend_from_slice(body);
        self.ask(APS_DATA_REQUEST, &p).await
    }
}

#[tokio::test(start_paused = true)]
async fn aps_data_requests_and_confirms() {
    let mut b = Bench::new(true).await;
    // NWK.
    let body = aps(7, 0x02, &[0x34, 0x12], &ONOFF);
    let (st, r) = b.aps_request(&body, body.len()).await;
    assert_eq!((st, r), (SUCCESS, vec![2, 0, 0x22, 7]));
    let (ty, p) = b.ncp.seen.remove(0);
    assert_eq!(ty, 0x0A10);
    // [handle][NWK][dst][ep][profile][cluster][src ep][tx & 0x0D][radius][len][asdu]
    assert_eq!(
        p,
        [
            0, 0x02, 0x34, 0x12, 1, 0x04, 0x01, 0x06, 0x00, 1, 0x04, 0x05, 2, 0xAA, 0xBB
        ]
    );
    // Group: no destination endpoint.
    let body = aps(8, 0x01, &[0x05, 0x00], &ONOFF[1..]);
    assert_eq!(b.aps_request(&body, body.len()).await.0, SUCCESS);
    assert_eq!(&b.ncp.seen.remove(0).1[..5], [1, 0x01, 0x05, 0x00, 0x04]);
    // IEEE.
    let ieee = 0x0015_8D00_01AB_CDEFu64.to_le_bytes();
    let body = aps(9, 0x03, &ieee, &ONOFF);
    assert_eq!(b.aps_request(&body, body.len()).await.0, SUCCESS);
    let p = b.ncp.seen.remove(0).1;
    assert_eq!(&p[..12], [&[2, 0x03][..], &ieee, &[1, 0x04]].concat());
    // zigbee-herdsman's NWK_addr_req: NWK mode, IEEE in place of the NWK address, payload 6
    // bytes longer than declared: a broadcast to 0xFFFD.
    let zdo = [0, 0x00, 0x00, 0x00, 0x00, 0, 3, 0x00, 0x10, 0x20, 0x30];
    let body = aps(10, 0x02, &ieee, &zdo);
    assert_eq!(b.aps_request(&body, body.len() - 6).await.0, SUCCESS);
    assert_eq!(&b.ncp.seen.remove(0).1[..5], [3, 0x02, 0xFD, 0xFF, 0]);
    // Malformed: invalid value, request id echoed when known.
    for (body, id) in [
        (vec![1, 0], 0),
        (aps(11, 0x04, &[0, 0], &ONOFF), 11),
        (aps(12, 0x02, &[0x34], &[]), 12),
        (aps(13, 0x03, &ieee[..7], &[]), 13),
        (aps(14, 0x02, &[0x34, 0x12], &[]), 14),
        (aps(15, 0x02, &[0x34, 0x12], &ONOFF[..6]), 15),
        (aps(16, 0x02, &[0x34, 0x12], &ONOFF[..9]), 16),
    ] {
        let (st, r) = b.aps_request(&body, body.len()).await;
        assert_eq!((st, r[3]), (INVALID_VALUE, id), "{body:?}");
    }
    // Too long for the radio: failure.
    let mut big = ONOFF[..6].to_vec();
    put_le16(&mut big, 201);
    big.extend_from_slice(&[0; 201]);
    let body = aps(17, 0x02, &[0x34, 0x12], &big);
    assert_eq!(
        b.aps_request(&body, body.len()).await,
        (FAILURE, vec![2, 0, 0x22, 17])
    );
    assert!(b.ncp.seen.is_empty());

    // Confirm for request 7 (radio handle 0): queued, flagged, then read.
    b.ncp.radio.send(0x8A10, &[0, 0x00]);
    b.settle().await;
    let r = b.replies();
    assert_eq!(r, [(DEVICE_STATE_CHANGED, 0, SUCCESS, vec![0x26, 0])]);
    let (st, r) = b.ask(APS_DATA_CONFIRM, &[0, 0]).await;
    assert_eq!(st, SUCCESS);
    // [len][state][req id][mode][dst][ep][src ep][status][4 x 0]
    assert_eq!(r, [12, 0, 0x22, 7, 0x02, 0x34, 0x12, 1, 1, 0, 0, 0, 0, 0]);
    assert_eq!(
        b.ask(APS_DATA_CONFIRM, &[0, 0]).await,
        (FAILURE, vec![0, 0])
    );
    // Group and IEEE confirms carry their address forms.
    b.ncp.radio.send(0x8A10, &[1, 0xE9]);
    b.ncp.radio.send(0x8A10, &[2, 0x00]);
    b.settle().await;
    b.replies();
    let r = b.ask(APS_DATA_CONFIRM, &[0, 0]).await.1;
    assert_eq!(&r[2..], [0x26, 8, 0x01, 0x05, 0x00, 1, 0xE9, 0, 0, 0, 0]);
    let r = b.ask(APS_DATA_CONFIRM, &[0, 0]).await.1;
    assert_eq!(&r[2..5], [0x22, 9, 0x03]);
    assert_eq!(&r[5..13], ieee);

    // A new client: confirms of the previous one's requests are dropped.
    b.server.connect_test_client();
    b.ncp.radio.send(0x8A10, &[3, 0x00]);
    b.settle().await;
    assert!(b.replies().is_empty());
    assert_eq!(b.ask(APS_DATA_CONFIRM, &[0, 0]).await.0, FAILURE);
    let body = aps(7, 0x02, &[0x34, 0x12], &ONOFF);
    b.aps_request(&body, body.len()).await;
    b.ncp.radio.send(0x8A10, &[4, 0x00]);
    b.settle().await;
    b.replies();
    assert_eq!(b.ask(APS_DATA_CONFIRM, &[0, 0]).await.1[3], 7);
}

#[tokio::test(start_paused = true)]
async fn indications() {
    let mut b = Bench::new(true).await;
    assert_eq!(
        b.ask(APS_DATA_INDICATION, &[0, 0]).await,
        (FAILURE, vec![0, 0])
    );
    let ind = [
        0x02, 0x00, 0x00, 0x01, 0x02, 0x34, 0x12, 0x01, 0x04, 0x01, 0x06, 0x00, 0xC8, 0xC4, 0x00,
        0x03, 0xAA, 0xBB, 0xCC,
    ];
    b.ncp.radio.send(0x8A11, &ind);
    b.settle().await;
    assert_eq!(
        b.replies(),
        [(DEVICE_STATE_CHANGED, 0, SUCCESS, vec![0x2A, 0])]
    );
    let (st, r) = b.ask(APS_DATA_INDICATION, &[0, 0]).await;
    assert_eq!(st, SUCCESS);
    // [len][state][dst mode][dst][dst ep][src mode][src][src ep][profile][cluster][len][asdu]
    // [0 0 lqi 0 0 0 0 rssi]
    assert_eq!(
        r,
        [
            26, 0, 0x22, 0x02, 0x00, 0x00, 0x01, 0x02, 0x34, 0x12, 0x01, 0x04, 0x01, 0x06, 0x00, 3,
            0, 0xAA, 0xBB, 0xCC, 0, 0, 0xC8, 0, 0, 0, 0, 0xC4
        ]
    );
    // Offline host: indications are not queued. More than 64: the oldest go.
    assert_eq!(b.ask(CHANGE_NETWORK_STATE, &[OFFLINE]).await.0, SUCCESS);
    b.ncp.radio.send(0x8A11, &ind);
    b.settle().await;
    assert_eq!(b.ask(APS_DATA_INDICATION, &[0, 0]).await.0, FAILURE);
    assert_eq!(b.ask(CHANGE_NETWORK_STATE, &[CONNECTED]).await.0, SUCCESS);
    for i in 0..70u8 {
        let mut f = ind;
        f[16] = i;
        b.ncp.radio.send(0x8A11, &f);
    }
    b.settle().await;
    b.replies();
    assert_eq!(b.ask(APS_DATA_INDICATION, &[0, 0]).await.1[17], 6);
}

#[tokio::test(start_paused = true)]
async fn formation_erases_then_forms_the_written_network() {
    let mut b = Bench::new(true).await;
    // zigpy: offline, write a new network, channel 20, then connected.
    assert_eq!(
        b.ask(CHANGE_NETWORK_STATE, &[OFFLINE]).await,
        (SUCCESS, vec![OFFLINE])
    );
    assert_eq!(b.write(NWK_PAN_ID, &[0x99, 0x88]).await, SUCCESS);
    assert_eq!(
        b.write(CHANNEL_MASK, &(1u32 << 20).to_le_bytes()).await,
        SUCCESS
    );
    assert_eq!(b.write(NWK_UPDATE_ID, &[4]).await, SUCCESS);
    assert_eq!(b.write(FRAME_COUNTER, &100u32.to_le_bytes()).await, SUCCESS);
    let r = b.req(CHANGE_NETWORK_STATE, &[CONNECTED]).await;
    assert_eq!(
        r[0],
        (CHANGE_NETWORK_STATE, b.seq, SUCCESS, vec![CONNECTED])
    );
    // Erased, then formed with the written PAN and channel, the radio's ext PAN, key and IEEE,
    // the frame counter past the old one by the NV margin.
    let (ty, _) = &b.ncp.seen[0];
    assert_eq!(*ty, 0x0A02);
    let (ty, p) = &b.ncp.seen[1];
    assert_eq!(*ty, 0x0A01);
    let mut want = vec![20, 0x99, 0x88];
    put_le64(&mut want, 0xDDDD_DDDD_DDDD_DDDD);
    want.extend_from_slice(&[7; 16]);
    put_le64(&mut want, IEEE);
    put_le32(&mut want, 5000 + 1024);
    want.push(4);
    assert_eq!(p, &want);
    // The host saw JOINING meanwhile, then CONNECTED on the new network.
    let states: Vec<u8> = r
        .iter()
        .filter(|r| r.0 == DEVICE_STATE_CHANGED)
        .map(|r| r.3[0] & 3)
        .collect();
    assert_eq!(states.first(), Some(&JOINING), "{r:?}");
    assert_eq!(states.last(), Some(&CONNECTED), "{r:?}");
    assert_eq!(b.coord.network().pan, 0x8899);
    assert_eq!(b.coord.network().channel, 20);
}

#[tokio::test(start_paused = true)]
async fn formation_without_network_and_channel_change() {
    let mut b = Bench::new(false).await;
    // No network and no channel mask: CONNECTED forms nothing (the host only erased).
    assert_eq!(b.ask(CHANGE_NETWORK_STATE, &[CONNECTED]).await.0, SUCCESS);
    assert!(b.ncp.seen.is_empty());
    // With a mask: formed at once on the first channel of the mask (no erase).
    assert_eq!(
        b.write(CHANNEL_MASK, &((1u32 << 25) | (1 << 12)).to_le_bytes())
            .await,
        SUCCESS
    );
    b.req(CHANGE_NETWORK_STATE, &[CONNECTED]).await;
    assert_eq!(b.ncp.seen.len(), 1);
    assert_eq!(b.ncp.seen[0].0, 0x0A01);
    assert_eq!(b.ncp.seen[0].1[0], 12);
    assert_eq!(get_le16(&b.ncp.seen[0].1[1..]), 0xFFFF);
    b.ncp.seen.clear();
    // On the network: a single other channel in the mask moves it.
    assert_eq!(
        b.write(CHANNEL_MASK, &(1u32 << 25).to_le_bytes()).await,
        SUCCESS
    );
    assert_eq!(b.write(NWK_UPDATE_ID, &[6]).await, SUCCESS);
    b.req(CHANGE_NETWORK_STATE, &[CONNECTED]).await;
    assert_eq!(b.ncp.seen, [(0x0A05, vec![25, 6])]);
}

#[tokio::test(start_paused = true)]
async fn radio_restart_drops_the_client() {
    let mut b = Bench::new(true).await;
    assert!(b.server.has_client());
    b.ncp.radio.send(BOOT_IND, &[]);
    b.settle().await;
    assert!(!b.server.has_client());
}

/// Runs the server and the coordinator for `ms`, the radio ACKing every frame.
async fn run(server: &mut DeconzServer, coord: &mut Coordinator, radio: &mut Radio, ms: u64) {
    let end = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        tokio::select! {
            w = server.next() => server.handle(w, coord),
            w = coord.next() => coord.handle(w, server),
            () = tokio::time::sleep_until(end) => break,
        }
        while let Some((ty, _)) = radio.read_frame() {
            radio.ack(ty, 0);
        }
    }
}

#[test]
fn tcp_clients_wait_for_the_radio_and_replace_each_other() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (mut link, mut radio) = link_pair(&absent_lines());
        assert_eq!(link.next().await, LinkEvent::Boot);
        let mut coord = Coordinator::new(link);
        let mut server = DeconzServer::new(0);
        server.attach(&mut coord);
        assert!(server.start());
        let port = server
            .listener
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        coord.start(&mut server);
        // Not ready (no network state yet): the connection is closed at once.
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        run(&mut server, &mut coord, &mut radio, 50).await;
        let mut buf = [0u8; 64];
        assert_eq!(c.read(&mut buf).await.unwrap(), 0);
        // Ready: served.
        let mut st = vec![2, 0, 15, 0x62, 0x1A];
        st.extend_from_slice(&[0; 40]);
        radio.send(0x8A00, &st);
        run(&mut server, &mut coord, &mut radio, 50).await;
        let mut c1 = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        run(&mut server, &mut coord, &mut radio, 50).await;
        c1.write_all(&encode(VERSION, 1, &[0, 0, 0, 0]))
            .await
            .unwrap();
        run(&mut server, &mut coord, &mut radio, 50).await;
        let n = c1.read(&mut buf).await.unwrap();
        assert_eq!(
            decode(&buf[..n]),
            [(VERSION, 1, SUCCESS, vec![0, 9, 0x78, 0x26])]
        );
        // A second client replaces the first.
        let mut c2 = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        run(&mut server, &mut coord, &mut radio, 50).await;
        assert_eq!(c1.read(&mut buf).await.unwrap(), 0);
        c2.write_all(&encode(DEVICE_STATE, 2, &[0, 0, 0]))
            .await
            .unwrap();
        run(&mut server, &mut coord, &mut radio, 50).await;
        let n = c2.read(&mut buf).await.unwrap();
        assert_eq!(
            decode(&buf[..n]),
            [(DEVICE_STATE, 2, SUCCESS, vec![0x22, 0, 0])]
        );
        // The client leaves: the server notices.
        drop(c2);
        run(&mut server, &mut coord, &mut radio, 50).await;
        assert!(!server.has_client());
    });
}

const EXT: u64 = 0xDDDD_DDDD_DDDD_DDDD;

#[tokio::test(start_paused = true)]
async fn erase_only_without_channel_mask() {
    let mut b = Bench::new(true).await;
    assert_eq!(b.write(NWK_PAN_ID, &[0xFF, 0xFF]).await, SUCCESS);
    let r = b.req(CHANGE_NETWORK_STATE, &[CONNECTED]).await;
    assert_eq!(
        r[0],
        (CHANGE_NETWORK_STATE, b.seq, SUCCESS, vec![CONNECTED])
    );
    assert_eq!(b.ncp.seen, [(0x0A02, vec![])]);
    assert!(!b.coord.network().formed);
    // Nothing formed afterwards: OFFLINE, free slots.
    assert_eq!(
        b.ask(DEVICE_STATE, &[0, 0, 0]).await,
        (SUCCESS, vec![0x20, 0, 0])
    );
}

/// Writes one parameter, then CONNECTED with the current channel as mask: true if the radio's
/// network was erased (a new network) rather than kept.
async fn makes_new_network(id: u8, value: &[u8]) -> bool {
    let mut b = Bench::new(true).await;
    assert_eq!(b.write(id, value).await, SUCCESS);
    assert_eq!(
        b.write(CHANNEL_MASK, &(1u32 << 15).to_le_bytes()).await,
        SUCCESS
    );
    b.req(CHANGE_NETWORK_STATE, &[CONNECTED]).await;
    let erased = b.ncp.seen.first().map(|s| s.0) == Some(0x0A02);
    assert_eq!(erased, !b.ncp.seen.is_empty(), "{:?}", b.ncp.seen);
    if erased {
        // Then formed again on channel 15 with the written value.
        assert_eq!(b.ncp.seen[1].0, 0x0A01);
        assert_eq!(b.ncp.seen[1].1[0], 15);
    }
    erased
}

#[tokio::test(start_paused = true)]
async fn each_new_parameter_alone_makes_a_new_network() {
    let other_key: Vec<u8> = [&[0u8][..], &[9; 16]].concat();
    let same_key: Vec<u8> = [&[0u8][..], &[7; 16]].concat();
    assert!(makes_new_network(MAC_ADDRESS, &0x1122_3344_5566_7788u64.to_le_bytes()).await);
    assert!(makes_new_network(NWK_EXT_PAN_ID, &1u64.to_le_bytes()).await);
    assert!(makes_new_network(APS_EXT_PAN_ID, &2u64.to_le_bytes()).await);
    assert!(makes_new_network(NETWORK_KEY, &other_key).await);
    assert!(makes_new_network(NWK_PAN_ID, &[0x63, 0x1A]).await);
    // The radio's own values: the network stays.
    assert!(!makes_new_network(MAC_ADDRESS, &IEEE.to_le_bytes()).await);
    assert!(!makes_new_network(NWK_EXT_PAN_ID, &EXT.to_le_bytes()).await);
    assert!(!makes_new_network(APS_EXT_PAN_ID, &EXT.to_le_bytes()).await);
    assert!(!makes_new_network(NETWORK_KEY, &same_key).await);
    assert!(!makes_new_network(NWK_PAN_ID, &[0x62, 0x1A]).await);
}

#[tokio::test(start_paused = true)]
async fn another_clients_parameters_do_not_erase() {
    let mut b = Bench::new(true).await;
    assert_eq!(b.write(NWK_PAN_ID, &[0x99, 0x88]).await, SUCCESS);
    assert_eq!(
        b.write(CHANNEL_MASK, &(1u32 << 20).to_le_bytes()).await,
        SUCCESS
    );
    // Client B: what A wrote is forgotten; its CONNECTED keeps the network.
    b.server.connect_test_client();
    assert_eq!(b.ask(CHANGE_NETWORK_STATE, &[CONNECTED]).await.0, SUCCESS);
    assert!(b.ncp.seen.is_empty(), "{:?}", b.ncp.seen);
    assert_eq!(b.coord.network().pan, 0x1A62);
}

/// A formed bench whose host asked for a new network on channel 20; the radio holds `hold`.
async fn forming_bench(hold_erase: bool, hold_form: bool) -> Bench {
    let mut b = Bench::new(true).await;
    b.ncp.hold_erase = hold_erase;
    b.ncp.hold_form = hold_form;
    assert_eq!(b.write(NWK_PAN_ID, &[0x99, 0x88]).await, SUCCESS);
    assert_eq!(
        b.write(CHANNEL_MASK, &(1u32 << 20).to_le_bytes()).await,
        SUCCESS
    );
    b.req(CHANGE_NETWORK_STATE, &[CONNECTED]).await;
    b
}

#[tokio::test(start_paused = true)]
async fn radio_restart_while_awaiting_the_erase_keeps_the_client_and_forms() {
    let mut b = forming_bench(true, false).await;
    assert_eq!(b.ncp.seen, [(0x0A02, vec![])]);
    assert_eq!(b.server.formation, Formation::AwaitErased);
    b.ncp.radio.send(BOOT_IND, &[]);
    b.settle().await;
    // The restart's state (no network) completes the erase: formed as asked.
    assert!(b.server.has_client());
    assert_eq!(b.ncp.seen.last().unwrap().0, 0x0A01);
    assert_eq!(b.coord.network().pan, 0x8899);
    assert_eq!(b.server.formation, Formation::None);
}

#[tokio::test(start_paused = true)]
async fn radio_restart_while_forming_ends_the_formation_and_drops_the_client() {
    let mut b = forming_bench(false, true).await;
    assert_eq!(
        b.ncp.seen.iter().map(|s| s.0).collect::<Vec<_>>(),
        [0x0A02, 0x0A01]
    );
    assert_eq!(b.server.formation, Formation::Forming);
    assert_eq!(b.ask(DEVICE_STATE, &[0, 0, 0]).await.1[0] & 3, JOINING);
    b.ncp.radio.send(BOOT_IND, &[]);
    b.settle().await;
    // As in C++: the restart fails the formation first (NONE), then the radio is not ready:
    // the host is dropped and reconnects once the radio reported its state.
    assert_eq!(b.server.formation, Formation::None);
    assert!(!b.server.has_client());
}

#[tokio::test(start_paused = true)]
async fn repeated_connected_while_forming_stays_joining() {
    let mut b = forming_bench(false, true).await;
    b.ncp.seen.clear();
    let r = b.req(CHANGE_NETWORK_STATE, &[CONNECTED]).await;
    assert_eq!(r[0].3, [CONNECTED]);
    // Formed again (superseding the pending one), no erase: the radio has no network.
    assert_eq!(b.ncp.seen.iter().map(|s| s.0).collect::<Vec<_>>(), [0x0A01]);
    // FORMING survives the superseded formation's formed(false) (C++ fell back to NONE).
    assert_eq!(b.server.formation, Formation::Forming);
    assert_eq!(b.ask(DEVICE_STATE, &[0, 0, 0]).await.1[0] & 3, JOINING);
}

#[tokio::test(start_paused = true)]
async fn host_values_read_back_while_formed() {
    let mut b = Bench::new(true).await;
    let mask = (1u32 << 11) | (1 << 25);
    assert_eq!(b.write(CHANNEL_MASK, &mask.to_le_bytes()).await, SUCCESS);
    assert_eq!(
        b.read(CHANNEL_MASK, &[]).await,
        (SUCCESS, mask.to_le_bytes().to_vec())
    );
    assert_eq!(b.write(NWK_UPDATE_ID, &[9]).await, SUCCESS);
    // Formed: the network's update id, not the written one.
    assert_eq!(b.read(NWK_UPDATE_ID, &[]).await, (SUCCESS, vec![3]));
}

#[tokio::test(start_paused = true)]
async fn more_reads_without_network() {
    let mut b = Bench::new(false).await;
    assert_eq!(b.read(APS_EXT_PAN_ID, &[]).await, (SUCCESS, vec![0xFF; 8]));
    assert_eq!(
        b.read(TRUST_CENTER_ADDRESS, &[]).await,
        (SUCCESS, IEEE.to_le_bytes().to_vec())
    );
    assert_eq!(b.read(FRAME_COUNTER, &[]).await, (SUCCESS, vec![0; 4]));
    assert_eq!(b.read(NWK_UPDATE_ID, &[]).await, (SUCCESS, vec![0]));
}

#[tokio::test(start_paused = true)]
async fn aps_tx_options_drop_nwk_security_bit() {
    let mut b = Bench::new(true).await;
    let mut rest = ONOFF;
    rest[10] = 0x06; // APS ACK + NWK security (deCONZ)
    let body = aps(1, 0x02, &[0x34, 0x12], &rest);
    assert_eq!(b.aps_request(&body, body.len()).await.0, SUCCESS);
    let p = &b.ncp.seen[0].1;
    // [handle][mode][dst 2][ep][profile 2][cluster 2][src ep][tx options]
    assert_eq!(p[10], 0x04);
}

#[tokio::test(start_paused = true)]
async fn aps_requests_refused_while_confirms_are_due() {
    let mut b = Bench::new(true).await;
    for id in 0..8u8 {
        let body = aps(id, 0x02, &[0x34, 0x12], &ONOFF);
        assert_eq!(b.aps_request(&body, body.len()).await.0, SUCCESS);
    }
    // No free slots: flag off and the next request is refused.
    let body = aps(8, 0x02, &[0x34, 0x12], &ONOFF);
    assert_eq!(
        b.aps_request(&body, body.len()).await,
        (FAILURE, vec![2, 0, 0x02, 8])
    );
    b.ncp.radio.send(0x8A10, &[0, 0]);
    b.settle().await;
    assert_eq!(b.aps_request(&body, body.len()).await.0, SUCCESS);
}

#[tokio::test(start_paused = true)]
async fn slip_frames_longer_than_1024_bytes_are_cut() {
    let mut b = Bench::new(true).await;
    // Frame bytes between the END markers: 5 header + payload + 2 checksum.
    let at = |len: usize| {
        let f = encode(VERSION, 0x11, &vec![0u8; len]);
        assert_eq!(f.len(), len + 7 + 2, "no escapes in this frame");
        f
    };
    b.server.on_bytes(&mut b.coord, &at(1024 - 7));
    assert_eq!(b.replies().len(), 1, "exactly 1024 bytes: kept");
    b.server.on_bytes(&mut b.coord, &at(1024 - 6));
    assert!(b.replies().is_empty(), "1025 bytes: cut, checksum fails");
    // The next frame is read normally.
    b.server.on_bytes(&mut b.coord, &at(4));
    assert_eq!(b.replies().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn client_not_reading_is_dropped() {
    let mut b = Bench::new(true).await;
    // The test client never takes its output: replies pile up until 64 KiB.
    let f = encode(DEVICE_STATE, 1, &[0, 0, 0]);
    let mut n = 0;
    while b.server.has_client() {
        b.server.on_bytes(&mut b.coord, &f);
        n += 1;
        assert!(n < 10_000);
    }
    assert!(n > 1000, "{n}");
}

fn unhex(h: &str) -> Vec<u8> {
    if h == "-" {
        return Vec::new();
    }
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// tests/golden/deconz_script.txt against this server and coordinator; deconz.txt is what the
/// C++ DeconzServer + ZigbeeCoordinator (rust-port-base) answered to the same script over a stub
/// link: the frames the radio got and the bytes the client got, line by line.
#[tokio::test(start_paused = true)]
async fn deconz_matches_cxx_golden() {
    use crate::test_util::hex;
    let mut out = String::new();
    let mut bench: Option<(Coordinator, DeconzServer, Radio, VecDeque<u8>)> = None;
    let mut seq = 0u8;
    // Runs the coordinator and the radio (ACKing each frame) until both are idle.
    async fn settle(
        coord: &mut Coordinator,
        server: &mut DeconzServer,
        radio: &mut Radio,
        acks: &mut VecDeque<u8>,
        out: &mut String,
    ) {
        loop {
            if let Some(w) = poll_once(coord.next()) {
                coord.handle(w, server);
                continue;
            }
            if let Some((ty, p)) = radio.read_frame() {
                out.push_str(&format!("send {ty:04x} {}\n", hex(&p)));
                radio.ack(ty, acks.pop_front().unwrap_or(0));
                continue;
            }
            tokio::task::yield_now().await;
            if poll_once(coord.next())
                .map(|w| coord.handle(w, server))
                .is_none()
                && radio.read_frame().is_none()
            {
                break;
            }
        }
    }
    let script = include_str!("../../tests/golden/deconz_script.txt");
    for line in script.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.push_str(&format!("> {line}\n"));
        let w: Vec<&str> = line.split_whitespace().collect();
        if w[0] == "new" {
            let (mut link, radio) = link_pair(&absent_lines());
            assert_eq!(link.next().await, LinkEvent::Boot);
            let mut coord = Coordinator::new(link);
            let mut server = DeconzServer::new(0);
            server.attach(&mut coord);
            coord.start(&mut server);
            bench = Some((coord, server, radio, VecDeque::new()));
            seq = 0;
        }
        let (coord, server, radio, acks) = bench.as_mut().unwrap();
        match w[0] {
            "new" => {}
            "client" => {
                if coord.ready() {
                    server.connect_test_client();
                }
            }
            "req" | "raw" => {
                let bytes = if w[0] == "req" {
                    seq = seq.wrapping_add(1);
                    encode(u8::from_str_radix(w[1], 16).unwrap(), seq, &unhex(w[2]))
                } else {
                    unhex(w[1])
                };
                if server.has_client() {
                    server.on_bytes(coord, &bytes);
                }
            }
            "radio" => radio.send(u16::from_str_radix(w[1], 16).unwrap(), &unhex(w[2])),
            "ack" => acks.push_back(w[1].parse().unwrap()),
            other => panic!("script: {other}"),
        }
        settle(coord, server, radio, acks, &mut out).await;
        let got = server.take_output();
        if !got.is_empty() {
            out.push_str(&format!("client {}\n", hex(&got)));
        }
        out.push_str(&format!(
            "client_connected {}\n",
            u8::from(server.has_client())
        ));
    }
    crate::test_util::assert_golden(include_str!("../../tests/golden/deconz.txt"), &out);
}
