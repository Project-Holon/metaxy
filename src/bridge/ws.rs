//! A small WebSocket client (RFC 6455): the upgrade, text messages,
//! ping and close. Over plain TCP (`ws`) or TLS (`wss`, on tokio-rustls
//! with the Mozilla roots).

use base64::Engine;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// A byte stream: TCP or TLS over TCP.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Stream = Box<dyn Io>;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn tls_connector() -> tokio_rustls::TlsConnector {
    use tokio_rustls::rustls;
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let conf = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(conf))
}

/// Open TCP to `host:port`, with TLS when `tls`. `check` sees the
/// address before the connection is made and may refuse it.
pub async fn open(
    tls: bool,
    host: &str,
    port: u16,
    check: &(dyn Fn(std::net::IpAddr) -> bool + Sync),
) -> Result<Stream, String> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("{host}: {e}"))?
        .collect();
    let addr = addrs
        .iter()
        .find(|a| check(a.ip().to_canonical()))
        .copied()
        .ok_or_else(|| format!("{host}: no address this relay may connect to"))?;
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("{host}:{port}: {e}"))?;
    let _ = tcp.set_nodelay(true);
    if !tls {
        return Ok(Box::new(tcp));
    }
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("{host}: {e}"))?;
    let s = tls_connector()
        .connect(name, tcp)
        .await
        .map_err(|e| format!("{host}:{port}: TLS: {e}"))?;
    Ok(Box::new(s))
}

/// Read an HTTP response head (up to the blank line). Returns the head and
/// the bytes read after it.
pub async fn read_head(s: &mut Stream, max: usize) -> Result<(String, Vec<u8>), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).into_owned();
            return Ok((head, buf[i + 4..].to_vec()));
        }
        if buf.len() > max {
            return Err("the answer's head is too long".into());
        }
        let n = s.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("closed before an answer".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// The status code of an HTTP head.
pub fn status(head: &str) -> u16 {
    head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

/// What the server sent.
#[derive(Debug, PartialEq, Eq)]
pub enum Msg {
    Text(String),
    Ping(Vec<u8>),
    /// The server closed the link.
    Close,
}

/// The reading side of a link.
pub struct Reader {
    rd: ReadHalf<Stream>,
    buf: Vec<u8>,
    max: usize,
}

/// The writing side of a link.
pub struct Writer {
    wr: WriteHalf<Stream>,
}

/// Do the WebSocket upgrade on `s` for `host` and `path`. `max` is the
/// largest message taken.
pub async fn upgrade(
    mut s: Stream,
    host: &str,
    path: &str,
    max: usize,
) -> Result<(Reader, Writer), String> {
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).expect("OS random source");
    let key = base64::engine::general_purpose::STANDARD.encode(raw);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: metaxy\r\n\r\n"
    );
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let (head, rest) = read_head(&mut s, 16 * 1024).await?;
    if status(&head) != 101 {
        return Err(format!(
            "no WebSocket upgrade (HTTP {})",
            head.lines().next().unwrap_or("")
        ));
    }
    let want = {
        let d = ring::digest::digest(
            &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
            format!("{key}{GUID}").as_bytes(),
        );
        base64::engine::general_purpose::STANDARD.encode(d.as_ref())
    };
    if header(&head, "sec-websocket-accept") != Some(want.as_str()) {
        return Err("the WebSocket upgrade does not match its key".into());
    }
    let (rd, wr) = tokio::io::split(s);
    Ok((Reader { rd, buf: rest, max }, Writer { wr }))
}

impl Reader {
    async fn need(&mut self, n: usize) -> Result<(), String> {
        let mut chunk = [0u8; 16384];
        while self.buf.len() < n {
            let k = self.rd.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if k == 0 {
                return Err("closed".into());
            }
            self.buf.extend_from_slice(&chunk[..k]);
        }
        Ok(())
    }

    /// The next message. Errors are a broken link.
    pub async fn next(&mut self) -> Result<Msg, String> {
        let mut msg: Vec<u8> = Vec::new();
        let mut in_text = false;
        loop {
            self.need(2).await?;
            let (b0, b1) = (self.buf[0], self.buf[1]);
            let fin = b0 & 0x80 != 0;
            let op = b0 & 0x0f;
            if b1 & 0x80 != 0 {
                return Err("the server masked a frame".into());
            }
            let (len, hl) = match b1 & 0x7f {
                126 => {
                    self.need(4).await?;
                    (u64::from(u16::from_be_bytes([self.buf[2], self.buf[3]])), 4)
                }
                127 => {
                    self.need(10).await?;
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&self.buf[2..10]);
                    (u64::from_be_bytes(b), 10)
                }
                n => (u64::from(n), 2),
            };
            if len as usize > self.max || msg.len() + len as usize > self.max {
                return Err(format!("a message over {} bytes", self.max));
            }
            let len = len as usize;
            self.need(hl + len).await?;
            let payload: Vec<u8> = self.buf[hl..hl + len].to_vec();
            self.buf.drain(..hl + len);
            match op {
                0x8 => return Ok(Msg::Close),
                0x9 => return Ok(Msg::Ping(payload)),
                0xA => continue,
                0x1 | 0x2 if !in_text => {
                    msg = payload;
                    in_text = true;
                }
                0x0 if in_text => msg.extend_from_slice(&payload),
                _ => return Err(format!("unexpected WebSocket opcode {op}")),
            }
            if fin {
                return String::from_utf8(msg)
                    .map(Msg::Text)
                    .map_err(|_| "a message that is not UTF-8".into());
            }
        }
    }
}

fn frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 14);
    f.push(0x80 | op);
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n <= 0xffff {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mut mask = [0u8; 4];
    getrandom::getrandom(&mut mask).expect("OS random source");
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

impl Writer {
    pub async fn text(&mut self, t: &str) -> Result<(), String> {
        self.wr
            .write_all(&frame(0x1, t.as_bytes()))
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn pong(&mut self, p: &[u8]) -> Result<(), String> {
        self.wr
            .write_all(&frame(0xA, p))
            .await
            .map_err(|e| e.to_string())
    }

    /// Send a close frame and end the stream.
    pub async fn close(mut self) {
        let _ = timeout(Duration::from_secs(2), async {
            let _ = self.wr.write_all(&frame(0x8, &1000u16.to_be_bytes())).await;
            let _ = self.wr.shutdown().await;
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_masked_with_the_right_lengths() {
        for n in [0usize, 125, 126, 65535, 65536] {
            let f = frame(1, &vec![b'a'; n]);
            assert_eq!(f[0], 0x81);
            assert_eq!(f[1] & 0x80, 0x80);
            let hl = match n {
                0..=125 => 2,
                126..=65535 => 4,
                _ => 10,
            };
            assert_eq!(f.len(), hl + 4 + n);
            let mask = &f[hl..hl + 4];
            assert!(f[hl + 4..]
                .iter()
                .enumerate()
                .all(|(i, b)| b ^ mask[i % 4] == b'a'));
        }
    }

    #[test]
    fn heads() {
        let h = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: abc";
        assert_eq!(status(h), 101);
        assert_eq!(header(h, "sec-websocket-accept"), Some("abc"));
        assert_eq!(header(h, "x"), None);
    }
}
