//! Listeners, connection tasks and the janitor.

use crate::codec::{self, encode};
use crate::config::Config;
use crate::outbox::{Ctl, Outbox};
use crate::registry::{log, Outcome, Registry};

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, timeout, Instant};

struct Shared {
    cfg: Arc<Config>,
    reg: Mutex<Registry>,
}

/// A running relay.
pub struct Server {
    /// Address of the plain listener, when it is on.
    pub addr: Option<SocketAddr>,
    /// Address of the TLS listener, when it is on.
    pub tls_addr: Option<SocketAddr>,
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

fn load_tls(cfg: &Config) -> Result<tokio_rustls::TlsAcceptor, String> {
    use std::fs::File;
    use std::io::BufReader;
    let certs = rustls_pemfile::certs(&mut BufReader::new(
        File::open(&cfg.tls_cert).map_err(|e| format!("{}: {e}", cfg.tls_cert))?,
    ))
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("{}: {e}", cfg.tls_cert))?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(
        File::open(&cfg.tls_key).map_err(|e| format!("{}: {e}", cfg.tls_key))?,
    ))
    .map_err(|e| format!("{}: {e}", cfg.tls_key))?
    .ok_or_else(|| format!("{}: no private key", cfg.tls_key))?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let conf = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&tokio_rustls::rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(conf)))
}

impl Server {
    /// Bind the listeners and start serving.
    pub async fn start(cfg: Config) -> Result<Server, String> {
        cfg.validate()?;
        let cfg = Arc::new(cfg);
        let reg = Registry::new(cfg.clone());
        let shared = Arc::new(Shared {
            cfg: cfg.clone(),
            reg: Mutex::new(reg),
        });
        let (stop, stop_rx) = watch::channel(false);
        let mut tasks = Vec::new();
        let mut addr = None;
        let mut tls_addr = None;

        if cfg.plain {
            let l = TcpListener::bind((cfg.bind.as_str(), cfg.port))
                .await
                .map_err(|e| format!("bind {}:{}: {e}", cfg.bind, cfg.port))?;
            addr = Some(l.local_addr().map_err(|e| e.to_string())?);
            tasks.push(tokio::spawn(accept_loop(
                l,
                None,
                shared.clone(),
                stop_rx.clone(),
            )));
        }
        if cfg.tls_enabled() {
            let acceptor = load_tls(&cfg)?;
            let l = TcpListener::bind((cfg.bind.as_str(), cfg.tls_port))
                .await
                .map_err(|e| format!("bind {}:{}: {e}", cfg.bind, cfg.tls_port))?;
            tls_addr = Some(l.local_addr().map_err(|e| e.to_string())?);
            tasks.push(tokio::spawn(accept_loop(
                l,
                Some(acceptor),
                shared.clone(),
                stop_rx.clone(),
            )));
        }
        tasks.push(tokio::spawn(janitor(shared.clone(), stop_rx)));
        Ok(Server {
            addr,
            tls_addr,
            shared,
            stop,
            tasks,
        })
    }

    /// Sessions kept right now.
    pub fn session_count(&self) -> usize {
        self.shared.reg.lock().unwrap().session_count()
    }

    /// Tell every client `E shutdown` and close.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
        self.shared.reg.lock().unwrap().shutdown_all();
        for _ in 0..300 {
            if self.shared.reg.lock().unwrap().conn_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

async fn janitor(shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let tick = Duration::from_millis((shared.cfg.session_expiry_ms / 10).clamp(10, 1000));
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            _ = tokio::time::sleep(tick) => {}
        }
        shared.reg.lock().unwrap().sweep();
    }
}

async fn accept_loop(
    l: TcpListener,
    tls: Option<tokio_rustls::TlsAcceptor>,
    shared: Arc<Shared>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let (sock, peer) = tokio::select! {
            _ = stop.changed() => return,
            r = l.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    log(&format!("accept failed: {e}"));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            }
        };
        let ip = peer.ip().to_canonical();
        let (out, rx) = Outbox::new();
        let id = shared.reg.lock().unwrap().admit(ip, out.clone());
        let Some(id) = id else {
            log(&format!("connection from {ip} refused: rate"));
            continue;
        };
        let shared = shared.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _ = sock.set_nodelay(true);
            log(&format!("connection {id} opened from {ip}"));
            match tls {
                None => run_conn(sock, id, ip, out, rx, shared).await,
                Some(acc) => match timeout(
                    Duration::from_millis(shared.cfg.handshake_ms),
                    acc.accept(sock),
                )
                .await
                {
                    Ok(Ok(s)) => run_conn(s, id, ip, out, rx, shared).await,
                    _ => {
                        out.kill(None);
                        shared.reg.lock().unwrap().disconnect(id);
                        log(&format!("connection {id} closed from {ip}: tls"));
                    }
                },
            }
        });
    }
}

async fn writer<W: AsyncWrite + Unpin>(
    mut wr: W,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    queued: Arc<std::sync::atomic::AtomicUsize>,
    ctl: Arc<Ctl>,
) {
    // `Some(true)`: told to stop and the stream is at a frame boundary.
    let mut stop: Option<bool> = None;
    while stop.is_none() {
        let f = tokio::select! {
            biased;
            _ = ctl.kill.notified() => { stop = Some(true); break; }
            m = rx.recv() => match m {
                None => break,
                Some(f) => f,
            }
        };
        let n = f.len();
        let write = async {
            wr.write_all(&f).await?;
            wr.flush().await
        };
        tokio::pin!(write);
        tokio::select! {
            biased;
            _ = ctl.kill.notified() => {
                // Finish the frame in progress, so the last frame is whole.
                let done = matches!(timeout(Duration::from_secs(2), &mut write).await, Ok(Ok(())));
                stop = Some(done);
            }
            r = &mut write => {
                queued.fetch_sub(n, Ordering::Relaxed);
                if r.is_err() { break; }
            }
        }
    }
    if stop == Some(true) {
        let last = ctl.last.lock().unwrap().take();
        if let Some(f) = last {
            let _ = timeout(Duration::from_secs(2), async {
                wr.write_all(&f).await?;
                wr.flush().await
            })
            .await;
        }
    }
    let _ = timeout(Duration::from_secs(2), wr.shutdown()).await;
    ctl.closed.notify_one();
}

async fn run_conn<S>(
    stream: S,
    id: u64,
    ip: IpAddr,
    out: Outbox,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    shared: Arc<Shared>,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let cfg = shared.cfg.clone();
    let (mut rd, wr) = tokio::io::split(stream);
    let wtask = tokio::spawn(writer(wr, rx, out.queued.clone(), out.ctl.clone()));
    let started = Instant::now();
    let mut last_frame = started;
    let mut in_session = false;
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let why: &str = 'conn: loop {
        // Use every whole frame already read.
        loop {
            match codec::parse(&buf, cfg.max_payload) {
                Ok(Some((frame, used))) => {
                    buf.drain(..used);
                    last_frame = Instant::now();
                    let outcome = shared.reg.lock().unwrap().on_frame(id, frame);
                    match outcome {
                        Outcome::Ok => {}
                        Outcome::Joined => in_session = true,
                        Outcome::Bye => break 'conn "bye",
                        Outcome::Error(reason) => {
                            log(&format!("error {reason} sent to {ip}"));
                            out.kill(Some(encode(b'E', reason.as_bytes())));
                            let _ =
                                timeout(Duration::from_secs(3), out.ctl.closed.notified()).await;
                            break 'conn reason;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log(&format!("error {} sent to {ip}", e.reason()));
                    out.kill(Some(encode(b'E', e.reason().as_bytes())));
                    let _ = timeout(Duration::from_secs(3), out.ctl.closed.notified()).await;
                    break 'conn e.reason();
                }
            }
        }
        let deadline = if in_session {
            last_frame + Duration::from_millis(cfg.idle_ms)
        } else {
            started + Duration::from_millis(cfg.handshake_ms)
        };
        tokio::select! {
            _ = sleep_until(deadline) => break 'conn if in_session { "idle" } else { "handshake" },
            _ = out.ctl.closed.notified() => break 'conn "closed",
            r = rd.read(&mut chunk) => match r {
                Ok(0) => break 'conn "eof",
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break 'conn "error",
            }
        }
    };
    out.kill(None);
    shared.reg.lock().unwrap().disconnect(id);
    let _ = wtask.await;
    if !matches!(why, "eof" | "error" | "bye") {
        // Read what the peer still sends, for a moment: closing a socket
        // with unread input resets it, and the peer would lose our `E`.
        let _ = timeout(Duration::from_secs(1), async {
            while matches!(rd.read(&mut chunk).await, Ok(n) if n > 0) {}
        })
        .await;
    }
    log(&format!("connection {id} closed from {ip}: {why}"));
}
