use std::collections::VecDeque;
use std::time::Duration;

use super::*;
use crate::link::BOOT_IND;
use crate::test_util::{Radio, absent_lines, assert_golden, hex, link_pair, poll_once};

/// The hooks print what the C++ harness printed.
#[derive(Default)]
struct Rec {
    out: String,
}

impl Hooks for Rec {
    fn received(&mut self, _: &mut Coordinator, f: &ApsFrame) {
        writeln!(
            self.out,
            "received src={:04x} dst={:04x} group={} src_ep={} dst_ep={} profile={:04x} cluster={:04x} lqi={} rssi={} data={}",
            f.src,
            f.dst,
            u8::from(f.group),
            f.src_ep,
            f.dst_ep,
            f.profile,
            f.cluster,
            f.lqi,
            f.rssi,
            hex(&f.data)
        )
        .unwrap();
    }

    fn sent(&mut self, _: &mut Coordinator, handle: u32, status: u8) {
        writeln!(self.out, "sent {handle:08x} {status:02x}").unwrap();
    }

    fn changed(&mut self, c: &mut Coordinator) {
        let n = c.network();
        writeln!(
            self.out,
            "changed ready={} formed={} forming={} pan={:04x} ch={} ext={:016x} ieee={:016x} nwk={:04x} upd={} key={} seq={} fc={}",
            u8::from(c.ready()),
            u8::from(n.formed),
            u8::from(n.forming),
            n.pan,
            n.channel,
            n.ext_pan,
            n.ieee,
            n.nwk,
            n.update_id,
            hex(&n.key),
            n.key_seq,
            n.frame_counter
        )
        .unwrap();
    }

    fn formed(&mut self, _: &mut Coordinator, ok: bool) {
        writeln!(self.out, "formed ok={}", u8::from(ok)).unwrap();
    }
}

/// Drives a coordinator and the radio in lock-step: the coordinator first (posted hooks, link
/// events, the sweep), then the radio reads one frame and ACKs it.
struct Run {
    c: Coordinator,
    radio: Radio,
    rec: Rec,
    acks: VecDeque<u8>,
    quiet: bool,
}

impl Run {
    fn step(&mut self) -> bool {
        if let Some(w) = poll_once(self.c.next()) {
            self.c.handle(w, &mut self.rec);
            return true;
        }
        if let Some((ty, p)) = self.radio.read_frame() {
            if !self.quiet {
                writeln!(self.rec.out, "send {ty:04x} {}", hex(&p)).unwrap();
            }
            let st = self.acks.pop_front().unwrap_or(0);
            self.radio.ack(ty, st);
            return true;
        }
        false
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

    async fn advance(&mut self, ms: u64) {
        tokio::time::advance(Duration::from_millis(ms)).await;
        self.settle().await;
    }

    async fn inject(&mut self, ty: u16, p: &[u8]) {
        self.radio.send(ty, p);
        self.settle().await;
    }

    fn print(&mut self, line: &str) {
        writeln!(self.rec.out, "{line}").unwrap();
    }

    async fn send(&mut self, handle: u32, f: ApsFrame, ieee: u64) {
        let ok = self.c.send(handle, f, ieee);
        self.print(&format!("send_result {}", u8::from(ok)));
        self.settle().await;
    }

    async fn form(&mut self, cfg: NetworkConfig) {
        self.c.form(&cfg, &mut self.rec);
        self.settle().await;
    }

    fn pending(&mut self) {
        let n = self.c.pending();
        self.print(&format!("pending {n}"));
    }
}

#[allow(clippy::too_many_arguments)]
fn state(
    st: u8,
    status: u8,
    ch: u8,
    pan: u16,
    ext: u64,
    ieee: u64,
    nwk: u16,
    upd: u8,
    key0: u8,
    seq: u8,
    fc: u32,
) -> Vec<u8> {
    let mut v = vec![st, status, ch];
    put_le16(&mut v, pan);
    put_le64(&mut v, ext);
    put_le64(&mut v, ieee);
    put_le16(&mut v, nwk);
    v.push(upd);
    v.extend((0..16).map(|i| key0 + i));
    v.push(seq);
    put_le32(&mut v, fc);
    v
}

const IEEE: u64 = 0x0012_4B00_0102_0304;
const EXT: u64 = 0xDDDD_DDDD_DDDD_DDDD;

fn zdo(dst: u16, cluster: u16, data: &[u8]) -> ApsFrame {
    ApsFrame {
        dst,
        cluster,
        data: data.to_vec(),
        ..ApsFrame::default()
    }
}

fn onoff(dst: u16, data: &[u8]) -> ApsFrame {
    ApsFrame {
        dst,
        dst_ep: 1,
        src_ep: 1,
        profile: 0x0104,
        cluster: 0x0006,
        data: data.to_vec(),
        ..ApsFrame::default()
    }
}

fn desc(ep: u8, profile: u16, device: u16, version: u8, i: &[u16], o: &[u16]) -> SimpleDescriptor {
    SimpleDescriptor {
        endpoint: ep,
        profile,
        device,
        version,
        in_clusters: i.to_vec(),
        out_clusters: o.to_vec(),
    }
}

/// Same script as the one-off C++ harness over the C++ coordinator (tests/golden/coordinator.txt):
/// NCP request payloads, state / indication / scan parsing, local ZDO answers, confirms, the
/// sweep and radio restarts.
#[tokio::test(start_paused = true)]
async fn coordinator_matches_cxx_golden() {
    let (mut link, radio) = link_pair(&absent_lines());
    assert_eq!(link.next().await, LinkEvent::Boot);
    let mut r = Run {
        c: Coordinator::new(link),
        radio,
        rec: Rec::default(),
        acks: VecDeque::new(),
        quiet: false,
    };
    r.c.set_endpoint(&desc(1, 0x0104, 0x0005, 1, &[0x0000, 0x0006], &[0x0019]));
    r.c.set_endpoint(&desc(0xF2, 0xA1E0, 0x0061, 0, &[], &[0x0021]));
    r.c.set_endpoint(&desc(0, 0x0104, 1, 0, &[], &[]));
    r.c.set_endpoint(&desc(0xFF, 0x0104, 1, 0, &[], &[]));

    r.print("step start");
    r.c.start(&mut r.rec);
    r.settle().await;
    r.print("step state-none");
    r.inject(
        0x8A00,
        &state(0, 3, 11, 0xFFFF, 0, IEEE, 0xFFFE, 0, 0, 0, 0),
    )
    .await;
    r.print("step send-not-formed");
    r.send(1, zdo(0x1234, 0, &[1]), 0).await;
    r.print("step form");
    let mut cfg = NetworkConfig {
        channel: 15,
        pan: 0x1A62,
        ext_pan: EXT,
        ieee: IEEE,
        frame_counter: 4096,
        update_id: 2,
        ..NetworkConfig::default()
    };
    for (i, k) in cfg.key.iter_mut().enumerate() {
        *k = 1 + i as u8;
    }
    r.form(cfg).await;
    r.print("step state-forming");
    r.inject(
        0x8A00,
        &state(1, 0, 15, 0x1A62, EXT, IEEE, 0, 2, 1, 0, 4096),
    )
    .await;
    r.print("step state-up");
    r.inject(
        0x8A00,
        &state(2, 0, 15, 0x1A63, EXT, IEEE, 0, 2, 1, 0, 4100),
    )
    .await;
    r.print("step state-same");
    r.inject(
        0x8A00,
        &state(2, 0, 15, 0x1A63, EXT, IEEE, 0, 2, 1, 0, 4100),
    )
    .await;
    r.print("step state-counter");
    r.inject(
        0x8A00,
        &state(2, 0, 15, 0x1A63, EXT, IEEE, 0, 2, 1, 0, 5000),
    )
    .await;
    assert_eq!(r.c.network().frame_counter, 5000);
    r.print("step state-short");
    let mut s = state(0, 0, 11, 0x2222, 0, 0, 0, 0, 0, 0, 0);
    s.pop();
    r.inject(0x8A00, &s).await;

    r.print("step send-nwk");
    let f = ApsFrame {
        tx_options: 0x07,
        radius: 5,
        ..onoff(0x1234, &[0x01, 0x02, 0x03])
    };
    r.send(0x0100, f, 0).await;
    r.pending();
    r.inject(0x8A10, &[0x00, 0x00]).await;
    r.pending();
    r.print("step send-group");
    let f = ApsFrame {
        group: true,
        dst: 0x0005,
        dst_ep: 0xFF,
        src_ep: 1,
        profile: 0x0104,
        cluster: 0x0008,
        data: vec![0x11, 0x22],
        tx_options: 0xFF,
        ..ApsFrame::default()
    };
    r.send(0x0101, f, 0).await;
    r.inject(0x8A10, &[0x01, 0xE9]).await;
    r.print("step send-ieee");
    let f = ApsFrame {
        dst: 0xFFFD,
        dst_ep: 2,
        cluster: 0x0001,
        data: vec![0x05, 0x34, 0x12, 0x00, 0x00],
        tx_options: 0x0F,
        ..ApsFrame::default()
    };
    r.send(0x0102, f, 0x0015_8D00_01AB_CDEF).await;
    r.inject(0x8A10, &[0x02, 0xA7]).await;
    r.print("step confirm-unknown");
    r.inject(0x8A10, &[0x42, 0x00]).await;
    r.inject(0x8A10, &[0x02]).await;
    r.print("step send-ack-fail");
    r.acks.push_back(2);
    r.send(0x0103, onoff(0x4321, &[0x01]), 0).await;
    r.pending();
    r.print("step send-too-long");
    r.send(0x0104, zdo(0x4321, 0, &[0xAB; 201]), 0).await;
    r.send(0x0105, zdo(0x4321, 0, &[0xAB; 200]), 0).await;
    r.inject(0x8A10, &[0x04, 0x00]).await;
    r.print("step send-self-nonzdo");
    r.send(0x0106, onoff(0x0000, &[0x01]), 0).await;
    let f = ApsFrame {
        dst_ep: 0,
        profile: 0,
        data: vec![],
        ..onoff(0x0000, &[])
    };
    r.send(0x0107, f, 0).await;

    r.print("step zdo-nwk-addr");
    let by_ieee = [0x04, 0x03, 0x02, 0x01, 0x00, 0x4B, 0x12, 0x00];
    r.send(
        0x0200,
        zdo(0, 0x0000, &[&[0x10][..], &by_ieee, &[0, 0]].concat()),
        0,
    )
    .await;
    r.send(
        0x0201,
        zdo(
            0,
            0x0000,
            &[
                0x11, 0x05, 0x03, 0x02, 0x01, 0x00, 0x4B, 0x12, 0x00, 0x00, 0x00,
            ],
        ),
        0,
    )
    .await;
    r.send(0x0202, zdo(0, 0x0000, &[0x12, 0x04, 0x03]), 0).await;
    r.print("step zdo-nwk-addr-by-ieee");
    r.send(
        0x0203,
        zdo(0x1111, 0x0000, &[&[0x13][..], &by_ieee].concat()),
        IEEE,
    )
    .await;
    r.print("step zdo-ieee-addr");
    r.send(0x0210, zdo(0, 0x0001, &[0x20, 0x00, 0x00, 0x00, 0x00]), 0)
        .await;
    r.send(0x0211, zdo(0, 0x0001, &[0x21, 0x01, 0x00, 0x00, 0x00]), 0)
        .await;
    r.send(0x0212, zdo(0, 0x0001, &[0x22, 0x00]), 0).await;
    r.print("step zdo-node-desc");
    r.send(0x0220, zdo(0, 0x0002, &[0x30, 0x00, 0x00]), 0).await;
    r.print("step zdo-simple-desc");
    r.send(0x0230, zdo(0, 0x0004, &[0x40, 0x00, 0x00, 0x01]), 0)
        .await;
    r.send(0x0231, zdo(0, 0x0004, &[0x41, 0x00, 0x00, 0xF2]), 0)
        .await;
    r.send(0x0232, zdo(0, 0x0004, &[0x42, 0x00, 0x00, 0x09]), 0)
        .await;
    r.send(0x0233, zdo(0, 0x0004, &[0x43, 0x00, 0x00]), 0).await;
    r.print("step zdo-active-ep");
    r.send(0x0240, zdo(0, 0x0005, &[0x50, 0x00, 0x00]), 0).await;
    r.print("step zdo-permit-join");
    r.send(0x0250, zdo(0, 0x0036, &[0x60, 0x3C, 0x01]), 0).await;
    r.send(0x0251, zdo(0, 0x0036, &[0x61]), 0).await;
    r.print("step zdo-nwk-update");
    r.send(
        0x0260,
        zdo(0, 0x0038, &[0x70, 0x00, 0x00, 0x10, 0x00, 0xFE, 0x05]),
        0,
    )
    .await;
    r.send(0x0261, zdo(0, 0x0038, &[0x71, 0x00, 0x00, 0x10]), 0)
        .await;
    r.print("step zdo-unknown");
    r.send(0x0270, zdo(0, 0x0031, &[0x72, 0x00]), 0).await;
    r.send(0x0271, zdo(0, 0x8000, &[0x73]), 0).await;

    r.print("step scan");
    r.send(
        0x0300,
        zdo(0, 0x0038, &[0x80, 0xFF, 0xFF, 0xFF, 0xFF, 0x02]),
        0,
    )
    .await;
    r.print("step scan-busy");
    r.send(
        0x0301,
        zdo(0, 0x0038, &[0x81, 0x00, 0x08, 0x00, 0x00, 0x01]),
        0,
    )
    .await;
    r.send(0x0302, onoff(0x5555, &[0x01]), 0).await; // held behind the scan
    r.print("step scan-rsp");
    r.inject(0x8A06, &[0x03, 0x0B, 0x14, 0x0F, 0xC8, 0x1A, 0x05])
        .await;
    r.inject(0x8A10, &[0x05, 0x00]).await;
    r.print("step scan-rsp-unsolicited");
    r.inject(0x8A06, &[0x01, 0x0B, 0x14]).await;
    r.print("step scan-rsp-short");
    r.send(
        0x0310,
        zdo(0, 0x0038, &[0x82, 0x00, 0x08, 0x01, 0x00, 0x00]),
        0,
    )
    .await;
    r.inject(0x8A06, &[0x05, 0x0B, 0x14, 0x18]).await;
    r.print("step scan-rsp-empty");
    r.send(
        0x0311,
        zdo(0, 0x0038, &[0x83, 0x00, 0x08, 0x00, 0x00, 0x05]),
        0,
    )
    .await;
    r.inject(0x8A06, &[]).await;
    r.print("step scan-hold-timeout");
    r.send(
        0x0312,
        zdo(0, 0x0038, &[0x84, 0x00, 0x08, 0x00, 0x00, 0x03]),
        0,
    )
    .await;
    r.send(0x0313, onoff(0x5555, &[0x02]), 0).await;
    // One channel at exponent 3: 138 ms + 3 s.
    r.advance(3000).await;
    r.advance(137).await;
    r.advance(1).await;
    r.inject(0x8A10, &[0x06, 0x00]).await;
    r.inject(0x8A06, &[0x01, 0x0B, 0x30]).await;
    r.print("step scan-ack-fail");
    r.acks.push_back(2);
    r.send(
        0x0320,
        zdo(0, 0x0038, &[0x85, 0x00, 0x08, 0x00, 0x00, 0x01]),
        0,
    )
    .await;
    r.send(
        0x0321,
        zdo(0, 0x0038, &[0x86, 0x00, 0x08, 0x00, 0x00, 0x01]),
        0,
    )
    .await;
    r.inject(0x8A06, &[0x01, 0x0B, 0x31]).await;

    let ieee_src = [0xEF, 0xCD, 0xAB, 0x01, 0x00, 0x8D, 0x15, 0x00];
    r.print("step indication-nwk");
    let nwk_ind = [
        0x02, 0x00, 0x00, 0x01, 0x02, 0x34, 0x12, 0x01, 0x04, 0x01, 0x06, 0x00, 0xC8, 0xC4, 0x00,
        0x03, 0xAA, 0xBB, 0xCC,
    ];
    r.inject(0x8A11, &nwk_ind).await;
    r.print("step indication-ieee");
    let ieee_ind = [
        &[0x02, 0x00, 0x00, 0x00, 0x03][..],
        &ieee_src,
        &[
            0x00, 0x00, 0x00, 0x13, 0x80, 0xFF, 0x7F, 0x01, 0x02, 0x10, 0x20,
        ],
    ]
    .concat();
    r.inject(0x8A11, &ieee_ind).await;
    r.print("step indication-group");
    r.inject(
        0x8A11,
        &[
            0x01, 0x05, 0x00, 0xFF, 0x02, 0x78, 0x56, 0x02, 0x04, 0x01, 0x08, 0x00, 0x10, 0x80,
            0x00, 0x00,
        ],
    )
    .await;
    r.print("step indication-truncated");
    r.inject(0x8A11, &nwk_ind[..4]).await;
    r.inject(0x8A11, &nwk_ind[..6]).await;
    r.inject(0x8A11, &nwk_ind[..15]).await;
    let mut long = nwk_ind;
    long[15] = 4;
    r.inject(0x8A11, &long).await;
    let mut short_ieee = ieee_ind.clone();
    short_ieee[3] = 0x01;
    short_ieee.truncate(21);
    r.inject(0x8A11, &short_ieee).await;
    r.print("step events");
    r.inject(
        0x8A20,
        &[&[0x01, 0x34, 0x12][..], &ieee_src, &[0; 9]].concat(),
    )
    .await;
    r.inject(0x8A20, &[&[0x02][..], &ieee_src, &[0x01]].concat())
        .await;
    r.inject(0x8A20, &[0x03, 0x3C]).await;
    r.inject(0x8A20, &[0x03, 0x00]).await;
    r.inject(0x8A20, &[0x01]).await;
    r.inject(0x8A99, &[0x01, 0x02]).await;

    r.print("step endpoints");
    r.c.remove_endpoint(0xF2);
    r.settle().await;
    r.c.remove_endpoint(0x09);
    r.settle().await;
    r.acks.push_back(2);
    r.c.set_endpoint(&desc(3, 0x0104, 0x0100, 2, &[0x0000], &[]));
    r.settle().await;
    r.c.set_endpoint(&desc(0xF2, 0xA1E0, 0x0061, 0, &[], &[0x0021]));
    r.settle().await;
    r.print("step commands");
    r.c.permit_join(0);
    r.c.permit_join(254);
    r.c.factory_reset();
    r.c.change_channel(20, 3);
    r.settle().await;
    r.print("step form-refused");
    r.acks.push_back(3);
    r.form(NetworkConfig::default()).await;
    r.print("step form-superseded");
    r.form(NetworkConfig {
        channel: 11,
        ..NetworkConfig::default()
    })
    .await;
    r.form(NetworkConfig {
        channel: 25,
        pan: 0x0BAD,
        ..NetworkConfig::default()
    })
    .await;
    let ext2 = 0x0102_0304_0506_0708;
    r.inject(
        0x8A00,
        &state(2, 0, 25, 0x0BAD, ext2, IEEE, 0, 0, 0x20, 1, 7),
    )
    .await;
    r.print("step form-failed");
    r.form(NetworkConfig::default()).await;
    r.inject(
        0x8A00,
        &state(0, 0x0E, 25, 0xFFFF, 0, IEEE, 0xFFFE, 0, 0, 0, 0),
    )
    .await;
    r.inject(
        0x8A00,
        &state(2, 0, 25, 0x0BAD, ext2, IEEE, 0, 0, 0x20, 1, 9),
    )
    .await;

    r.print("step collision");
    r.quiet = true;
    for i in 0..257u32 {
        let f = ApsFrame {
            dst_ep: 1,
            profile: 0x0104,
            cluster: 0x0006,
            data: vec![i as u8],
            ..zdo(0x6666, 0, &[])
        };
        if !r.c.send(0x1000 + i, f, 0) {
            r.print(&format!("send failed {i}"));
        }
        r.settle().await;
    }
    r.quiet = false;
    r.pending();
    r.print("step sweep");
    for i in 0..12 {
        r.print(&format!("t+{}", (i + 1) * 5));
        r.advance(5000).await;
        if i == 5 {
            r.pending();
        }
    }
    r.pending();

    r.print("step boot");
    let f = ApsFrame {
        dst_ep: 1,
        profile: 0x0104,
        cluster: 0x0006,
        data: vec![0x01],
        ..zdo(0x7777, 0, &[])
    };
    r.send(0x2000, f.clone(), 0).await;
    r.send(0x2001, f.clone(), 0).await;
    r.form(NetworkConfig::default()).await;
    r.send(
        0x2002,
        zdo(0, 0x0038, &[0x90, 0x00, 0x08, 0x00, 0x00, 0x01]),
        0,
    )
    .await;
    r.send(0x2003, f, 0).await; // held
    r.inject(BOOT_IND, &[]).await;
    r.pending();
    r.print("step after-boot");
    r.send(0x2004, zdo(0, 0x0005, &[0x91, 0x00, 0x00]), 0).await;
    r.inject(
        0x8A00,
        &state(2, 0, 25, 0x0BAD, ext2, IEEE, 0, 0, 0x20, 1, 1033),
    )
    .await;
    r.send(0x2005, zdo(0, 0x0005, &[0x92, 0x00, 0x00]), 0).await;
    r.print("step sweep-scan");
    r.send(
        0x2006,
        zdo(0, 0x0038, &[0x93, 0x00, 0x08, 0x00, 0x00, 0x00]),
        0,
    )
    .await;
    for _ in 0..6 {
        r.advance(5000).await;
    }
    r.inject(0x8A06, &[0x01, 0x0B, 0x01]).await;
    for _ in 0..6 {
        r.advance(5000).await;
    }
    assert_golden(
        include_str!("../../tests/golden/coordinator.txt"),
        &r.rec.out,
    );
}

#[test]
fn descriptors_round_trip_and_reject_truncation() {
    let d = desc(1, 0x0104, 0x0005, 1, &[0x0000, 0x0006], &[0x0019]);
    let mut v = Vec::new();
    put_descriptor(&mut v, &d);
    assert_eq!(hex(&v), "0104010500010200000600011900");
    assert_eq!(get_descriptor(&v), Some(d));
    assert_eq!(
        get_descriptor(&[2, 0x04, 0x01, 0x00, 0x00, 0, 0, 0]),
        Some(desc(2, 0x0104, 0, 0, &[], &[]))
    );
    for n in 0..v.len() {
        assert_eq!(get_descriptor(&v[..n]), None, "{n} bytes");
    }
    // Trailing bytes are ignored, as in C++.
    v.push(0xEE);
    assert!(get_descriptor(&v).is_some());
}

#[tokio::test(start_paused = true)]
async fn scan_holds_other_frames_for_the_computed_time() {
    let (mut link, radio) = link_pair(&absent_lines());
    assert_eq!(link.next().await, LinkEvent::Boot);
    let mut r = Run {
        c: Coordinator::new(link),
        radio,
        rec: Rec::default(),
        acks: VecDeque::new(),
        quiet: false,
    };
    r.c.start(&mut r.rec);
    r.settle().await;
    r.inject(0x8A00, &state(2, 0, 15, 0x1A62, EXT, IEEE, 0, 0, 0, 0, 0))
        .await;
    r.rec.out.clear();
    // 16 channels at exponent 2: 16 * 76 ms + 3 s.
    r.send(1, zdo(0, 0x0038, &[0x80, 0x00, 0xF8, 0xFF, 0x07, 0x02]), 0)
        .await;
    r.c.permit_join(1);
    r.advance(4215).await;
    assert!(!r.rec.out.contains("0a03"), "{}", r.rec.out);
    r.advance(1).await;
    assert!(r.rec.out.ends_with("send 0a03 01\n"), "{}", r.rec.out);
}
