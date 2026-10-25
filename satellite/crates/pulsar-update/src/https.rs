//! Minimal HTTPS GET on rustls (ring provider). Certificates are verified against a PEM CA bundle
//! (the firmware's, `CA_BUNDLE`), host names by rustls. The HTTP layer, its limits and its error
//! texts are those of the C++ client this replaces (OpenSSL through dlopen).

use std::cell::Cell;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::{CertificateError, ClientConfig, RootCertStore};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::{Instant, Sleep};
use tokio_rustls::TlsConnector;

/// The firmware's CA bundle (both models).
pub const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
/// Per address of the host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per read or write once connected (the C++ client's SO_RCVTIMEO / SO_SNDTIMEO).
const IO_TIMEOUT: Duration = Duration::from_secs(20);
/// The response header must end within this many buffered bytes.
const MAX_HEAD: usize = 65536;
const READ_CHUNK: usize = 16384;
const CLOSED_IN_BODY: &str = "connection closed in the body";
const TOO_LARGE: &str = "response too large";

#[derive(Debug, Default)]
pub struct HttpsResponse {
    /// HTTP status of the final response; 0 = transport/TLS error.
    pub status: i32,
    pub body: Vec<u8>,
    /// Location of a redirect that was not followed.
    pub location: String,
    /// Set when status == 0 (including a body over max_bytes).
    pub error: String,
}

/// TLS client settings: trust anchors of a PEM bundle; TLS 1.2 and 1.3 (rustls has nothing
/// older); SNI and host name verification.
#[derive(Clone)]
pub struct Tls {
    connector: TlsConnector,
}

impl Tls {
    /// Reads and parses the bundle (blocking: call it off the runtime thread in the daemon).
    pub fn load(ca_bundle: &Path) -> Result<Tls, String> {
        let fail = || format!("cannot load {}", ca_bundle.display());
        let pem = std::fs::read(ca_bundle).map_err(|_| fail())?;
        let mut roots = RootCertStore::empty();
        // Best effort, like OpenSSL's loader: bundles carry certificates webpki cannot parse.
        let (added, _) = roots
            .add_parsable_certificates(CertificateDer::pem_slice_iter(&pem).filter_map(Result::ok));
        if added == 0 {
            return Err(fail());
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS setup: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Tls {
            connector: TlsConnector::from(Arc::new(config)),
        })
    }
}

/// Stops a running `https_get` at its next await point; it then fails with "cancelled".
#[derive(Default)]
pub struct Cancel {
    set: Cell<bool>,
    notify: Notify,
}

impl Cancel {
    pub fn cancel(&self) {
        self.set.set(true);
        self.notify.notify_waiters();
    }

    pub fn is_set(&self) -> bool {
        self.set.get()
    }

    pub(crate) async fn cancelled(&self) {
        // Created before the check: notify_waiters reaches it even before its first poll.
        let notified = self.notify.notified();
        if !self.set.get() {
            notified.await;
        }
    }
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = String::with_capacity(64);
    for b in digest.as_ref() {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 15)]));
    }
    out
}

/// GET `url`, following up to `max_redirects` redirects (GitHub release assets redirect to another
/// host); a redirect beyond that is returned with its Location. `headers` are extra
/// "Name: value" lines. `progress(received, total)`: total is 0 when unknown.
pub async fn https_get(
    tls: &Tls,
    url: &str,
    headers: &[&str],
    max_bytes: usize,
    progress: Option<&mut dyn FnMut(usize, usize)>,
    cancel: Option<&Cancel>,
    max_redirects: u32,
) -> HttpsResponse {
    let get = get(tls, url, headers, max_bytes, progress, max_redirects);
    let Some(cancel) = cancel else {
        return get.await;
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => HttpsResponse { error: "cancelled".into(), ..Default::default() },
        r = get => r,
    }
}

async fn get(
    tls: &Tls,
    url: &str,
    headers: &[&str],
    max_bytes: usize,
    mut progress: Option<&mut dyn FnMut(usize, usize)>,
    max_redirects: u32,
) -> HttpsResponse {
    let mut r = HttpsResponse::default();
    let mut url = url.to_owned();
    let mut hop = 0;
    loop {
        let Some(u) = parse_url(&url) else {
            r.error = format!("not an https URL: {url}");
            return r;
        };
        let io = match connect(tls, &u).await {
            Ok(io) => io,
            Err(e) => {
                r.error = e;
                return r;
            }
        };
        let follow = hop < max_redirects;
        match exchange(io, &u, headers, max_bytes, follow, &mut progress, &mut r).await {
            Some(next) => {
                url = next;
                hop += 1;
            }
            None => return r,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Url {
    pub host: String,
    pub port: String,
    pub path: String,
}

/// `https://host[:port][/path]`; the path keeps any query.
pub(crate) fn parse_url(url: &str) -> Option<Url> {
    let rest = url.strip_prefix("https://")?;
    let (hostport, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, "/"),
    };
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, "443"));
    (!host.is_empty()).then(|| Url {
        host: host.into(),
        port: port.into(),
        path: path.into(),
    })
}

/// Location of a redirect from `u`: a path is relative to `u`'s origin.
pub(crate) fn resolve_location(location: String, u: &Url) -> String {
    if !location.starts_with('/') {
        return location;
    }
    let port = if u.port == "443" {
        String::new()
    } else {
        format!(":{}", u.port)
    };
    format!("https://{}{port}{location}", u.host)
}

/// An OS error as strerror(3) words, without std's " (os error N)".
pub(crate) fn os_text(e: &io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(code) => match text.strip_suffix(&format!(" (os error {code})")) {
            Some(t) => t.to_owned(),
            None => text,
        },
        None => text,
    }
}

async fn connect(
    tls: &Tls,
    u: &Url,
) -> Result<tokio_rustls::client::TlsStream<Timed<TcpStream>>, String> {
    let host = &u.host;
    // getaddrinfo's EAI_SERVICE for what the C++ client passed on as a service name.
    let port: u16 = u
        .port
        .parse()
        .map_err(|_| format!("resolve {host}: Servname not supported for ai_socktype"))?;
    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| {
            // std words the getaddrinfo failure as "failed to lookup address information: <gai_strerror>".
            let text = e.to_string();
            let gai = text
                .strip_prefix("failed to lookup address information: ")
                .unwrap_or(&text);
            format!("resolve {host}: {gai}")
        })?;
    let mut err = format!("connect {host}: no address");
    let mut tcp = None;
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                tcp = Some(s);
                break;
            }
            Ok(Err(e)) => err = format!("connect {host}: {}", os_text(&e)),
            Err(_) => err = format!("connect {host}: Connection timed out"),
        }
    }
    let tcp = tcp.ok_or(err)?;
    let name = ServerName::try_from(host.clone()).map_err(|e| format!("TLS {host}: {e}"))?;
    tls.connector
        .connect(name, Timed::new(tcp))
        .await
        .map_err(
            |e| match e.get_ref().and_then(|e| e.downcast_ref::<rustls::Error>()) {
                Some(rustls::Error::InvalidCertificate(c)) => {
                    format!("TLS {host}: certificate: {}", certificate_text(c))
                }
                Some(tls_err) => format!("TLS {host}: {tls_err}"),
                None => format!("TLS {host}: {}", os_text(&e)),
            },
        )
}

/// OpenSSL's words (X509_verify_cert_error_string) for the common failures, as HA showed them.
fn certificate_text(e: &CertificateError) -> String {
    match e {
        CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
            "certificate has expired".into()
        }
        CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
            "certificate is not yet valid".into()
        }
        CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
            "hostname mismatch".into()
        }
        CertificateError::UnknownIssuer => "unable to get local issuer certificate".into(),
        CertificateError::BadSignature => "certificate signature failure".into(),
        CertificateError::Revoked => "certificate revoked".into(),
        CertificateError::InvalidPurpose | CertificateError::InvalidPurposeContext { .. } => {
            "unsupported certificate purpose".into()
        }
        other => other.to_string(),
    }
}

/// One request on a connected stream. Fills `r` and returns the next URL if the response is a
/// redirect to follow.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    io: S,
    u: &Url,
    headers: &[&str],
    max_bytes: usize,
    follow: bool,
    progress: &mut Option<&mut dyn FnMut(usize, usize)>,
    r: &mut HttpsResponse,
) -> Option<String> {
    let mut c = Conn {
        io,
        buf: Vec::new(),
    };
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept-Encoding: identity\r\n",
        u.path, u.host
    );
    for h in headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    if let Err(e) = c.write_all(req.as_bytes()).await {
        r.error = format!("write: {}", os_text(&e));
        return None;
    }
    let head = match c.read_head().await {
        Ok(head) => head,
        Err(e) => {
            r.error = e;
            return None;
        }
    };
    let head = match parse_head(&head) {
        Some(head) => head,
        None => {
            r.error = "bad status line".into();
            return None;
        }
    };
    if (300..400).contains(&head.status) && !head.location.is_empty() {
        let location = resolve_location(head.location, u);
        if follow {
            return Some(location);
        }
        r.location = location;
    }
    r.status = head.status;
    if let Err(e) = c
        .read_body(head.length, head.chunked, max_bytes, &mut r.body, progress)
        .await
    {
        r.status = 0;
        r.error = e;
    }
    None
}

#[derive(Debug, PartialEq)]
pub(crate) struct Head {
    pub status: i32,
    pub location: String,
    /// Content-Length; -1 without one.
    pub length: i64,
    pub chunked: bool,
}

/// The response header without its final CRLF CRLF; None = bad status line.
pub(crate) fn parse_head(head: &[u8]) -> Option<Head> {
    let status = parse_status(head)?;
    let mut h = Head {
        status,
        location: String::new(),
        length: -1,
        chunked: false,
    };
    let Some(first) = find(head, b"\r\n") else {
        return Some(h);
    };
    let mut rest = &head[first + 2..];
    loop {
        let (line, next) = match find(rest, b"\r\n") {
            Some(end) => (&rest[..end], Some(&rest[end + 2..])),
            None => (rest, None),
        };
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            let name = line[..colon].to_ascii_lowercase();
            let mut value = &line[colon + 1..];
            while let [b' ' | b'\t', tail @ ..] = value {
                value = tail;
            }
            match name.as_slice() {
                b"location" => h.location = String::from_utf8_lossy(value).into_owned(),
                b"content-length" => h.length = atoll(value),
                b"transfer-encoding" => {
                    h.chunked = find(&value.to_ascii_lowercase(), b"chunked").is_some();
                }
                _ => {}
            }
        }
        match next {
            Some(n) => rest = n,
            None => return Some(h),
        }
    }
}

fn is_c_space(c: u8) -> bool {
    c == b' ' || (b'\t'..=b'\r').contains(&c)
}

/// sscanf(head, "HTTP/%*s %d").
fn parse_status(head: &[u8]) -> Option<i32> {
    let mut s = head.strip_prefix(b"HTTP/")?;
    while let [c, tail @ ..] = s
        && is_c_space(*c)
    {
        s = tail;
    }
    let version = s.iter().take_while(|&&c| !is_c_space(c)).count();
    if version == 0 {
        return None;
    }
    let n = strtol(&s[version..], 10)?;
    Some(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

/// atoll: 0 without digits.
fn atoll(s: &[u8]) -> i64 {
    strtol(s, 10).unwrap_or(0)
}

/// strtoul(s, 16): the size of a chunk; a negative number wraps like the C conversion.
fn chunk_size(s: &[u8]) -> u64 {
    match strtol(s, 16) {
        Some(n) if n < 0 => u64::MAX,
        Some(n) => n as u64,
        None => 0,
    }
}

/// C strtol family: leading whitespace, a sign, for base 16 an optional 0x, then digits; None
/// without digits. Saturates like strtoll.
fn strtol(s: &[u8], radix: u32) -> Option<i64> {
    let mut p = 0;
    while s.get(p).copied().is_some_and(is_c_space) {
        p += 1;
    }
    let neg = s.get(p) == Some(&b'-');
    if matches!(s.get(p), Some(b'-' | b'+')) {
        p += 1;
    }
    if radix == 16
        && s.get(p) == Some(&b'0')
        && matches!(s.get(p + 1), Some(b'x' | b'X'))
        && s.get(p + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        p += 2;
    }
    let start = p;
    let mut n: i64 = 0;
    while let Some(d) = s.get(p).and_then(|&c| char::from(c).to_digit(radix)) {
        n = n
            .saturating_mul(i64::from(radix))
            .saturating_add(i64::from(d));
        p += 1;
    }
    (p > start).then_some(if neg { -n } else { n })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// One connection with buffered reads.
struct Conn<S> {
    io: S,
    buf: Vec<u8>,
}

enum Fill {
    More,
    Closed,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conn<S> {
    async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.io.write_all(data).await?;
        self.io.flush().await
    }

    /// Reads more into `buf`.
    async fn fill(&mut self) -> Result<Fill, String> {
        let old = self.buf.len();
        self.buf.resize(old + READ_CHUNK, 0);
        let r = self.io.read(&mut self.buf[old..]).await;
        let n = *r.as_ref().unwrap_or(&0);
        self.buf.truncate(old + n);
        match r {
            Ok(0) => Ok(Fill::Closed),
            Ok(_) => Ok(Fill::More),
            // A close without TLS close_notify ends the stream like a clean close, as on the
            // C++ client's OpenSSL 1.1 (SSL_ERROR_SYSCALL with errno 0).
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(Fill::Closed),
            Err(e) => Err(format!("read: {}", os_text(&e))),
        }
    }

    /// Reads up to the end of the response header and returns the header without it.
    async fn read_head(&mut self) -> Result<Vec<u8>, String> {
        loop {
            if let Some(end) = find(&self.buf, b"\r\n\r\n") {
                let head = self.buf[..end].to_vec();
                self.buf.drain(..end + 4);
                return Ok(head);
            }
            if self.buf.len() > MAX_HEAD || matches!(self.fill().await?, Fill::Closed) {
                return Err("connection closed in the response header".into());
            }
        }
    }

    /// More body bytes; the stream ending counts as an error here.
    async fn more(&mut self) -> Result<(), String> {
        match self.fill().await? {
            Fill::More => Ok(()),
            Fill::Closed => Err(CLOSED_IN_BODY.into()),
        }
    }

    /// The body (Content-Length, chunked or up to the close) into `out`.
    async fn read_body(
        &mut self,
        length: i64,
        chunked: bool,
        max_bytes: usize,
        out: &mut Vec<u8>,
        progress: &mut Option<&mut dyn FnMut(usize, usize)>,
    ) -> Result<(), String> {
        if length > 0 && length as u64 > max_bytes as u64 {
            return Err(TOO_LARGE.into());
        }
        if chunked {
            loop {
                let eol = loop {
                    match find(&self.buf, b"\r\n") {
                        Some(eol) => break eol,
                        None => self.more().await?,
                    }
                };
                // strtoul reads from the line start on, across whitespace (as the C++ client).
                let n = chunk_size(&self.buf);
                self.buf.drain(..eol + 2);
                if n == 0 {
                    return Ok(());
                }
                if (out.len() as u64).saturating_add(n) > max_bytes as u64 {
                    return Err(TOO_LARGE.into());
                }
                let n = n as usize;
                while self.buf.len() < n + 2 {
                    self.more().await?;
                }
                out.extend_from_slice(&self.buf[..n]);
                self.buf.drain(..n + 2);
                if let Some(p) = progress {
                    p(out.len(), 0);
                }
            }
        }
        let total = if length > 0 {
            // Bounded by max_bytes above: one allocation for the whole body.
            out.reserve(length as usize);
            length as usize
        } else {
            0
        };
        loop {
            out.append(&mut self.buf);
            if out.len() > max_bytes {
                return Err(TOO_LARGE.into());
            }
            if let Some(p) = progress {
                p(out.len(), total);
            }
            if length >= 0 && out.len() as u64 >= length as u64 {
                out.truncate(length as usize);
                return Ok(());
            }
            match self.fill().await? {
                Fill::More => {}
                // Without a length the body ends with the connection.
                Fill::Closed if length < 0 => return Ok(()),
                Fill::Closed => return Err(CLOSED_IN_BODY.into()),
            }
        }
    }
}

/// Each read and each write (including flush / shutdown) waits at most `IO_TIMEOUT`, like the
/// SO_RCVTIMEO / SO_SNDTIMEO of the C++ client: a server that stalls fails the request.
struct Timed<S> {
    io: S,
    read: Deadline,
    write: Deadline,
}

impl<S> Timed<S> {
    fn new(io: S) -> Self {
        Timed {
            io,
            read: Deadline::default(),
            write: Deadline::default(),
        }
    }
}

#[derive(Default)]
struct Deadline {
    /// Armed while an operation is pending.
    sleep: Option<Pin<Box<Sleep>>>,
}

impl Deadline {
    fn check<T>(&mut self, cx: &mut Context<'_>, r: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if r.is_ready() {
            self.sleep = None;
            return r;
        }
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(Instant::now() + IO_TIMEOUT)));
        if sleep.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        self.sleep = None;
        // The C++ client's words for its timeout (strerror(EAGAIN)). Not ErrorKind::WouldBlock:
        // tokio-rustls takes that for "not ready" and would wait without a wakeup.
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Resource temporarily unavailable",
        )))
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Timed<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_read(cx, buf);
        this.read.check(cx, r)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Timed<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write(cx, buf);
        this.write.check(cx, r)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.write.check(cx, r)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_flush(cx);
        this.write.check(cx, r)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_shutdown(cx);
        this.write.check(cx, r)
    }
}

#[cfg(test)]
mod tests;
