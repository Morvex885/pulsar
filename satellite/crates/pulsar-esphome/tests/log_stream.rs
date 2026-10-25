//! SubscribeLogs end to end through the installed logger and the public API. Alone in its
//! binary: the log sink is process-wide.

use std::time::Duration;

use pulsar_esphome::{ApiConfig, ApiEvent, DeviceIdentity, spawn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(p: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0;
    for shift in (0..70).step_by(7) {
        let b = *p.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

struct Client(TcpStream, Vec<u8>);

impl Client {
    async fn connect(port: u16) -> Self {
        Self(
            TcpStream::connect(("127.0.0.1", port)).await.unwrap(),
            Vec::new(),
        )
    }

    async fn send(&mut self, msg_type: u32, msg: &[u8]) {
        let mut f = vec![0];
        put_varint(&mut f, msg.len() as u64);
        put_varint(&mut f, u64::from(msg_type));
        f.extend_from_slice(msg);
        self.0.write_all(&f).await.unwrap();
    }

    async fn recv(&mut self) -> (u32, Vec<u8>) {
        loop {
            let mut q = 1;
            if self.1.len() > 1
                && let (Some(len), Some(t)) =
                    (get_varint(&self.1, &mut q), get_varint(&self.1, &mut q))
                && self.1.len() >= q + len as usize
            {
                let msg = self.1[q..q + len as usize].to_vec();
                self.1.drain(..q + len as usize);
                return (t as u32, msg);
            }
            self.1.reserve(4096);
            let n = timeout(Duration::from_secs(10), self.0.read_buf(&mut self.1))
                .await
                .expect("timed out")
                .unwrap();
            assert!(n > 0, "closed");
        }
    }

    async fn hello(&mut self) {
        self.send(1, b"\x0a\x04test").await;
        assert_eq!(self.recv().await.0, 2);
        assert_eq!(self.recv().await.0, 36);
    }

    async fn subscribe_logs(&mut self, level: u8) {
        self.send(28, &[0x08, level]).await;
        // Ping round trip: the subscription is in place.
        self.send(7, &[]).await;
        assert_eq!(self.recv().await.0, 8);
    }

    /// SubscribeLogsResponse lines until one ends with `text`; returns (level, line) of each.
    async fn logs_until(&mut self, text: &str) -> Vec<(u64, String)> {
        let mut lines = Vec::new();
        loop {
            let (t, m) = self.recv().await;
            assert_eq!(t, 29);
            // [0x08 level][0x1a len message]
            assert_eq!(m[0], 0x08);
            let mut q = 1;
            let level = get_varint(&m, &mut q).unwrap();
            assert_eq!(m[q], 0x1a);
            q += 1;
            let len = get_varint(&m, &mut q).unwrap() as usize;
            let line = String::from_utf8(m[q..q + len].to_vec()).unwrap();
            let done = line.ends_with(text);
            lines.push((level, line));
            if done {
                return lines;
            }
        }
    }
}

#[tokio::test]
async fn subscribed_clients_get_log_lines_at_their_level() {
    pulsar_core::init_logger();
    let port = std::net::TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let id = DeviceIdentity {
        name: "pulsar-test".into(),
        ..DeviceIdentity::default()
    };
    let cfg = ApiConfig {
        port,
        ..ApiConfig::default()
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let (h, mut rx) = spawn(id, cfg).unwrap();
            let mut debug = Client::connect(port).await;
            debug.hello().await;
            debug.subscribe_logs(5).await;
            let mut warn = Client::connect(port).await;
            warn.hello().await;
            warn.subscribe_logs(2).await;
            assert!(matches!(
                rx.recv().await,
                Some(ApiEvent::ClientConnected { .. })
            ));

            // A debug record reaches the DEBUG subscriber only.
            debug.send(200, &[]).await;
            let lines = debug
                .logs_until("api: ignoring message type 200 (0 bytes)")
                .await;
            let (level, line) = lines.last().unwrap();
            assert_eq!(*level, 5);
            assert!(line.starts_with("[D][commands.rs:"), "{line}");

            // A warning reaches both.
            let mut bad = Client::connect(port).await;
            bad.send(89, &[]).await;
            for (c, max) in [(&mut debug, 5), (&mut warn, 2)] {
                let lines = c.logs_until("api: message 89 before hello").await;
                let (level, line) = lines.last().unwrap();
                assert_eq!(*level, 2);
                assert!(line.starts_with("[W][server.rs:"), "{line}");
                assert!(lines.iter().all(|(l, _)| *l <= max), "{lines:?}");
            }
            drop(h);
        })
        .await;
}
