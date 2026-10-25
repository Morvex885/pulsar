//! The Midi's radio as a Zigbee router in somebody else's network (Zigbee2MQTT, ZHA, ...): it runs
//! pulsar's router firmware (radio/router), which joins and leaves on command and otherwise
//! forwards on its own. Over zbhci: 0x0045 -> 0x8045 network info, 0x0002 network steering
//! (join), 0x0005 leave + factory new; unsolicited 0x8906 joined, 0x8920 `[event][status]` (1
//! started: status 0 = on a network; 2 join failed: BDB status; 3 left).

use log::{info, warn};
use pulsar_core::Timers;

use crate::bytes::get_be16;
use crate::link::{AckTag, Link, LinkEvent};

const STEER: u16 = 0x0002;
const FACTORY_NEW: u16 = 0x0005;
const NWK_INFO_REQ: u16 = 0x0045;
const NWK_INFO_RSP: u16 = 0x8045;
const JOINED: u16 = 0x8906;
const EVENT: u16 = 0x8920;
const EVENT_STARTED: u8 = 1;
const EVENT_JOIN_FAILED: u8 = 2;
const EVENT_LEFT: u8 = 3;
/// Steering scans every channel, then retries.
const JOIN_TIMEOUT_MS: u64 = 90_000;
/// After a leave the firmware erases its network and may restart; it is asked once it settled.
const AFTER_LEAVE_MS: u64 = 1000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// The radio answered with its network state.
    pub ready: bool,
    pub joined: bool,
    /// Network steering in progress.
    pub joining: bool,
    pub pan: u16,
    pub nwk: u16,
    pub channel: u8,
    /// Last failure, "" = none.
    pub error: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    Join,
    /// One per leave: each asks for the network info again.
    NwkInfo(u64),
}

/// What woke the router; hand it back to [`Router::handle`].
pub struct Wake(WakeKind);

enum WakeKind {
    Link(LinkEvent),
    Timer(Timer),
}

pub struct Router {
    link: Link,
    timers: Timers<Timer>,
    st: State,
    leaves: u64,
}

fn join_error(bdb_status: u8) -> String {
    match bdb_status {
        3 => "no open network found (allow joining on the coordinator)".into(),
        10 => "trust center key exchange failed".into(),
        _ => format!("join failed (BDB status {bdb_status})"),
    }
}

impl Router {
    /// `link`: open, the running firmware identified as pulsar's router. Events the link had
    /// already queued are the router's.
    pub fn new(link: Link) -> Router {
        Router {
            link,
            timers: Timers::new(),
            st: State::default(),
            leaves: 0,
        }
    }

    pub fn start(&mut self) {
        self.on_boot();
    }

    pub fn state(&self) -> &State {
        &self.st
    }

    /// Next thing to handle. Cancel-safe.
    pub async fn next(&mut self) -> Wake {
        tokio::select! {
            biased;
            ev = self.link.next() => Wake(WakeKind::Link(ev)),
            t = self.timers.next() => Wake(WakeKind::Timer(t)),
        }
    }

    /// true: the state changed (C++ `changed()`).
    pub fn handle(&mut self, w: Wake) -> bool {
        match w.0 {
            WakeKind::Link(LinkEvent::Frame { ty, payload }) => self.on_frame(ty, &payload),
            WakeKind::Link(LinkEvent::Ack {
                tag: AckTag::Steer,
                status,
            }) if status != 0 => {
                self.st.joining = false;
                self.st.error = "radio refused the join command".into();
                true
            }
            WakeKind::Link(LinkEvent::Ack {
                tag: AckTag::Leave, ..
            }) => {
                self.leaves += 1;
                self.timers.set(Timer::NwkInfo(self.leaves), AFTER_LEAVE_MS);
                false
            }
            WakeKind::Link(LinkEvent::Ack { .. }) => false,
            WakeKind::Link(LinkEvent::Boot) => {
                self.on_boot();
                false
            }
            WakeKind::Timer(Timer::Join) => {
                if !self.st.joining {
                    return false;
                }
                self.st.joining = false;
                self.st.error = "no network joined".into();
                true
            }
            WakeKind::Timer(Timer::NwkInfo(_)) => {
                self.link.send(NWK_INFO_REQ, &[], None);
                false
            }
        }
    }

    /// Joins the network that currently allows joining (permit join on its coordinator).
    /// true: the state changed.
    pub fn join(&mut self) -> bool {
        if !self.st.ready || self.st.joined || self.st.joining {
            return false;
        }
        info!("zigbee router: joining");
        self.st.joining = true;
        self.st.error.clear();
        self.link.send(STEER, &[], Some(AckTag::Steer));
        self.timers.set(Timer::Join, JOIN_TIMEOUT_MS);
        true
    }

    /// Announces the leave and forgets the network. Changes the state without reporting it,
    /// as the C++ router did.
    pub fn leave(&mut self) {
        if !self.st.ready {
            return;
        }
        info!("zigbee router: leaving the network");
        self.st.joining = false;
        self.st.error.clear();
        // The firmware broadcasts a Leave, erases its network and may restart.
        self.link.send(FACTORY_NEW, &[], Some(AckTag::Leave));
    }

    fn on_boot(&mut self) {
        self.st.ready = false;
        self.link.send(NWK_INFO_REQ, &[], None);
    }

    fn on_frame(&mut self, ty: u16, p: &[u8]) -> bool {
        match ty {
            // [type][capability][on a network][PAN BE][ext PAN 8][NWK BE][IEEE 8][channel];
            // 0x8906 the same without the channel
            NWK_INFO_RSP | JOINED => {
                if p.len() < 23 {
                    return false;
                }
                self.st.ready = true;
                self.st.joined = p[2] != 0;
                self.st.pan = get_be16(&p[3..]);
                self.st.nwk = get_be16(&p[13..]);
                if ty == NWK_INFO_RSP && p.len() >= 24 {
                    self.st.channel = p[23];
                }
                if self.st.joined {
                    self.st.joining = false;
                    self.st.error.clear();
                }
                if ty == JOINED {
                    info!(
                        "zigbee router: joined PAN 0x{:04x} as 0x{:04x}",
                        self.st.pan, self.st.nwk
                    );
                    // The channel; its reply publishes the new state.
                    self.link.send(NWK_INFO_REQ, &[], None);
                    return false;
                }
                true
            }
            EVENT => {
                if p.len() < 2 {
                    return false;
                }
                match p[0] {
                    EVENT_JOIN_FAILED => {
                        self.st.joining = false;
                        self.st.error = join_error(p[1]);
                        warn!("zigbee router: {}", self.st.error);
                    }
                    EVENT_LEFT => {
                        info!("zigbee router: left the network");
                        self.st.joined = false;
                    }
                    EVENT_STARTED => self.link.send(NWK_INFO_REQ, &[], None),
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::time::Duration;

    use super::*;
    use crate::link::BOOT_IND;
    use crate::test_util::{Radio, absent_lines, assert_golden, hex, link_pair, poll_once};

    /// Drives a router and the radio in lock-step, recording what the C++ harness printed.
    struct Run {
        router: Router,
        radio: Radio,
        acks: std::collections::VecDeque<u8>,
        out: String,
    }

    impl Run {
        fn changed(&mut self, changed: bool) {
            if !changed {
                return;
            }
            let s = &self.router.st;
            writeln!(
                self.out,
                "changed ready={} joined={} joining={} pan={:04x} nwk={:04x} ch={} error=\"{}\"",
                u8::from(s.ready),
                u8::from(s.joined),
                u8::from(s.joining),
                s.pan,
                s.nwk,
                s.channel,
                s.error
            )
            .unwrap();
        }

        fn step(&mut self) -> bool {
            if let Some(w) = poll_once(self.router.next()) {
                let c = self.router.handle(w);
                self.changed(c);
                return true;
            }
            if let Some((ty, p)) = self.radio.read_frame() {
                writeln!(self.out, "send {ty:04x} {}", hex(&p)).unwrap();
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

        fn step_name(&mut self, name: &str) {
            writeln!(self.out, "step {name}").unwrap();
        }

        async fn join(&mut self) {
            let c = self.router.join();
            self.changed(c);
            self.settle().await;
        }

        async fn leave(&mut self) {
            self.router.leave();
            self.settle().await;
        }
    }

    fn info(on: u8, pan: u16, nwk: u16, ch: Option<u8>) -> Vec<u8> {
        let mut v = vec![0x02, 0x8E, on];
        v.extend_from_slice(&pan.to_be_bytes());
        v.extend(0xA0..0xA8);
        v.extend_from_slice(&nwk.to_be_bytes());
        v.extend(0xB0..0xB8);
        v.extend(ch);
        v
    }

    /// The C++ router's hooks and frames for the same script (tests/golden/router.txt).
    #[tokio::test(start_paused = true)]
    async fn router_matches_cxx_golden() {
        let (mut link, radio) = link_pair(&absent_lines());
        assert_eq!(link.next().await, LinkEvent::Boot);
        let mut r = Run {
            router: Router::new(link),
            radio,
            acks: Default::default(),
            out: String::new(),
        };
        r.step_name("start");
        r.router.start();
        r.settle().await;
        r.step_name("join-not-ready");
        r.join().await;
        r.step_name("leave-not-ready");
        r.leave().await;
        r.step_name("info");
        r.inject(0x8045, &info(0, 0xFFFF, 0xFFFE, Some(11))).await;
        r.step_name("join");
        r.join().await;
        r.step_name("join-again");
        r.join().await;
        r.step_name("join-failed");
        r.inject(0x8920, &[0x02, 0x03]).await;
        r.inject(0x8920, &[0x02, 0x0A]).await;
        r.inject(0x8920, &[0x02, 0x07]).await;
        r.step_name("join-refused");
        r.acks.push_back(2);
        r.join().await;
        r.step_name("join-timeout");
        r.join().await;
        r.advance(89_999).await;
        r.advance(1).await;
        r.step_name("join-timeout-after-fail");
        r.join().await;
        r.inject(0x8920, &[0x02, 0x03]).await;
        r.advance(90_000).await;
        r.step_name("join-retimed");
        r.join().await;
        r.advance(50_000).await;
        r.inject(0x8920, &[0x02, 0x03]).await;
        r.join().await;
        r.advance(50_000).await;
        r.advance(40_000).await;
        r.step_name("joined");
        r.join().await;
        r.inject(0x8906, &info(1, 0x1A62, 0x4F21, None)).await;
        r.inject(0x8045, &info(1, 0x1A62, 0x4F21, Some(15))).await;
        r.advance(90_000).await;
        r.step_name("join-while-joined");
        r.join().await;
        r.step_name("left");
        r.inject(0x8920, &[0x03, 0x00]).await;
        r.step_name("started");
        r.inject(0x8920, &[0x01, 0x00]).await;
        r.inject(0x8045, &info(1, 0x1A62, 0x4F21, Some(15))).await;
        r.step_name("other-events");
        r.inject(0x8920, &[0x04, 0x00]).await;
        r.inject(0x8920, &[0x02]).await;
        r.step_name("short-info");
        r.inject(0x8045, &[0x02, 0x8E, 0x00]).await;
        r.inject(0x8045, &info(0, 0x1234, 0x5678, None)).await;
        r.step_name("leave");
        r.join().await;
        r.leave().await;
        r.advance(999).await;
        r.advance(1).await;
        r.step_name("leave-twice");
        r.leave().await;
        r.router.leave();
        r.acks.push_back(5);
        r.settle().await;
        r.advance(1000).await;
        r.step_name("boot");
        r.inject(BOOT_IND, &[]).await;
        r.step_name("leave-after-boot");
        r.leave().await;
        r.advance(1000).await;
        r.step_name("boot-ready");
        r.inject(0x8045, &info(1, 0x1A62, 0x4F21, Some(15))).await;
        assert_golden(include_str!("../tests/golden/router.txt"), &r.out);
    }
}
