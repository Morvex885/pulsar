use std::collections::VecDeque;
use std::time::Duration;

use super::*;
use crate::test_server::{End, Reply, Server, data, ok, raw, redirect};

/// Reads return `pieces` one by one, then EOF; writes are recorded.
struct Script {
    pieces: VecDeque<io::Result<Vec<u8>>>,
    written: Vec<u8>,
}

impl AsyncRead for Script {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(front) = self.pieces.front_mut() else {
            return Poll::Ready(Ok(()));
        };
        let Ok(piece) = front else {
            let e = self
                .pieces
                .pop_front()
                .and_then(Result::err)
                .expect("an error");
            return Poll::Ready(Err(e));
        };
        let n = piece.len().min(buf.remaining());
        buf.put_slice(&piece[..n]);
        piece.drain(..n);
        if piece.is_empty() {
            self.pieces.pop_front();
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Script {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.written.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct Exchanged {
    r: HttpsResponse,
    next: Option<String>,
    progress: Vec<(usize, usize)>,
    request: String,
}

fn pieces(texts: &[&str]) -> Vec<io::Result<Vec<u8>>> {
    texts.iter().map(|t| Ok(t.as_bytes().to_vec())).collect()
}

async fn exchange_script(
    pieces: Vec<io::Result<Vec<u8>>>,
    max_bytes: usize,
    follow: bool,
) -> Exchanged {
    let u = Url {
        host: "example.com".into(),
        port: "8443".into(),
        path: "/a/b?c=d".into(),
    };
    let mut script = Script {
        pieces: pieces.into(),
        written: Vec::new(),
    };
    let mut calls = Vec::new();
    let mut record = |got: usize, total: usize| calls.push((got, total));
    let mut progress: Option<&mut dyn FnMut(usize, usize)> = Some(&mut record);
    let mut r = HttpsResponse::default();
    let next = exchange(
        &mut script,
        &u,
        &["User-Agent: test/1"],
        max_bytes,
        follow,
        &mut progress,
        &mut r,
    )
    .await;
    Exchanged {
        r,
        next,
        progress: calls,
        request: String::from_utf8(script.written).expect("ASCII request"),
    }
}

#[test]
fn urls() {
    let u = |host: &str, port: &str, path: &str| {
        Some(Url {
            host: host.into(),
            port: port.into(),
            path: path.into(),
        })
    };
    assert_eq!(
        parse_url("https://github.com/o/r/releases/latest"),
        u("github.com", "443", "/o/r/releases/latest")
    );
    assert_eq!(parse_url("https://h:8443"), u("h", "8443", "/"));
    assert_eq!(parse_url("https://h:8443/x?y=1"), u("h", "8443", "/x?y=1"));
    assert_eq!(parse_url("https://h:/x"), u("h", "", "/x"));
    assert_eq!(parse_url("https:///x"), None);
    assert_eq!(parse_url("https://:443/x"), None);
    assert_eq!(parse_url("http://h/x"), None);
    assert_eq!(parse_url("HTTPS://h/x"), None);
    assert_eq!(parse_url(""), None);

    let from = |port: &str| Url {
        host: "h".into(),
        port: port.into(),
        path: "/old".into(),
    };
    assert_eq!(
        resolve_location("/new?q".into(), &from("443")),
        "https://h/new?q"
    );
    assert_eq!(
        resolve_location("/new".into(), &from("8443")),
        "https://h:8443/new"
    );
    assert_eq!(
        resolve_location("https://cdn/x".into(), &from("8443")),
        "https://cdn/x"
    );
    // Not resolved (as in C++): a relative path without the slash and http:// are kept and
    // then fail as "not an https URL".
    assert_eq!(resolve_location("new".into(), &from("443")), "new");
}

#[test]
fn response_heads() {
    let head = |s: &str| parse_head(s.as_bytes());
    let h = head("HTTP/1.1 302 Found\r\nlocation:  \thttps://x/y \r\nContent-Length: 12\r\nX: y")
        .expect("parses");
    assert_eq!(
        h,
        Head {
            status: 302,
            location: "https://x/y ".into(),
            length: 12,
            chunked: false,
        }
    );
    let h = head("HTTP/2 200\r\nTRANSFER-ENCODING: gzip, Chunked\r\ncontent-length: abc")
        .expect("parses");
    assert_eq!((h.status, h.length, h.chunked), (200, 0, true));
    // The last header of a name wins.
    let h = head("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: identity")
        .expect("parses");
    assert!(!h.chunked);
    assert_eq!(
        head("HTTP/1.1 204").map(|h| (h.status, h.length)),
        Some((204, -1))
    );
    assert_eq!(head("HTTP/1.0   +301 Moved").map(|h| h.status), Some(301));
    assert_eq!(head("HTTP/1.1 OK"), None);
    assert_eq!(head("HTTP/1.1"), None);
    assert_eq!(head("HTTP/ 1.1 200").map(|h| h.status), Some(200));
    assert_eq!(head("ICY 200 OK"), None);
    assert_eq!(head(""), None);
    assert_eq!(atoll(b" -42x"), -42);
    assert_eq!(atoll(b"99999999999999999999999"), i64::MAX);
    assert_eq!(chunk_size(b"1a;ext=1\r\n"), 26);
    assert_eq!(chunk_size(b"0x1F\r\n"), 31);
    assert_eq!(chunk_size(b"\r\n10\r\n"), 16);
    assert_eq!(chunk_size(b"-1\r\n"), u64::MAX);
    assert_eq!(chunk_size(b"zz\r\n"), 0);
}

#[tokio::test]
async fn content_length_body() {
    let e = exchange_script(
        pieces(&[
            "HTTP/1.1 200 OK\r\nContent-Le",
            "ngth: 10\r\n\r\n0123",
            "456789extra",
        ]),
        100,
        true,
    )
    .await;
    assert_eq!(
        e.request,
        "GET /a/b?c=d HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\
         Accept-Encoding: identity\r\nUser-Agent: test/1\r\n\r\n"
    );
    assert_eq!((e.r.status, e.r.error.as_str()), (200, ""));
    assert_eq!(e.r.body, b"0123456789");
    assert_eq!(e.progress, [(4, 10), (15, 10)]);
    assert_eq!(e.next, None);

    let e = exchange_script(
        pieces(&["HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"]),
        100,
        true,
    )
    .await;
    assert_eq!((e.r.status, e.r.body.len()), (200, 0));
    assert_eq!(e.progress, [(0, 0)]);
}

#[tokio::test]
async fn body_limits_and_early_close() {
    let e = exchange_script(
        pieces(&["HTTP/1.1 200 OK\r\nContent-Length: 101\r\n\r\n"]),
        100,
        true,
    )
    .await;
    assert_eq!((e.r.status, e.r.error.as_str()), (0, "response too large"));

    let e = exchange_script(
        pieces(&["HTTP/1.1 200 OK\r\n\r\n", &"x".repeat(60), &"x".repeat(41)]),
        100,
        true,
    )
    .await;
    assert_eq!((e.r.status, e.r.error.as_str()), (0, "response too large"));

    let e = exchange_script(
        pieces(&["HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123"]),
        100,
        true,
    )
    .await;
    assert_eq!(
        (e.r.status, e.r.error.as_str(), e.r.body.as_slice()),
        (0, "connection closed in the body", &b"0123"[..])
    );

    // Without a length the close ends the body, also one without TLS close_notify.
    let mut p = pieces(&["HTTP/1.1 404 Not Found\r\n\r\nno", "pe"]);
    p.push(Err(io::ErrorKind::UnexpectedEof.into()));
    let e = exchange_script(p, 100, true).await;
    assert_eq!((e.r.status, e.r.body.as_slice()), (404, &b"nope"[..]));
    assert_eq!(e.progress, [(2, 0), (4, 0)]);

    let mut p = pieces(&["HTTP/1.1 200 OK\r\n\r\nab"]);
    p.push(Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "Resource temporarily unavailable",
    )));
    let e = exchange_script(p, 100, true).await;
    assert_eq!(
        (e.r.status, e.r.error.as_str()),
        (0, "read: Resource temporarily unavailable")
    );
    let mut p = pieces(&["HTTP/1.1 200 OK\r\n\r\nab"]);
    p.push(Err(io::Error::from_raw_os_error(104)));
    let e = exchange_script(p, 100, true).await;
    assert_eq!(e.r.error, "read: Connection reset by peer");
}

#[tokio::test]
async fn chunked_body() {
    let e = exchange_script(
        pieces(&[
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            "4;ext=1\r\nWi",
            "ki\r\n",
            "A\r\n0123456789\r\n0",
            "\r\n\r\n",
        ]),
        100,
        true,
    )
    .await;
    assert_eq!((e.r.status, e.r.error.as_str()), (200, ""));
    assert_eq!(e.r.body, b"Wiki0123456789");
    assert_eq!(e.progress, [(4, 0), (14, 0)]);

    let e = exchange_script(
        pieces(&["HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nabcde\r\n6\r\n"]),
        10,
        true,
    )
    .await;
    assert_eq!((e.r.status, e.r.error.as_str()), (0, "response too large"));
    assert_eq!(e.r.body, b"abcde");

    let e = exchange_script(
        pieces(&["HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab"]),
        10,
        true,
    )
    .await;
    assert_eq!(e.r.error, "connection closed in the body");
    let e = exchange_script(
        pieces(&["HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5"]),
        10,
        true,
    )
    .await;
    assert_eq!(e.r.error, "connection closed in the body");
}

#[tokio::test]
async fn header_errors() {
    let e = exchange_script(pieces(&["HTTP/1.1 200 OK\r\nServer: x\r\n"]), 100, true).await;
    assert_eq!(
        (e.r.status, e.r.error.as_str()),
        (0, "connection closed in the response header")
    );
    let big = format!("HTTP/1.1 200 OK\r\nX: {}", "y".repeat(70000));
    let e = exchange_script(pieces(&[&big, "\r\n\r\n"]), 100, true).await;
    assert_eq!(e.r.error, "connection closed in the response header");
    let e = exchange_script(pieces(&["SSH-2.0-OpenSSH\r\n\r\n"]), 100, true).await;
    assert_eq!((e.r.status, e.r.error.as_str()), (0, "bad status line"));
    let p = vec![Err(io::Error::from_raw_os_error(104))];
    let e = exchange_script(p, 100, true).await;
    assert_eq!(e.r.error, "read: Connection reset by peer");
}

#[tokio::test]
async fn redirects_followed_or_returned() {
    let reply =
        "HTTP/1.1 302 Found\r\nLocation: /releases/tag/v1.2.3\r\nContent-Length: 4\r\n\r\nbody";
    let e = exchange_script(pieces(&[reply]), 100, true).await;
    assert_eq!(
        e.next.as_deref(),
        Some("https://example.com:8443/releases/tag/v1.2.3")
    );
    assert_eq!((e.r.status, e.r.location.as_str()), (0, ""));
    assert!(e.progress.is_empty());

    let e = exchange_script(pieces(&[reply]), 100, false).await;
    assert_eq!(e.next, None);
    assert_eq!(
        (e.r.status, e.r.location.as_str(), e.r.body.as_slice()),
        (
            302,
            "https://example.com:8443/releases/tag/v1.2.3",
            &b"body"[..]
        )
    );

    // A 3xx without Location is a final response.
    let e = exchange_script(
        pieces(&["HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n\r\n"]),
        100,
        true,
    )
    .await;
    assert_eq!((e.next, e.r.status), (None, 304));
}

#[tokio::test(start_paused = true)]
async fn io_timeout_per_operation() {
    let (client, mut server) = tokio::io::duplex(4);
    let mut io = Timed::new(client);
    let start = Instant::now();
    let mut buf = [0u8; 8];
    let (n, ()) = tokio::join!(io.read(&mut buf), async {
        tokio::time::sleep(Duration::from_secs(19)).await;
        server.write_all(b"a").await.expect("write");
    });
    assert_eq!(n.expect("read"), 1);
    assert_eq!(start.elapsed(), Duration::from_secs(19));
    let e = io.read(&mut buf).await.expect_err("times out");
    assert_eq!(
        (e.kind(), e.to_string().as_str()),
        (io::ErrorKind::TimedOut, "Resource temporarily unavailable")
    );
    assert_eq!(start.elapsed(), Duration::from_secs(39));
    // The pipe takes 4 bytes; the writer then waits for a reader that never comes.
    let e = io.write_all(b"12345678").await.expect_err("times out");
    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    assert_eq!(start.elapsed(), Duration::from_secs(59));
    drop(server);
}

#[test]
fn sha256() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn ca_bundle_loading() {
    assert_eq!(
        Tls::load(Path::new("/nonexistent/ca.pem")).err().as_deref(),
        Some("cannot load /nonexistent/ca.pem")
    );
    // No certificate in it.
    let key = data("server.key");
    assert_eq!(
        Tls::load(&key).err(),
        Some(format!("cannot load {}", key.display()))
    );
    assert!(Tls::load(&data("ca.pem")).is_ok());
}

#[test]
fn certificate_words() {
    assert_eq!(
        certificate_text(&CertificateError::Expired),
        "certificate has expired"
    );
    assert_eq!(
        certificate_text(&CertificateError::NotValidYet),
        "certificate is not yet valid"
    );
    assert_eq!(
        certificate_text(&CertificateError::UnknownIssuer),
        "unable to get local issuer certificate"
    );
    assert_eq!(
        certificate_text(&CertificateError::NotValidForName),
        "hostname mismatch"
    );
}

fn tls() -> Tls {
    Tls::load(&data("ca.pem")).expect("test CA")
}

async fn local<F: Future>(f: F) -> F::Output {
    tokio::task::LocalSet::new().run_until(f).await
}

async fn get(url: &str, max_redirects: u32) -> HttpsResponse {
    https_get(
        &tls(),
        url,
        &["User-Agent: test/2"],
        1 << 20,
        None,
        None,
        max_redirects,
    )
    .await
}

#[tokio::test]
async fn over_tls() {
    local(async {
        let s = Server::start().await;
        let big: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        s.route("/big", ok(&big));
        let mut calls = Vec::new();
        let mut record = |got: usize, total: usize| calls.push((got, total));
        let r = https_get(
            &tls(),
            &s.url("/big"),
            &["User-Agent: test/2"],
            1 << 20,
            Some(&mut record),
            None,
            5,
        )
        .await;
        assert_eq!((r.status, r.error.as_str()), (200, ""));
        assert!(r.body == big);
        assert_eq!(calls.last(), Some(&(100_000, 100_000)));
        assert_eq!(
            s.requests.borrow()[0],
            "GET /big HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Accept-Encoding: identity\r\nUser-Agent: test/2\r\n\r\n"
        );

        s.route(
            "/chunked",
            raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"),
        );
        let r = get(&s.url("/chunked"), 5).await;
        assert_eq!((r.status, r.body.as_slice()), (200, &b"abc"[..]));

        // Read to the close, which comes without close_notify.
        s.route(
            "/unsized",
            Reply {
                bytes: b"HTTP/1.0 200 OK\r\n\r\nto the end".to_vec(),
                end: End::Drop,
            },
        );
        let r = get(&s.url("/unsized"), 5).await;
        assert_eq!((r.status, r.body.as_slice()), (200, &b"to the end"[..]));

        let r = get(&s.url("/missing"), 5).await;
        assert_eq!((r.status, r.body.as_slice()), (404, &b"not found"[..]));
    })
    .await;
}

#[tokio::test]
async fn sni_and_protocol_versions() {
    local(async {
        let s = Server::start().await;
        s.route("/", ok(b"x"));
        assert_eq!(get(&s.url("/"), 5).await.status, 200);
        assert_eq!(
            s.sessions.borrow().as_slice(),
            [(
                Some("localhost".to_owned()),
                Some(rustls::ProtocolVersion::TLSv1_3)
            )]
        );

        // A server that speaks TLS 1.2 only (the floor of the C++ client).
        let s12 = Server::start_with(&[&rustls::version::TLS12]).await;
        s12.route("/", ok(b"twelve"));
        let r = get(&s12.url("/"), 5).await;
        assert_eq!((r.status, r.body.as_slice()), (200, &b"twelve"[..]));
        assert_eq!(
            s12.sessions.borrow().as_slice(),
            [(
                Some("localhost".to_owned()),
                Some(rustls::ProtocolVersion::TLSv1_2)
            )]
        );
    })
    .await;
}

#[tokio::test]
async fn redirect_chain() {
    local(async {
        let s = Server::start().await;
        s.route("/a", redirect("/b"));
        s.route("/b", redirect(&s.url("/c")));
        s.route("/c", ok(b"done"));
        let r = get(&s.url("/a"), 5).await;
        assert_eq!(
            (r.status, r.body.as_slice(), r.location.as_str()),
            (200, &b"done"[..], "")
        );
        let r = get(&s.url("/a"), 0).await;
        assert_eq!(
            (r.status, r.body.as_slice(), r.location.as_str()),
            (302, &b"moved"[..], s.url("/b").as_str())
        );
        let r = get(&s.url("/a"), 1).await;
        assert_eq!((r.status, r.location.as_str()), (302, s.url("/c").as_str()));
        s.route("/plain", redirect("http://localhost/x"));
        let r = get(&s.url("/plain"), 5).await;
        assert_eq!(
            (r.status, r.error.as_str()),
            (0, "not an https URL: http://localhost/x")
        );
        // A redirect loop ends after max_redirects with the last Location.
        s.route("/loop", redirect("/loop"));
        let r = get(&s.url("/loop"), 3).await;
        assert_eq!(
            (r.status, r.location.as_str()),
            (302, s.url("/loop").as_str())
        );
        assert_eq!(
            s.requests
                .borrow()
                .iter()
                .filter(|h| h.starts_with("GET /loop "))
                .count(),
            4
        );
    })
    .await;
}

#[tokio::test]
async fn connection_and_certificate_errors() {
    local(async {
        let s = Server::start().await;
        s.route("/", ok(b"x"));
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = closed.local_addr().expect("addr").port();
        drop(closed);
        let r = get(&format!("https://127.0.0.1:{port}/"), 5).await;
        assert_eq!(
            (r.status, r.error.as_str()),
            (0, "connect 127.0.0.1: Connection refused")
        );

        // The certificate names DNS:localhost only.
        let r = get(&format!("https://127.0.0.1:{}/", s.port), 5).await;
        assert_eq!(r.error, "TLS 127.0.0.1: certificate: hostname mismatch");

        let other = Tls::load(&data("other-ca.pem")).expect("other CA");
        let r = https_get(&other, &s.url("/"), &[], 100, None, None, 5).await;
        assert_eq!(
            (r.status, r.error.as_str()),
            (
                0,
                "TLS localhost: certificate: unable to get local issuer certificate"
            )
        );

        let r = get(&format!("https://localhost:{}x/", s.port), 5).await;
        assert_eq!(
            r.error,
            "resolve localhost: Servname not supported for ai_socktype"
        );
    })
    .await;
}

#[tokio::test]
async fn cancel_stops_a_request() {
    local(async {
        let s = Server::start().await;
        s.route(
            "/hang",
            Reply {
                bytes: b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n01".to_vec(),
                end: End::Hang,
            },
        );
        let cancel = Cancel::default();
        let tls = tls();
        let url = s.url("/hang");
        let (r, ()) = tokio::join!(
            https_get(&tls, &url, &[], 100, None, Some(&cancel), 5),
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                cancel.cancel();
            }
        );
        assert_eq!((r.status, r.error.as_str()), (0, "cancelled"));
        // Already cancelled: nothing is sent.
        let n = s.requests.borrow().len();
        let r = https_get(&tls, &url, &[], 100, None, Some(&cancel), 5).await;
        assert_eq!(r.error, "cancelled");
        assert_eq!(s.requests.borrow().len(), n);
    })
    .await;
}
