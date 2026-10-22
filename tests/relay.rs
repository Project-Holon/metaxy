//! Integration tests over real TCP on ephemeral ports.

use metaxy::{Config, Server};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Instant};

const TOK1: &str = "11111111111111111111111111111111";
const TOK2: &str = "22222222222222222222222222222222";
const TOK3: &str = "33333333333333333333333333333333";
const TOK4: &str = "44444444444444444444444444444444";

fn cfg() -> Config {
    Config {
        bind: "127.0.0.1".into(),
        port: 0,
        conn_per_window: 10_000,
        sessions_per_ip: 1000,
        ..Config::default()
    }
}

async fn start(c: Config) -> (Server, SocketAddr) {
    let s = Server::start(c).await.unwrap();
    let a = s.addr.unwrap();
    (s, a)
}

fn frame(kind: char, payload: &[u8]) -> Vec<u8> {
    let mut v = format!("{kind} {}\n", payload.len()).into_bytes();
    v.extend_from_slice(payload);
    v
}

struct Client {
    s: TcpStream,
    buf: Vec<u8>,
}

/// What a read gave.
#[derive(Debug, PartialEq)]
enum Got {
    Frame(char, Vec<u8>),
    Closed,
    Nothing,
}

impl Client {
    async fn connect(a: SocketAddr) -> Client {
        Client {
            s: TcpStream::connect(a).await.unwrap(),
            buf: Vec::new(),
        }
    }

    async fn send(&mut self, kind: char, payload: &[u8]) {
        self.s.write_all(&frame(kind, payload)).await.unwrap();
    }

    async fn send_raw(&mut self, b: &[u8]) {
        self.s.write_all(b).await.unwrap();
    }

    fn take(&mut self) -> Option<(char, Vec<u8>)> {
        let nl = self.buf.iter().position(|&b| b == b'\n')?;
        let head = std::str::from_utf8(&self.buf[..nl]).unwrap().to_string();
        let kind = head.chars().next().unwrap();
        let len: usize = head[2..].parse().unwrap();
        if self.buf.len() < nl + 1 + len {
            return None;
        }
        let p = self.buf[nl + 1..nl + 1 + len].to_vec();
        self.buf.drain(..nl + 1 + len);
        Some((kind, p))
    }

    async fn read(&mut self, wait: Duration) -> Got {
        let end = Instant::now() + wait;
        loop {
            if let Some((k, p)) = self.take() {
                return Got::Frame(k, p);
            }
            let mut tmp = vec![0u8; 65536];
            match timeout(
                end.saturating_duration_since(Instant::now()),
                self.s.read(&mut tmp),
            )
            .await
            {
                Err(_) => return Got::Nothing,
                Ok(Ok(0)) | Ok(Err(_)) => return Got::Closed,
                Ok(Ok(n)) => self.buf.extend_from_slice(&tmp[..n]),
            }
        }
    }

    async fn expect(&mut self, kind: char, payload: &str) {
        let g = self.read(Duration::from_secs(3)).await;
        assert_eq!(g, Got::Frame(kind, payload.as_bytes().to_vec()));
    }

    async fn expect_none(&mut self) {
        assert_eq!(self.read(Duration::from_millis(150)).await, Got::Nothing);
    }

    async fn expect_closed(&mut self) {
        assert_eq!(self.read(Duration::from_secs(3)).await, Got::Closed);
    }

    /// An error frame, then the close.
    async fn expect_error(&mut self, reason: &str) {
        self.expect('E', reason).await;
        self.expect_closed().await;
    }

    async fn host(a: SocketAddr, tok: &str, name: &str) -> (Client, String) {
        let mut c = Client::connect(a).await;
        c.send('H', format!("eidos-net/1 {tok} {name}").as_bytes())
            .await;
        let Got::Frame('C', code) = c.read(Duration::from_secs(3)).await else {
            panic!("no code");
        };
        let code = String::from_utf8(code).unwrap();
        c.expect('W', &format!("1 {name}")).await;
        (c, code)
    }

    async fn join_raw(a: SocketAddr, tok: &str, code: &str, name: &str) -> Client {
        let mut c = Client::connect(a).await;
        c.send('J', format!("eidos-net/1 {tok} {code} {name}").as_bytes())
            .await;
        c
    }
}

fn data(peer: u32, key: &str, bytes: &[u8]) -> Vec<u8> {
    let mut v = format!("{peer}\n{key}\n").into_bytes();
    v.extend_from_slice(bytes);
    v
}

/// A host and a joiner, handshakes done.
async fn pair(a: SocketAddr) -> (Client, Client, String) {
    let (mut h, code) = Client::host(a, TOK1, "Host").await;
    let mut j = Client::join_raw(a, TOK2, &code, "Guest").await;
    j.expect('W', "2 Guest").await;
    j.expect('P', "1 Host").await;
    h.expect('P', "2 Guest").await;
    (h, j, code)
}

#[tokio::test]
async fn host_gets_code_then_welcome() {
    let (_s, a) = start(cfg()).await;
    let (_h, code) = Client::host(a, TOK1, "Ann Lee").await;
    assert_eq!(code.len(), 6);
    assert!(code
        .bytes()
        .all(|b| b"23456789ABCDEFGHJKMNPQRSTUVWXYZ".contains(&b)));
}

#[tokio::test]
async fn codes_are_new_each_time() {
    let (_s, a) = start(cfg()).await;
    let (_h1, c1) = Client::host(a, TOK1, "A").await;
    let (_h2, c2) = Client::host(a, TOK1, "A").await;
    assert_ne!(c1, c2);
}

#[tokio::test]
async fn join_sequence_and_lowest_free_number() {
    let (_s, a) = start(cfg()).await;
    let (mut h, mut j, code) = pair(a).await;
    let mut k = Client::join_raw(a, TOK3, &code.to_lowercase(), "Cy Dee").await;
    k.expect('W', "3 Cy Dee").await;
    k.expect('P', "1 Host").await;
    k.expect('P', "2 Guest").await;
    h.expect('P', "3 Cy Dee").await;
    j.expect('P', "3 Cy Dee").await;
    // peer 2 leaves, the next joiner takes 2
    j.send('B', b"").await;
    j.expect_closed().await;
    h.expect('X', "2").await;
    k.expect('X', "2").await;
    let mut m = Client::join_raw(a, TOK4, &code, "Eve").await;
    m.expect('W', "2 Eve").await;
}

#[tokio::test]
async fn join_errors() {
    let (_s, a) = start(cfg()).await;
    let (_h, code) = Client::host(a, TOK1, "H").await;
    let mut c = Client::join_raw(a, TOK2, "ZZZZZZ", "G").await;
    c.expect_error("unknown-code").await;
    let mut c = Client::connect(a).await;
    c.send('J', format!("eidos-net/2 {TOK2} {code} G").as_bytes())
        .await;
    c.expect_error("version").await;
    let mut c = Client::connect(a).await;
    c.send('H', format!("eidos-net/2 {TOK2} G").as_bytes())
        .await;
    c.expect_error("version").await;
    let mut c = Client::connect(a).await;
    c.send('J', format!("eidos-net/1 NOTHEX {code} G").as_bytes())
        .await;
    c.expect_error("bad-frame").await;
    let mut c = Client::connect(a).await;
    c.send('J', format!("eidos-net/1 {TOK2} {code}").as_bytes())
        .await;
    c.expect_error("bad-frame").await;
    let mut c = Client::connect(a).await;
    c.send('H', format!("eidos-net/1 {TOK2} bad\u{7}name").as_bytes())
        .await;
    c.expect_error("bad-frame").await;
}

#[tokio::test]
async fn full_session() {
    let (_s, a) = start(Config {
        max_players: 2,
        ..cfg()
    })
    .await;
    let (_h, _j, code) = pair(a).await;
    let mut c = Client::join_raw(a, TOK3, &code, "X").await;
    c.expect_error("full").await;
}

#[tokio::test]
async fn sixteen_players() {
    let (_s, a) = start(cfg()).await;
    let (mut h, code) = Client::host(a, TOK1, "H").await;
    let mut others = Vec::new();
    for i in 2..=16u32 {
        let tok = format!("{i:032x}");
        let mut c = Client::join_raw(a, &tok, &code, "P").await;
        c.expect('W', &format!("{i} P")).await;
        others.push(c);
    }
    let mut c = Client::join_raw(a, TOK2, &code, "P").await;
    c.expect_error("full").await;
    h.expect('P', "2 P").await;
}

#[tokio::test]
async fn name_is_trimmed_and_may_have_spaces() {
    let (_s, a) = start(cfg()).await;
    let long = "é".repeat(40);
    let mut c = Client::connect(a).await;
    c.send('H', format!("eidos-net/1 {TOK1} {long}").as_bytes())
        .await;
    c.read(Duration::from_secs(3)).await;
    c.expect('W', &format!("1 {}", "é".repeat(32))).await;
}

#[tokio::test]
async fn forwarding_one_all_order_and_drops() {
    let (_s, a) = start(cfg()).await;
    let (mut h, mut j, code) = pair(a).await;
    let mut k = Client::join_raw(a, TOK3, &code, "K").await;
    k.expect('W', "3 K").await;
    k.expect('P', "1 Host").await;
    k.expect('P', "2 Guest").await;
    h.expect('P', "3 K").await;
    j.expect('P', "3 K").await;
    // to one
    j.send('D', &data(3, "mod.a", b"hello\nworld")).await;
    k.expect('D', "2\nmod.a\nhello\nworld").await;
    h.expect_none().await;
    // to all others, never back
    h.send('D', &data(0, "mod.b", &[0, 1, 2, 255])).await;
    let Got::Frame('D', p) = j.read(Duration::from_secs(3)).await else {
        panic!()
    };
    assert_eq!(p, b"1\nmod.b\n\x00\x01\x02\xff");
    assert!(matches!(
        k.read(Duration::from_secs(3)).await,
        Got::Frame('D', _)
    ));
    h.expect_none().await;
    // unknown peer, self and a number past 16: silent
    h.send('D', &data(9, "k", b"x")).await;
    h.send('D', &data(1, "k", b"x")).await;
    h.send('D', &data(17, "k", b"x")).await;
    h.send('D', &data(9999, "k", b"x")).await;
    h.expect_none().await;
    j.expect_none().await;
    // order
    for i in 0..50 {
        h.send('D', &data(2, "k", format!("{i}").as_bytes())).await;
    }
    for i in 0..50 {
        j.expect('D', &format!("1\nk\n{i}")).await;
    }
    // empty message
    j.send('D', &data(1, "k", b"")).await;
    h.expect('D', "2\nk\n").await;
}

#[tokio::test]
async fn bad_state_and_bad_frames() {
    let (_s, a) = start(cfg()).await;
    let mut c = Client::connect(a).await;
    c.send('D', &data(0, "k", b"x")).await;
    c.expect_error("bad-state").await;
    let mut c = Client::connect(a).await;
    c.send('Q', b"2").await;
    c.expect_error("bad-state").await;
    // a second H or J
    let (mut h, code) = Client::host(a, TOK1, "H").await;
    h.send('H', format!("eidos-net/1 {TOK1} H").as_bytes())
        .await;
    h.expect_error("bad-state").await;
    let (mut h, _) = Client::host(a, TOK2, "H").await;
    h.send('J', format!("eidos-net/1 {TOK2} {code} H").as_bytes())
        .await;
    h.expect_error("bad-state").await;
    for raw in [
        &b"Z 0\n"[..],
        b"D 01\nx",
        b"D -1\n",
        b"D x\n",
        b"D1\n",
        b"D 12345678901234567890\n",
        b"\n",
    ] {
        let (mut h, _) = Client::host(a, TOK3, "H").await;
        h.send_raw(raw).await;
        h.expect_error("bad-frame").await;
    }
    // bad D bodies inside a session
    for p in [&b"x"[..], b"1\nk", b"1\n\nx", b"a\nk\nx", b""] {
        let (mut h, _) = Client::host(a, TOK3, "H").await;
        h.send('D', p).await;
        h.expect_error("bad-frame").await;
    }
    // oversize
    let (mut h, _) = Client::host(a, TOK3, "H").await;
    h.send_raw(b"D 4225\n").await;
    h.expect_error("too-big").await;
    let (mut h, _) = Client::host(a, TOK3, "H").await;
    h.send('D', &data(0, "k", &vec![0u8; 4097])).await;
    h.expect_error("too-big").await;
    // a 0-length frame from a client is fine
    let (mut h, _) = Client::host(a, TOK3, "H").await;
    h.send('K', b"").await;
    h.expect_none().await;
}

#[tokio::test]
async fn max_size_message_passes() {
    let (_s, a) = start(cfg()).await;
    let (mut h, mut j, _) = pair(a).await;
    let key = "k".repeat(96);
    let body = vec![7u8; 4096];
    h.send('D', &data(2, &key, &body)).await;
    let Got::Frame('D', p) = j.read(Duration::from_secs(3)).await else {
        panic!()
    };
    assert_eq!(p.len(), 2 + 96 + 1 + 4096);
}

#[tokio::test]
async fn drop_gives_x_and_rejoin_keeps_number() {
    let (_s, a) = start(cfg()).await;
    let (mut h, j, code) = pair(a).await;
    drop(j);
    h.expect('X', "2").await;
    let mut m = Client::join_raw(a, TOK3, &code, "Three").await;
    m.expect('W', "3 Three").await;
    m.expect('P', "1 Host").await;
    h.expect('P', "3 Three").await;
    // peer 2 comes back with its token: number 2, sees the others
    let mut j = Client::join_raw(a, TOK2, &code, "Guest").await;
    j.expect('W', "2 Guest").await;
    j.expect('P', "1 Host").await;
    j.expect('P', "3 Three").await;
    h.expect('P', "2 Guest").await;
    m.expect('P', "2 Guest").await;
}

#[tokio::test]
async fn host_rejoins_as_peer_one() {
    let (_s, a) = start(cfg()).await;
    let (h, mut j, code) = pair(a).await;
    drop(h);
    j.expect('X', "1").await;
    let mut h = Client::join_raw(a, TOK1, &code, "Host").await;
    h.expect('W', "1 Host").await;
    h.expect('P', "2 Guest").await;
    j.expect('P', "1 Host").await;
    // a new joiner does not take number 1 while the host is away
    drop(h);
    j.expect('X', "1").await;
    let mut n = Client::join_raw(a, TOK3, &code, "N").await;
    n.expect('W', "3 N").await;
}

#[tokio::test]
async fn rejoin_over_a_live_connection_closes_the_old_one_without_x() {
    let (_s, a) = start(cfg()).await;
    let (mut h, mut j, code) = pair(a).await;
    let mut j2 = Client::join_raw(a, TOK2, &code, "Guest").await;
    j2.expect('W', "2 Guest").await;
    j2.expect('P', "1 Host").await;
    j.expect_closed().await;
    h.expect_none().await;
    // the new connection works
    j2.send('D', &data(1, "k", b"hi")).await;
    h.expect('D', "2\nk\nhi").await;
    // and when it leaves, X is sent once
    j2.send('B', b"").await;
    h.expect('X', "2").await;
    h.expect_none().await;
}

#[tokio::test]
async fn sessions_are_forgotten_after_the_expiry() {
    let (_s, a) = start(Config {
        session_expiry_ms: 600,
        ..cfg()
    })
    .await;
    let (mut h, j, code) = pair(a).await;
    drop(j);
    h.expect('X', "2").await;
    drop(h);
    sleep(Duration::from_millis(200)).await;
    // still in memory: the host can return
    let mut h = Client::join_raw(a, TOK1, &code, "Host").await;
    h.expect('W', "1 Host").await;
    h.expect_none().await;
    drop(h);
    // forgotten after the expiry
    sleep(Duration::from_millis(2200)).await;
    let mut c = Client::join_raw(a, TOK1, &code, "Host").await;
    c.expect_error("unknown-code").await;
}

#[tokio::test]
async fn session_survives_while_someone_is_connected() {
    let (_s, a) = start(Config {
        session_expiry_ms: 300,
        ..cfg()
    })
    .await;
    let (_h, code) = Client::host(a, TOK1, "Host").await;
    sleep(Duration::from_millis(1500)).await;
    let mut j = Client::join_raw(a, TOK2, &code, "G").await;
    j.expect('W', "2 G").await;
}

#[tokio::test]
async fn only_the_host_can_remove() {
    let (_s, a) = start(cfg()).await;
    let (mut h, mut j, code) = pair(a).await;
    let mut k = Client::join_raw(a, TOK3, &code, "K").await;
    k.expect('W', "3 K").await;
    k.expect('P', "1 Host").await;
    k.expect('P', "2 Guest").await;
    h.expect('P', "3 K").await;
    j.expect('P', "3 K").await;
    // from a non-host: ignored
    j.send('Q', b"3").await;
    k.expect_none().await;
    h.expect_none().await;
    // unknown target and self: ignored
    h.send('Q', b"9").await;
    h.send('Q', b"1").await;
    h.expect_none().await;
    // from the host
    h.send('Q', b"3").await;
    k.expect_error("removed").await;
    j.expect('X', "3").await;
    h.expect('X', "3").await;
    h.expect_none().await;
    // the removed player's number is free again
    let mut m = Client::join_raw(a, TOK4, &code, "M").await;
    m.expect('W', "3 M").await;
    // a malformed Q from the host is a bad frame
    h.send('Q', b"x").await;
    h.expect('P', "3 M").await;
    h.expect_error("bad-frame").await;
}

#[tokio::test]
async fn idle_connection_is_dropped_and_k_keeps_it() {
    let (_s, a) = start(Config {
        idle_ms: 600,
        ..cfg()
    })
    .await;
    let (mut h, mut j, _) = pair(a).await;
    let start = Instant::now();
    // j sends K every 200 ms for 1.8 s, h stays silent
    while start.elapsed() < Duration::from_millis(1800) {
        j.send('K', b"").await;
        sleep(Duration::from_millis(200)).await;
    }
    h.expect_closed().await;
    j.expect('X', "1").await;
    // j was alive the whole time and K was never answered
    j.expect_none().await;
}

#[tokio::test]
async fn handshake_timeout() {
    let (_s, a) = start(Config {
        handshake_ms: 300,
        ..cfg()
    })
    .await;
    let mut c = Client::connect(a).await;
    c.expect_closed().await;
    // K does not keep a handshake alive
    let mut c = Client::connect(a).await;
    for _ in 0..4 {
        c.send('K', b"").await;
        sleep(Duration::from_millis(100)).await;
    }
    c.expect_closed().await;
}

#[tokio::test]
async fn message_rate_burst_and_silent_drop() {
    let (_s, a) = start(Config {
        msg_rate: 10.0,
        msg_burst: 20.0,
        ..cfg()
    })
    .await;
    let (mut h, mut j, _) = pair(a).await;
    let mut all = Vec::new();
    for i in 0..60 {
        all.extend(frame('D', &data(2, "k", format!("{i}").as_bytes())));
    }
    h.send_raw(&all).await;
    let mut n = 0;
    while let Got::Frame('D', _) = j.read(Duration::from_millis(400)).await {
        n += 1;
    }
    assert!((20..=26).contains(&n), "{n} passed");
    // the connection is still open and works
    sleep(Duration::from_millis(500)).await;
    h.send('D', &data(2, "k", b"after")).await;
    j.expect('D', "1\nk\nafter").await;
}

#[tokio::test]
async fn byte_rate_drops_silently() {
    let (_s, a) = start(Config {
        byte_rate: 10_000.0,
        byte_burst: 20_000.0,
        ..cfg()
    })
    .await;
    let (mut h, mut j, _) = pair(a).await;
    let mut all = Vec::new();
    for _ in 0..20 {
        all.extend(frame('D', &data(2, "k", &[1u8; 2000])));
    }
    h.send_raw(&all).await;
    let mut n = 0;
    while let Got::Frame('D', _) = j.read(Duration::from_millis(400)).await {
        n += 1;
    }
    assert!((8..=12).contains(&n), "{n} passed");
}

#[tokio::test]
async fn sustained_excess_gets_e_rate() {
    let (_s, a) = start(Config {
        msg_rate: 20.0,
        msg_burst: 20.0,
        rate_window_ms: 100,
        rate_close_windows: 5,
        ..cfg()
    })
    .await;
    let (mut h, _j, _) = pair(a).await;
    // thousands of frames per second against a limit of 20 per second
    let batch: Vec<u8> = (0..50)
        .flat_map(|_| frame('D', &data(2, "k", b"x")))
        .collect();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        if h.s.write_all(&batch).await.is_err() {
            break;
        }
        sleep(Duration::from_millis(1)).await;
    }
    let mut closed = false;
    loop {
        match h.read(Duration::from_secs(2)).await {
            Got::Frame('E', p) => {
                assert_eq!(p, b"rate");
                closed = true;
            }
            Got::Frame(..) => {}
            Got::Closed | Got::Nothing => break,
        }
    }
    assert!(closed, "no E rate");
}

#[tokio::test]
async fn moderate_excess_is_not_closed() {
    // over the rate but under 4 times it: dropped, not closed
    let (_s, a) = start(Config {
        msg_rate: 100.0,
        msg_burst: 10.0,
        rate_window_ms: 100,
        rate_close_windows: 3,
        ..cfg()
    })
    .await;
    let (mut h, _j, _) = pair(a).await;
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1200) {
        h.send('D', &data(2, "k", b"x")).await;
        sleep(Duration::from_millis(5)).await; // about 150 per second
    }
    h.expect_none().await;
}

/// Send `frames` messages of 4096 bytes, `gap` apart, and count what the
/// other side gets.
async fn transfer(a: SocketAddr, frames: usize, gap: Duration) -> usize {
    let (mut h, mut j, _) = pair(a).await;
    let t = tokio::spawn(async move {
        let body = [9u8; 4096];
        for _ in 0..frames {
            h.send('D', &data(2, "save", &body)).await;
            if !gap.is_zero() {
                sleep(gap).await;
            }
        }
        h
    });
    let mut n = 0;
    while let Got::Frame('D', p) = j.read(Duration::from_millis(700)).await {
        assert_eq!(p.len(), 2 + 4 + 1 + 4096);
        n += 1;
    }
    let _h = t.await.unwrap();
    n
}

#[tokio::test]
async fn half_a_megabyte_at_full_speed_is_not_dropped() {
    let (_s, a) = start(cfg()).await;
    // A whole 512 KB save in 128 pieces, back to back: the message burst
    // is 256, so none is dropped.
    assert_eq!(transfer(a, 128, Duration::ZERO).await, 128);
}

#[tokio::test]
async fn a_512kb_save_in_128_pieces_is_not_dropped() {
    // The same save with a short pause between pieces.
    let (_s, a) = start(cfg()).await;
    assert_eq!(transfer(a, 128, Duration::from_millis(2)).await, 128);
}

#[tokio::test]
async fn slow_reader_gets_too_slow_and_is_dropped() {
    let (_s, a) = start(Config {
        queue_cap: 100_000,
        msg_rate: 1e9,
        msg_burst: 1e9,
        byte_rate: 1e12,
        byte_burst: 1e12,
        rate_factor: 1e9,
        ..cfg()
    })
    .await;
    let (mut h, mut j, _) = pair(a).await;
    let body = vec![5u8; 4096];
    let fr = frame('D', &data(2, "k", &body));
    // j never reads; h floods until the relay gives up on j
    let mut x_seen = false;
    'flood: for i in 0..40_000 {
        if h.s.write_all(&fr).await.is_err() {
            break;
        }
        if i % 100 == 0 {
            while let Got::Frame(k, p) = h.read(Duration::from_millis(1)).await {
                if k == 'X' {
                    assert_eq!(p, b"2");
                    x_seen = true;
                    break 'flood;
                }
            }
        }
    }
    if !x_seen {
        while let Got::Frame(k, _) = h.read(Duration::from_secs(5)).await {
            if k == 'X' {
                x_seen = true;
                break;
            }
        }
    }
    assert!(x_seen, "the slow peer was never dropped");
    // j now reads: some frames, then E too-slow, then the close
    let mut got_e = false;
    loop {
        match j.read(Duration::from_secs(5)).await {
            Got::Frame('E', p) => {
                assert_eq!(p, b"too-slow");
                got_e = true;
            }
            Got::Frame('D', p) => assert_eq!(p.len(), 2 + 1 + 1 + 4096),
            Got::Closed => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(got_e);
}

#[tokio::test]
async fn connections_per_ip_are_limited() {
    let (_s, a) = start(Config {
        conn_per_window: 3,
        conn_window_ms: 600,
        ..cfg()
    })
    .await;
    let mut keep = Vec::new();
    for _ in 0..3 {
        keep.push(Client::connect(a).await);
    }
    let mut c = Client::connect(a).await;
    c.expect_closed().await;
    // the window passes
    sleep(Duration::from_millis(800)).await;
    let (_h, _code) = Client::host(a, TOK1, "H").await;
}

#[tokio::test]
async fn sessions_per_ip_are_limited() {
    let (_s, a) = start(Config {
        sessions_per_ip: 2,
        ..cfg()
    })
    .await;
    let (_a1, _) = Client::host(a, TOK1, "H").await;
    let (_a2, _) = Client::host(a, TOK2, "H").await;
    let mut c = Client::connect(a).await;
    c.send('H', format!("eidos-net/1 {TOK3} H").as_bytes())
        .await;
    c.expect_error("busy").await;
}

#[tokio::test]
async fn shutdown_sends_e_shutdown() {
    let (s, a) = start(cfg()).await;
    let (mut h, mut j, _) = pair(a).await;
    s.shutdown().await;
    h.expect_error("shutdown").await;
    j.expect_error("shutdown").await;
}

#[tokio::test]
async fn partial_frames_across_writes() {
    let (_s, a) = start(cfg()).await;
    let (mut h, mut j, _) = pair(a).await;
    let f = frame('D', &data(2, "k", b"split me"));
    for b in f {
        h.send_raw(&[b]).await;
        sleep(Duration::from_millis(2)).await;
    }
    j.expect('D', "1\nk\nsplit me").await;
}

#[tokio::test]
async fn tls_listener() {
    use std::sync::Arc;
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.key_pair.serialize_pem()).unwrap();
    let s = Server::start(Config {
        plain: false,
        tls_cert: cert.to_str().unwrap().into(),
        tls_key: key.to_str().unwrap().into(),
        tls_port: 0,
        ..cfg()
    })
    .await
    .unwrap();
    assert!(s.addr.is_none());
    let mut roots = RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let conf = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&tokio_rustls::rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let conn = tokio_rustls::TlsConnector::from(Arc::new(conf));
    let tcp = TcpStream::connect(s.tls_addr.unwrap()).await.unwrap();
    let mut tls = conn
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    tls.write_all(&frame('H', format!("eidos-net/1 {TOK1} Tls").as_bytes()))
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let mut got = Vec::new();
    while !String::from_utf8_lossy(&got).contains("W 5\n1 Tls") {
        let n = timeout(Duration::from_secs(3), tls.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    assert!(got.starts_with(b"C 6\n"));
}
