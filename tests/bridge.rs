//! The Archipelago bridge against fake servers on local ports: one that
//! speaks Archipelago's WebSocket start, one that never upgrades, one that
//! upgrades but sends something else first, one that says nothing, and a
//! room-status page.

use base64::Engine;
use metaxy::world::client::{Client, Got};
use metaxy::{Config, Server};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ROOM_INFO: &str = r#"[{"cmd": "RoomInfo", "version": {"major": 0, "minor": 6, "build": 0, "class": "Version"}, "tags": ["AP"], "password": false, "seed_name": "fake"}]"#;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// RoomInfo, then a ping, then every text message echoed back; "big"
    /// asks for a 100 KB message, "huge" for one over the limit, "bye"
    /// closes the link.
    Good,
    NoUpgrade,
    WrongFirst,
    Silent,
}

async fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).await.unwrap_or(0) == 0 {
            break;
        }
        buf.push(b[0]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn server_frame(op: u8, p: &[u8]) -> Vec<u8> {
    let mut f = vec![0x80 | op];
    if p.len() < 126 {
        f.push(p.len() as u8);
    } else if p.len() <= 0xffff {
        f.push(126);
        f.extend_from_slice(&(p.len() as u16).to_be_bytes());
    } else {
        f.push(127);
        f.extend_from_slice(&(p.len() as u64).to_be_bytes());
    }
    f.extend_from_slice(p);
    f
}

/// Read one client frame (masked); returns opcode and payload.
async fn client_frame(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).await.ok()?;
    assert!(h[1] & 0x80 != 0, "client frames are masked");
    let mut len = u64::from(h[1] & 0x7f);
    if len == 126 {
        let mut b = [0u8; 2];
        s.read_exact(&mut b).await.ok()?;
        len = u64::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        s.read_exact(&mut b).await.ok()?;
        len = u64::from_be_bytes(b);
    }
    let mut mask = [0u8; 4];
    s.read_exact(&mut mask).await.ok()?;
    let mut p = vec![0u8; len as usize];
    s.read_exact(&mut p).await.ok()?;
    for (i, b) in p.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Some((h[0] & 0x0f, p))
}

/// A fake Archipelago server. Counts the pongs it got.
async fn fake(mode: Mode) -> (u16, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let pongs = Arc::new(AtomicUsize::new(0));
    let pc = pongs.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let pongs = pc.clone();
            tokio::spawn(async move {
                let head = read_head(&mut s).await;
                if mode == Mode::NoUpgrade || !head.starts_with("GET ") {
                    let _ = s
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                let key = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
                    .unwrap()
                    .trim()
                    .to_string();
                let d = ring::digest::digest(
                    &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
                    format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
                );
                let acc = base64::engine::general_purpose::STANDARD.encode(d.as_ref());
                let resp = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {acc}\r\n\r\n");
                s.write_all(resp.as_bytes()).await.unwrap();
                match mode {
                    Mode::Silent => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    Mode::WrongFirst => {
                        let _ = s
                            .write_all(&server_frame(1, br#"[{"cmd": "Print"}]"#))
                            .await;
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        return;
                    }
                    _ => {}
                }
                s.write_all(&server_frame(1, ROOM_INFO.as_bytes()))
                    .await
                    .unwrap();
                s.write_all(&server_frame(9, b"hi")).await.unwrap();
                while let Some((op, p)) = client_frame(&mut s).await {
                    match op {
                        0xA => {
                            pongs.fetch_add(1, Ordering::Relaxed);
                        }
                        0x8 => return,
                        0x1 => {
                            let t = String::from_utf8(p).unwrap();
                            let reply = match t.as_str() {
                                "bye" => {
                                    let _ =
                                        s.write_all(&server_frame(8, &1000u16.to_be_bytes())).await;
                                    return;
                                }
                                "big" => format!(
                                    "[{{\"cmd\": \"Big\", \"x\": \"{}\"}}]",
                                    "y".repeat(100_000)
                                ),
                                "huge" => "z".repeat(300_000),
                                _ => t,
                            };
                            // in two fragments when big, as servers may
                            if reply.len() > 1000 {
                                let (a, b) = reply.as_bytes().split_at(500);
                                let mut f1 = server_frame(1, a);
                                f1[0] &= 0x7f;
                                s.write_all(&f1).await.unwrap();
                                s.write_all(&server_frame(0, b)).await.unwrap();
                            } else {
                                s.write_all(&server_frame(1, reply.as_bytes()))
                                    .await
                                    .unwrap();
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    (port, pongs)
}

/// A fake site with a room page and its status (`last_port`).
async fn site(last_port: u16) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let head = read_head(&mut s).await;
                let body = if head.starts_with("GET /api/room_status/R1 ") {
                    format!("{{\"last_port\": {last_port}, \"players\": []}}")
                } else {
                    "<html>room</html>".into()
                };
                // the status in chunks, the page with a length
                let resp = if head.contains("/api/") {
                    format!(
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                        body.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

fn bcfg() -> Config {
    Config {
        bind: "127.0.0.1".into(),
        port: 0,
        conn_per_window: 10_000,
        bridge: true,
        bridge_private: true,
        bridge_setup_ms: 2000,
        bridge_msg_max: 200_000,
        ..Config::default()
    }
}

async fn start(c: Config) -> (Server, String) {
    let s = Server::start(c).await.unwrap();
    let a = s.addr.unwrap().to_string();
    (s, a)
}

async fn link(a: &str, to: &str) -> Client {
    let mut c = Client::connect(a).await.unwrap();
    let tok = c.token.clone();
    c.send('L', format!("eidos-net/2 {tok}\n{to}").as_bytes())
        .await
        .unwrap();
    c
}

async fn error(c: &mut Client, prefix: &str) {
    let mut g = c.read(Duration::from_secs(10)).await;
    while matches!(g, Got::Frame('T' | 'L', _)) {
        g = c.read(Duration::from_secs(10)).await;
    }
    match g {
        Got::Frame('E', p) => {
            let t = String::from_utf8(p).unwrap();
            assert!(t.starts_with(prefix), "got E {t:?}, wanted {prefix:?}");
        }
        g => panic!("wanted E {prefix}, got {g:?}"),
    }
}

async fn frame(c: &mut Client, kind: char) -> String {
    let p = c
        .until(kind, Duration::from_secs(10), |_, _| {})
        .await
        .unwrap();
    String::from_utf8(p).unwrap()
}

#[tokio::test]
async fn messages_pass_both_ways() {
    let (port, pongs) = fake(Mode::Good).await;
    let (_s, a) = start(bcfg()).await;
    let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
    assert_eq!(frame(&mut c, 'L').await, "ok");
    assert_eq!(frame(&mut c, 'T').await, ROOM_INFO);
    let msg = r#"[{"cmd": "Connect", "name": "Ash"}]"#;
    c.send('T', msg.as_bytes()).await.unwrap();
    assert_eq!(frame(&mut c, 'T').await, msg);
    // a big message from the server arrives whole, in F pieces
    c.send('T', b"big").await.unwrap();
    let t = frame(&mut c, 'T').await;
    assert_eq!(
        t,
        format!("[{{\"cmd\": \"Big\", \"x\": \"{}\"}}]", "y".repeat(100_000))
    );
    // a big message from the player, in F pieces, echoed back whole
    let big = format!(
        "[{{\"cmd\": \"Say\", \"text\": \"{}\"}}]",
        "q".repeat(50_000)
    );
    c.send_t(&big).await.unwrap();
    assert_eq!(frame(&mut c, 'T').await, big);
    assert!(pongs.load(Ordering::Relaxed) >= 1, "the ping got a pong");
    // the server closes: E ap-closed
    c.send('T', b"bye").await.unwrap();
    error(&mut c, "ap-closed").await;
}

#[tokio::test]
async fn room_links_look_up_the_port() {
    let (port, _) = fake(Mode::Good).await;
    let site = site(port).await;
    let (_s, a) = start(bcfg()).await;
    let mut c = link(&a, &format!("http://127.0.0.1:{site}/room/R1")).await;
    assert_eq!(frame(&mut c, 'L').await, "ok");
    assert_eq!(frame(&mut c, 'T').await, ROOM_INFO);
    let mut c = link(&a, &format!("http://127.0.0.1:{site}/room/NOPE")).await;
    error(&mut c, "ap-unreachable").await;
}

#[tokio::test]
async fn not_archipelago_is_refused() {
    let (_s, a) = start(bcfg()).await;
    for mode in [Mode::NoUpgrade, Mode::WrongFirst, Mode::Silent] {
        let (port, _) = fake(mode).await;
        let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
        // nothing the player sends passes before L ok
        c.send('T', b"[]").await.unwrap();
        error(&mut c, "ap-unreachable").await;
    }
    // nothing listening; host:port tries wss then ws
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = l.local_addr().unwrap().port();
    drop(l);
    let mut c = link(&a, &format!("127.0.0.1:{dead}")).await;
    error(&mut c, "ap-unreachable").await;
    let mut c = link(&a, "not a link").await;
    error(&mut c, "ap-unreachable").await;
}

#[tokio::test]
async fn host_port_falls_back_to_ws() {
    let (port, _) = fake(Mode::Good).await;
    let (_s, a) = start(bcfg()).await;
    let mut c = link(&a, &format!("127.0.0.1:{port}")).await;
    assert_eq!(frame(&mut c, 'L').await, "ok");
}

#[tokio::test]
async fn private_addresses_need_the_setting() {
    let (port, _) = fake(Mode::Good).await;
    let mut cfg = bcfg();
    cfg.bridge_private = false;
    let (_s, a) = start(cfg).await;
    let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
    error(&mut c, "ap-unreachable").await;
}

#[tokio::test]
async fn limits_and_states() {
    let (port, _) = fake(Mode::Good).await;
    // off by default
    let mut cfg = bcfg();
    cfg.bridge = false;
    let (_s, a) = start(cfg).await;
    let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
    error(&mut c, "unavailable").await;
    // links per address
    let mut cfg = bcfg();
    cfg.bridge_per_ip = 2;
    let (_s, a) = start(cfg).await;
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
        assert_eq!(frame(&mut c, 'L').await, "ok");
        held.push(c);
    }
    let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
    error(&mut c, "busy").await;
    // a closed link frees its place
    drop(held.pop());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
    assert_eq!(frame(&mut c, 'L').await, "ok");
    // T before L, L twice, I with a link, a message over the limit
    let mut t = Client::connect(&a).await.unwrap();
    t.send('T', b"[]").await.unwrap();
    error(&mut t, "bad-state T before L ok").await;
    let tok = c.token.clone();
    c.send(
        'L',
        format!("eidos-net/2 {tok}\nws://127.0.0.1:{port}").as_bytes(),
    )
    .await
    .unwrap();
    error(&mut c, "bad-state L after linking").await;
    drop(held);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
    assert_eq!(frame(&mut c, 'L').await, "ok");
    c.send('T', b"huge").await.unwrap();
    error(&mut c, "ap-closed").await;
    let mut c = link(&a, &format!("ws://127.0.0.1:{port}")).await;
    assert_eq!(frame(&mut c, 'L').await, "ok");
    let tok = c.token.clone();
    c.send('I', format!("eidos-net/2 {tok} ABCDEF Ash").as_bytes())
        .await
        .unwrap();
    error(&mut c, "bad-state I with a bridge link").await;
    // the version
    let mut c = Client::connect(&a).await.unwrap();
    let tok = c.token.clone();
    c.send(
        'L',
        format!("eidos-net/1 {tok}\nws://127.0.0.1:{port}").as_bytes(),
    )
    .await
    .unwrap();
    error(&mut c, "version").await;
}

#[tokio::test]
async fn a_relay_host_may_hold_a_link() {
    let (port, _) = fake(Mode::Good).await;
    let (_s, a) = start(bcfg()).await;
    let mut h = Client::connect(&a).await.unwrap();
    let tok = h.token.clone();
    h.send('H', format!("eidos-net/1 {tok} Host").as_bytes())
        .await
        .unwrap();
    frame(&mut h, 'W').await;
    h.send(
        'L',
        format!("eidos-net/2 {tok}\nws://127.0.0.1:{port}").as_bytes(),
    )
    .await
    .unwrap();
    assert_eq!(frame(&mut h, 'L').await, "ok");
    h.send('T', b"[1]").await.unwrap();
    assert_eq!(frame(&mut h, 'T').await, ROOM_INFO);
    assert_eq!(frame(&mut h, 'T').await, "[1]");
}
