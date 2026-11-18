//! Native TLS for `tripletrail serve` (#221): `--tls-cert` and `--tls-key` (PEM).
//!
//! The HTTP server listens on a loopback port of its own; this module accepts TLS on the public
//! address and relays each connection's plaintext to it, one thread per direction. The relay
//! records which client each of its loopback connections carries, so the server sees the
//! client's address (for limits, failed-key counting and trusted-proxy checks) and knows the
//! request came over HTTPS.
//!
//! The certificate and key are read again when either file changes (looked at, at most once a
//! second, when a connection arrives) or on SIGHUP; connections already open keep theirs.

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How long a client may take to finish the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the files' modification times are looked at.
const RELOAD_CHECK: Duration = Duration::from_secs(1);

/// Set by SIGHUP: read the certificate and key again.
static RELOAD: AtomicBool = AtomicBool::new(false);

pub(crate) struct Tls {
    cert: PathBuf,
    key: PathBuf,
    current: RwLock<Current>,
    /// The client behind each relay connection, by the relay's local (loopback) port.
    clients: Mutex<HashMap<u16, SocketAddr>>,
}

struct Current {
    config: Arc<ServerConfig>,
    modified: (Option<SystemTime>, Option<SystemTime>),
    checked: Instant,
}

impl Tls {
    pub(crate) fn load(cert: &Path, key: &Path) -> Result<Self, String> {
        let config = config(cert, key)?;
        install_sighup();
        Ok(Self {
            cert: cert.to_owned(),
            key: key.to_owned(),
            current: RwLock::new(Current {
                config,
                modified: (modified(cert), modified(key)),
                checked: Instant::now(),
            }),
            clients: Mutex::default(),
        })
    }

    /// The configuration for a new connection, read again if the files changed or SIGHUP came.
    fn config(&self) -> Arc<ServerConfig> {
        let forced = RELOAD.swap(false, Ordering::Relaxed);
        {
            let current = self.current.read().unwrap_or_else(|e| e.into_inner());
            if !forced && current.checked.elapsed() < RELOAD_CHECK {
                return Arc::clone(&current.config);
            }
        }
        let mut current = self.current.write().unwrap_or_else(|e| e.into_inner());
        current.checked = Instant::now();
        let now = (modified(&self.cert), modified(&self.key));
        if forced || now != current.modified {
            current.modified = now;
            match config(&self.cert, &self.key) {
                Ok(config) => {
                    current.config = config;
                    eprintln!(
                        "tripletrail: reloaded the TLS certificate {}",
                        self.cert.display()
                    );
                }
                Err(e) => eprintln!("tripletrail: keeping the previous TLS certificate: {e}"),
            }
        }
        Arc::clone(&current.config)
    }

    /// The client a request on the HTTP server's loopback port came from, if it came through
    /// the relay.
    pub(crate) fn client_of(&self, relay: SocketAddr) -> Option<SocketAddr> {
        if !relay.ip().is_loopback() {
            return None;
        }
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&relay.port())
            .copied()
    }

    /// Accepts TLS connections on `listener` and relays them to `inner`, on threads of its own.
    pub(crate) fn serve(self: &Arc<Self>, listener: TcpListener, inner: SocketAddr) {
        let tls = Arc::clone(self);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let tls = Arc::clone(&tls);
                thread::spawn(move || {
                    if let Err(e) = tls.relay(stream, inner) {
                        // Handshake failures from scanners and clients that go away are common;
                        // only say something when it is not a plain disconnect.
                        if !matches!(
                            e.kind(),
                            std::io::ErrorKind::UnexpectedEof
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::BrokenPipe
                        ) {
                            eprintln!("tripletrail: TLS connection: {e}");
                        }
                    }
                });
            }
        });
    }

    fn relay(&self, mut client: TcpStream, inner: SocketAddr) -> std::io::Result<()> {
        let peer = client.peer_addr()?;
        let _ = client.set_nodelay(true);
        let mut conn = rustls::ServerConnection::new(self.config()).map_err(other)?;
        client.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        while conn.is_handshaking() {
            conn.complete_io(&mut client)?;
        }
        client.set_read_timeout(None)?;

        let upstream = TcpStream::connect(inner)?;
        let _ = upstream.set_nodelay(true);
        let port = upstream.local_addr()?.port();
        let _registered = Registered::new(self, port, peer);

        // The client may send its request with the end of the handshake, so some of it can
        // already be decrypted, and TLS 1.3 session tickets may wait to be sent.
        let mut early = Vec::new();
        let mut plain = vec![0u8; 64 * 1024];
        let mut closed = drain(&mut conn, &mut plain, &mut early)?;
        while conn.wants_write() {
            conn.write_tls(&mut client)?;
        }
        if !early.is_empty() {
            (&upstream).write_all(&early)?;
        }

        let conn = Arc::new(Mutex::new(conn));
        // Server → client: plaintext from the HTTP server, encrypted to the client.
        let to_client = {
            let conn = Arc::clone(&conn);
            let mut upstream = upstream.try_clone()?;
            let mut client = client.try_clone()?;
            thread::spawn(move || -> std::io::Result<()> {
                let mut buf = vec![0u8; 64 * 1024];
                // Once the client is gone, the rest of the server's output is read and dropped:
                // the connection stays registered until the server has closed it, so a request
                // it still has buffered is never taken for one from loopback.
                let mut gone = false;
                loop {
                    let n = upstream.read(&mut buf)?;
                    if gone {
                        if n == 0 {
                            return Ok(());
                        }
                        continue;
                    }
                    let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
                    if let Err(e) = send(&mut conn, &mut client, &buf[..n]) {
                        drop(conn);
                        // Also ends the other direction's read, should it still wait.
                        let _ = client.shutdown(Shutdown::Both);
                        if n == 0 {
                            return Err(e);
                        }
                        gone = true;
                        continue;
                    }
                    if n == 0 {
                        let _ = client.shutdown(Shutdown::Both);
                        return Ok(());
                    }
                }
            })
        };
        // Client → server: records from the client, decrypted to the HTTP server.
        let result = (|| -> std::io::Result<()> {
            let mut upstream = upstream.try_clone()?;
            let mut raw = vec![0u8; 64 * 1024];
            loop {
                if closed {
                    return Ok(());
                }
                let n = client.read(&mut raw)?;
                if n == 0 {
                    return Ok(());
                }
                let mut records = &raw[..n];
                let mut out = Vec::new();
                {
                    let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
                    while !records.is_empty() {
                        conn.read_tls(&mut records)?;
                        conn.process_new_packets().map_err(other)?;
                        closed |= drain(&mut conn, &mut plain, &mut out)?;
                    }
                    while conn.wants_write() {
                        conn.write_tls(&mut client)?;
                    }
                }
                upstream.write_all(&out)?;
            }
        })();
        // The client is done sending: let the server finish its answer, then close.
        let _ = upstream.shutdown(Shutdown::Write);
        let _ = to_client.join();
        let _ = client.shutdown(Shutdown::Both);
        result
    }
}

/// Encrypts `data` to the client (a close notification when it is empty), in pieces that fit
/// rustls's plaintext buffer.
fn send(
    conn: &mut rustls::ServerConnection,
    client: &mut TcpStream,
    mut data: &[u8],
) -> std::io::Result<()> {
    if data.is_empty() {
        conn.send_close_notify();
    }
    loop {
        while conn.wants_write() {
            conn.write_tls(client)?;
        }
        if data.is_empty() {
            return Ok(());
        }
        let k = conn.writer().write(data)?;
        if k == 0 && !conn.wants_write() {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        data = &data[k..];
    }
}

/// Moves the plaintext `conn` has decrypted into `out`; true once the client has closed.
fn drain(
    conn: &mut rustls::ServerConnection,
    buf: &mut [u8],
    out: &mut Vec<u8>,
) -> std::io::Result<bool> {
    loop {
        match conn.reader().read(buf) {
            Ok(0) => return Ok(true),
            Ok(k) => out.extend_from_slice(&buf[..k]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) => return Err(e),
        }
    }
}

/// Keeps a relay connection's client recorded while it is open.
struct Registered<'a> {
    tls: &'a Tls,
    port: u16,
}

impl<'a> Registered<'a> {
    fn new(tls: &'a Tls, port: u16, peer: SocketAddr) -> Self {
        tls.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(port, peer);
        Self { tls, port }
    }
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.tls
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.port);
    }
}

fn other(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn config(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, String> {
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|i| i.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{}: no certificate in the file", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| format!("the TLS certificate and key do not match or are invalid: {e}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

#[cfg(unix)]
fn install_sighup() {
    extern "C" fn on_sighup(_: libc::c_int) {
        RELOAD.store(true, Ordering::Relaxed);
    }
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(
            libc::SIGHUP,
            on_sighup as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
}

#[cfg(not(unix))]
fn install_sighup() {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use std::os::fd::AsRawFd;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/tls-fixtures")
            .join(name)
    }

    /// A client that sends two pipelined requests and resets the connection, so the relay's
    /// write of the first answer fails, stays registered until the server closes its side:
    /// the second request, still buffered there, must not be taken for one from loopback.
    #[test]
    fn a_reset_client_stays_known_until_the_server_closes() {
        let tls = Arc::new(Tls::load(&fixture("server1.pem"), &fixture("server1.key")).unwrap());
        let inner = TcpListener::bind("127.0.0.1:0").unwrap();
        let public = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = public.local_addr().unwrap();
        tls.serve(public, inner.local_addr().unwrap());

        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_file_iter(fixture("ca1.pem")).unwrap() {
            roots.add(cert.unwrap()).unwrap();
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let mut conn = rustls::ClientConnection::new(
            Arc::new(config),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut socket = TcpStream::connect(address).unwrap();
        let mut stream = rustls::Stream::new(&mut conn, &mut socket);
        stream
            .write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        stream.flush().unwrap();

        // The "server" reads both requests, then the client resets.
        let (mut upstream, relay) = inner.accept().unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while !seen.windows(6).any(|w| w == b"GET /b") {
            let n = upstream.read(&mut buf).unwrap();
            assert!(n > 0);
            seen.extend_from_slice(&buf[..n]);
        }
        assert!(tls.client_of(relay).is_some());
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a valid socket and a linger struct of the size given.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&linger as *const libc::linger).cast(),
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            );
        }
        drop(socket);
        thread::sleep(Duration::from_millis(200));

        // The answer to the first request cannot reach the client; the relay keeps the
        // connection registered while the server still has it open.
        let answer = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        let _ = upstream.write_all(answer);
        thread::sleep(Duration::from_millis(200));
        let _ = upstream.write_all(answer);
        thread::sleep(Duration::from_millis(200));
        assert!(tls.client_of(relay).is_some());

        // Once the server closes, it is forgotten.
        drop(upstream);
        let deadline = Instant::now() + Duration::from_secs(5);
        while tls.client_of(relay).is_some() {
            assert!(Instant::now() < deadline, "still registered");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
