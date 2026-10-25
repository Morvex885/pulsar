//! Local HTTPS server for the client and updater tests: rustls with the test CA of tests/data
//! (`ca.pem` signed `server.pem` for DNS:localhost; `other-ca.pem` signed nothing here).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use rustls::server::Acceptor;
use rustls::{ProtocolVersion, ServerConfig, SupportedProtocolVersion};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::LazyConfigAcceptor;

pub fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
}

/// What the connection does after the response bytes.
#[derive(Clone, Copy)]
pub enum End {
    /// TLS close_notify, then close.
    Close,
    /// Close without close_notify.
    Drop,
    /// Keep the connection open without sending more.
    Hang,
}

#[derive(Clone)]
pub struct Reply {
    pub bytes: Vec<u8>,
    pub end: End,
}

pub fn ok(body: &[u8]) -> Reply {
    let mut bytes =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body);
    Reply {
        bytes,
        end: End::Close,
    }
}

pub fn redirect(location: &str) -> Reply {
    raw(&format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 5\r\n\r\nmoved"
    ))
}

pub fn raw(text: &str) -> Reply {
    Reply {
        bytes: text.as_bytes().to_vec(),
        end: End::Close,
    }
}

pub type Session = (Option<String>, Option<ProtocolVersion>);

pub struct Server {
    pub port: u16,
    /// Request heads in arrival order.
    pub requests: Rc<RefCell<Vec<String>>>,
    /// SNI and negotiated version of each completed handshake.
    pub sessions: Rc<RefCell<Vec<Session>>>,
    routes: Rc<RefCell<HashMap<String, Reply>>>,
}

impl Server {
    /// Serves TLS 1.2 and 1.3 on 127.0.0.1 from a task of the current LocalSet; unknown paths get
    /// a 404.
    pub async fn start() -> Server {
        Self::start_with(rustls::DEFAULT_VERSIONS).await
    }

    pub async fn start_with(versions: &[&'static SupportedProtocolVersion]) -> Server {
        let certs: Vec<_> = CertificateDer::pem_file_iter(data("server.pem"))
            .expect("server.pem")
            .collect::<Result<_, _>>()
            .expect("server.pem");
        let key = PrivateKeyDer::from_pem_file(data("server.key")).expect("server.key");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("server certificate");
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let server = Server {
            port,
            requests: Rc::default(),
            sessions: Rc::default(),
            routes: Rc::default(),
        };
        let (requests, sessions, routes) = (
            server.requests.clone(),
            server.sessions.clone(),
            server.routes.clone(),
        );
        tokio::task::spawn_local(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let (config, requests, sessions, routes) = (
                    config.clone(),
                    requests.clone(),
                    sessions.clone(),
                    routes.clone(),
                );
                tokio::task::spawn_local(async move {
                    let acceptor = LazyConfigAcceptor::new(Acceptor::default(), tcp);
                    let Ok(start) = acceptor.await else {
                        return;
                    };
                    let sni = start.client_hello().server_name().map(str::to_owned);
                    // Fails when the client rejects the certificate.
                    let Ok(mut tls) = start.into_stream(config).await else {
                        return;
                    };
                    sessions
                        .borrow_mut()
                        .push((sni, tls.get_ref().1.protocol_version()));
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                    requests.borrow_mut().push(head);
                    let reply = routes.borrow().get(&path).cloned();
                    let reply = reply.unwrap_or_else(|| {
                        raw("HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found")
                    });
                    if tls.write_all(&reply.bytes).await.is_err() || tls.flush().await.is_err() {
                        return;
                    }
                    match reply.end {
                        End::Close => {
                            let _ = tls.shutdown().await;
                        }
                        End::Drop => {}
                        End::Hang => std::future::pending::<()>().await,
                    }
                });
            }
        });
        server
    }

    pub fn route(&self, path: &str, reply: Reply) {
        self.routes.borrow_mut().insert(path.into(), reply);
    }

    pub fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.port)
    }
}
