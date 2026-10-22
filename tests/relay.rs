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
    c.expect_error("unknown-code no session has code ZZZZZZ")
        .await;
    let version = "version got \"eidos-net/2\", this relay speaks eidos-net/1";
    let mut c = Client::connect(a).await;
    c.send('J', format!("eidos-net/2 {TOK2} {code} G").as_bytes())
        .await;
    c.expect_error(version).await;
    let mut c = Client::connect(a).await;
    c.send('H', format!("eidos-net/2 {TOK2} G").as_bytes())
        .await;
    c.expect_error(version).await;
    let mut c = Client::connect(a).await;
    c.send('J', format!("eidos-net/1 NOTHEX {code} G").as_bytes())
        .await;
    c.expect_error("bad-frame J token must be 16 to 64 characters of 0-9 and a-f (got 6 bytes)")
        .await;
    let mut c = Client::connect(a).await;
    c.send('J', format!("eidos-net/1 {TOK2} {code}").as_bytes())
        .await;
    c.expect_error("bad-frame J has no name").await;
    let mut c = Client::connect(a).await;
    c.send('H', format!("eidos-net/1 {TOK2} bad\u{7}name").as_bytes())
        .await;
    c.expect_error("bad-frame H name has a control character")
        .await;
    let mut c = Client::connect(a).await;
    c.send('H', b"eidos-net/1").await;
    c.expect_error("bad-frame H has no token").await;
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
    c.expect_error("full the session has 2 of 2 players").await;
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
    c.expect_error("full the session has 16 of 16 players")
        .await;
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
    c.expect_error("bad-state D before joining").await;
    let mut c = Client::connect(a).await;
    c.send('Q', b"2").await;
    c.expect_error("bad-state Q before joining").await;
    // a second H or J
    let (mut h, code) = Client::host(a, TOK1, "H").await;
    h.send('H', format!("eidos-net/1 {TOK1} H").as_bytes())
        .await;
    h.expect_error("bad-state H after joining").await;
    let (mut h, _) = Client::host(a, TOK2, "H").await;
    h.send('J', format!("eidos-net/1 {TOK2} {code} H").as_bytes())
        .await;
    h.expect_error("bad-state J after joining").await;
    for (raw, text) in [
        (&b"Z 0\n"[..], "bad-frame unknown frame type \"Z\""),
        (b"D 01\nx", "bad-frame length \"01\" has a leading zero"),
        (b"D -1\n", "bad-frame length \"-\" is not a number"),
        (b"D x\n", "bad-frame length \"x\" is not a number"),
        (b"D 12a\n", "bad-frame length \"12a\" is not a number"),
        (b"D1\n", "bad-frame no space after the frame type \"D\""),
        (
            b"D 12345678901234567890\n",
            "bad-frame header is longer than 16 bytes",
        ),
        (b"D \n", "bad-frame length is empty"),
        (b"\n", "bad-frame unknown frame type \"\\n\""),
    ] {
        let (mut h, _) = Client::host(a, TOK3, "H").await;
        h.send_raw(raw).await;
        h.expect_error(text).await;
    }
    // bad D bodies inside a session
    for (p, text) in [
        (&b"x"[..], "bad-frame D has no newline after the peer"),
        (b"1\nk", "bad-frame D has no newline after the key"),
        (b"1\n\nx", "bad-frame D key is empty"),
        (
            b"a\nk\nx",
            "bad-frame D peer is not a number of up to 4 digits",
        ),
        (b"", "bad-frame D has no newline after the peer"),
    ] {
        let (mut h, _) = Client::host(a, TOK3, "H").await;
        h.send('D', p).await;
        h.expect_error(text).await;
    }
    // oversize
    let (mut h, _) = Client::host(a, TOK3, "H").await;
    h.send_raw(b"D 4225\n").await;
    h.expect_error("too-big frame of 4225 bytes (limit 4224)")
        .await;
    let (mut h, _) = Client::host(a, TOK3, "H").await;
    h.send_raw(b"D 9000\n").await;
    h.expect_error("too-big frame of 9000 bytes (limit 4224)")
        .await;
    let (mut h, _) = Client::host(a, TOK3, "H").await;
    h.send('D', &data(0, "k", &vec![0u8; 4097])).await;
    h.expect_error("too-big D message of 4097 bytes (limit 4096)")
        .await;
    let (mut h, _) = Client::host(a, TOK3, "H").await;
    h.send('D', &data(0, &"k".repeat(97), b"x")).await;
    h.expect_error("bad-frame D key of 97 bytes (limit 96)")
        .await;
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
    c.expect_error(&format!("unknown-code no session has code {code}"))
        .await;
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
    k.expect_error("removed by the host").await;
    j.expect('X', "3").await;
    h.expect('X', "3").await;
    h.expect_none().await;
    // the removed player's number is free again
    let mut m = Client::join_raw(a, TOK4, &code, "M").await;
    m.expect('W', "3 M").await;
    // a malformed Q from the host is a bad frame
    h.send('Q', b"x").await;
    h.expect('P', "3 M").await;
    h.expect_error("bad-frame Q peer \"x\" is not a number of up to 4 digits")
        .await;
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
                let p = String::from_utf8(p).unwrap();
                assert!(
                    p.starts_with("rate over 4x the message rate for 0.5 s (")
                        && p.ends_with(" per second, limit 20)"),
                    "{p}"
                );
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
                let p = String::from_utf8(p).unwrap();
                assert!(
                    p.starts_with("too-slow ") && p.ends_with(" bytes queued (limit 100000)"),
                    "{p}"
                );
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
    c.expect_error("busy this address has 2 open sessions (limit 2)")
        .await;
}

#[tokio::test]
async fn session_total_is_limited() {
    let (_s, a) = start(Config {
        max_sessions: 1,
        ..cfg()
    })
    .await;
    let (_h, _) = Client::host(a, TOK1, "H").await;
    let mut c = Client::connect(a).await;
    c.send('H', format!("eidos-net/1 {TOK2} H").as_bytes())
        .await;
    c.expect_error("busy the relay holds 1 sessions (limit 1)")
        .await;
}

#[tokio::test]
async fn shutdown_sends_e_shutdown() {
    let (s, a) = start(cfg()).await;
    let (mut h, mut j, _) = pair(a).await;
    s.shutdown().await;
    h.expect_error("shutdown the relay is stopping").await;
    j.expect_error("shutdown the relay is stopping").await;
}

/// Copy `from` to `to` the way a TCP proxy on the internet can: what was
/// read is written again in small pieces of odd sizes, with short pauses,
/// so frame borders fall anywhere.
async fn reslice(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
) {
    let mut buf = vec![0u8; 65536];
    let sizes = [7usize, 1, 97, 1400, 3, 536, 13];
    let mut k = 0;
    while let Ok(n) = from.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let mut at = 0;
        while at < n {
            let end = (at + sizes[k % sizes.len()]).min(n);
            k += 1;
            if to.write_all(&buf[at..end]).await.is_err() {
                return;
            }
            at = end;
            if k % 16 == 0 {
                sleep(Duration::from_millis(1)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }
    }
    let _ = to.shutdown().await;
}

/// A proxy in front of the relay that re-slices both directions.
async fn slicing_proxy(relay: SocketAddr) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((c, _)) = l.accept().await {
            let u = TcpStream::connect(relay).await.unwrap();
            let _ = c.set_nodelay(true);
            let _ = u.set_nodelay(true);
            let (cr, cw) = c.into_split();
            let (ur, uw) = u.into_split();
            tokio::spawn(reslice(cr, uw));
            tokio::spawn(reslice(ur, cw));
        }
    });
    a
}

#[tokio::test]
async fn save_pieces_through_a_slicing_proxy_arrive_whole_and_in_order() {
    // The pattern of a client sending a 512 KB save: pieces of 3840 bytes
    // under a text header, the frame header and the payload in two writes,
    // several back to back. The bytes hold newlines, spaces, digits and
    // letters, so a reader that looked inside the payload would trip.
    let (_s, relay) = start(cfg()).await;
    let a = slicing_proxy(relay).await;
    let (mut h, mut j, _) = pair(a).await;
    let mut x: u32 = 12345;
    let save: Vec<u8> = (0..5 * 3840)
        .map(|i| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            match i % 7 {
                0 => b'\n',
                1 => b' ',
                2 => b'0' + (x >> 16) as u8 % 10,
                3 => b'D',
                _ => (x >> 16) as u8,
            }
        })
        .collect();
    let mut want = Vec::new();
    for (n, piece) in save.chunks(3840).enumerate() {
        let mut msg = format!("save 524288 {} 305419896\n", n * 3840).into_bytes();
        msg.extend_from_slice(piece);
        let payload = data(2, "eidos:port", &msg);
        h.send_raw(format!("D {}\n", payload.len()).as_bytes())
            .await;
        h.send_raw(&payload).await;
        want.push(data(1, "eidos:port", &msg));
    }
    for w in want {
        assert_eq!(j.read(Duration::from_secs(10)).await, Got::Frame('D', w));
    }
    j.expect_none().await;
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

/// Captured log output.
#[derive(Clone, Default)]
struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Run `f` with a debug-level logger on this thread and return the log.
fn capture(json: bool) -> (LogBuf, tracing::subscriber::DefaultGuard) {
    let buf = LogBuf::default();
    let b = buf.clone();
    let sub = metaxy::logging::subscriber("debug", json, move || b.clone()).unwrap();
    (buf, tracing::subscriber::set_default(sub))
}

const SECRET_KEY: &str = "SECRETKEY.mod.zq9";
const SECRET_BYTES: &str = "SECRETBYTES-4471";

/// A full session life with secrets in the messages.
async fn secret_session(a: SocketAddr) {
    let (mut h, mut j, code) = pair(a).await;
    // a third player drops
    let mut k = Client::join_raw(a, TOK4, &code, "Third").await;
    k.expect('W', "3 Third").await;
    k.expect('P', "1 Host").await;
    k.expect('P', "2 Guest").await;
    h.expect('P', "3 Third").await;
    j.expect('P', "3 Third").await;
    drop(k);
    h.expect('X', "3").await;
    j.expect('X', "3").await;
    for i in 0..30 {
        h.send(
            'D',
            &data(2, SECRET_KEY, format!("{SECRET_BYTES}{i}").as_bytes()),
        )
        .await;
    }
    sleep(Duration::from_millis(300)).await;
    while let Got::Frame(..) = j.read(Duration::from_millis(100)).await {}
    // refusals and a removal
    let mut c = Client::join_raw(a, TOK3, "ZZZZZZ", "Nobody").await;
    c.expect_error("unknown-code no session has code ZZZZZZ")
        .await;
    h.send('Q', b"2").await;
    j.expect('E', "removed by the host").await;
    h.expect('X', "2").await;
    let _ = code;
    h.send('B', b"").await;
    h.expect_closed().await;
    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn logs_say_what_happened_and_hold_no_secrets() {
    let (log, _g) = capture(false);
    let (_s, a) = start(Config {
        msg_rate: 5.0,
        msg_burst: 5.0,
        ..cfg()
    })
    .await;
    secret_session(a).await;
    let t = log.text();
    for secret in [SECRET_KEY, SECRET_BYTES, TOK1, TOK2, TOK3, TOK4] {
        assert!(!t.contains(secret), "log holds {secret}:\n{t}");
    }
    for want in [
        "connection opened",
        "mode=\"plain\"",
        "session created",
        "host_name=\"Host\"",
        "join",
        "kind=\"new\"",
        "name=\"Guest\"",
        "E sent",
        "reason=\"unknown-code\"",
        "detail=\"no session has code ZZZZZZ\"",
        "Q removed a player",
        "removed_name=\"Guest\"",
        "leave",
        "how=\"B\"",
        "how=\"drop\"",
        "connection closed",
        "cause=\"left (B)\"",
        "cause=\"removed by the host\"",
        "D frames dropped",
        "why=\"message rate\"",
        "peer traffic at disconnect",
        "d_dropped=",
        "queue_high=",
        "addr=127.0.0.1",
    ] {
        assert!(t.contains(want), "log lacks {want}:\n{t}");
    }
    // every line starts with an RFC 3339 UTC time with milliseconds
    for line in t.lines() {
        let b = line.as_bytes();
        assert!(
            b.len() > 24
                && b[4] == b'-'
                && b[7] == b'-'
                && b[10] == b'T'
                && b[13] == b':'
                && b[19] == b'.'
                && b[23] == b'Z',
            "{line}"
        );
    }
}

#[tokio::test]
async fn json_logs_are_json_lines_without_secrets() {
    let (log, _g) = capture(true);
    let (_s, a) = start(cfg()).await;
    secret_session(a).await;
    let t = log.text();
    assert!(!t.contains(SECRET_KEY) && !t.contains(SECRET_BYTES) && !t.contains(TOK1));
    assert!(t.lines().count() > 10);
    for line in t.lines() {
        assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
        assert!(line.contains("\"timestamp\":\"20") && line.contains("\"level\":\""));
    }
    assert!(t.contains("\"reason\":\"unknown-code\""));
}

#[tokio::test]
async fn refusals_and_closes_are_logged_with_their_cause() {
    let (log, _g) = capture(false);
    let (s, a) = start(Config {
        conn_per_window: 3,
        idle_ms: 400,
        handshake_ms: 300,
        sessions_per_ip: 1,
        ..cfg()
    })
    .await;
    // idle timeout
    let (_h, _code) = Client::host(a, TOK1, "Host").await;
    // busy
    let mut c = Client::connect(a).await;
    c.send('H', format!("eidos-net/1 {TOK2} H").as_bytes())
        .await;
    c.expect_error("busy this address has 1 open sessions (limit 1)")
        .await;
    // handshake timeout
    let mut c = Client::connect(a).await;
    c.expect_closed().await;
    sleep(Duration::from_millis(700)).await;
    // connection rate: four were used in this window
    let mut c = Client::connect(a).await;
    c.expect_closed().await;
    s.shutdown().await;
    sleep(Duration::from_millis(100)).await;
    let t = log.text();
    for want in [
        "cause=\"idle timeout (",
        "cause=\"handshake timeout (",
        "cause=\"busy\"",
        "connection refused",
        "reason=\"connection rate\"",
        "count=3",
        "window_s=60",
        "listening",
    ] {
        // "listening" is logged by the binary, not the library
        if want == "listening" {
            continue;
        }
        assert!(t.contains(want), "log lacks {want}:\n{t}");
    }
}
