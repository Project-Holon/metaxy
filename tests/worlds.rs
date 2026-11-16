//! World sessions over real TCP: each test runs a server on an ephemeral
//! port with its own world store, and real generations.

use metaxy::world::client::{locations, parse_items, Client, Got};
use metaxy::{Config, Server};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;

const SEC: Duration = Duration::from_secs(1);
const GEN: Duration = Duration::from_secs(60);

/// arkhipelagos' two-player test configuration.
const TWO: &str = r#"{"schema": 1, "seed": 4242,
 "options": {"goal": "ghetsis", "season_control": "vanilla"},
 "players": [{"name": "Ash", "version": "black"},
             {"name": "Misty", "version": "white", "options": {"modify_logic": ["require_flash"]}}]}"#;

fn players(n: usize, seed: u64) -> String {
    let ps: Vec<String> = (1..=n)
        .map(|i| {
            let v = if i % 2 == 0 { "white" } else { "black" };
            format!(r#"{{"name": "P{i}", "version": "{v}"}}"#)
        })
        .collect();
    format!(
        r#"{{"schema": 1, "seed": {seed}, "players": [{}]}}"#,
        ps.join(",")
    )
}

fn wcfg(dir: &Path) -> Config {
    Config {
        bind: "127.0.0.1".into(),
        port: 0,
        conn_per_window: 10_000,
        sessions_per_ip: 1000,
        world_store: dir.display().to_string(),
        gen_workers: 2,
        gen_per_hour: 1000,
        worlds_per_ip: 1000,
        gen_time_ms: 60_000,
        ..Config::default()
    }
}

async fn start(c: Config) -> (Server, String) {
    let s = Server::start(c).await.unwrap();
    let a: SocketAddr = s.addr.unwrap();
    (s, a.to_string())
}

async fn conn(a: &str) -> Client {
    Client::connect(a).await.unwrap()
}

/// Create and start a world; returns (code, owner key).
async fn world(a: &str, config: &str) -> (String, String) {
    let mut c = conn(a).await;
    let (code, key, _) = c.create(config, GEN).await.unwrap();
    assert_eq!(
        c.owner(&code, &key, "start").await.unwrap(),
        format!("{code} started")
    );
    (code, key)
}

async fn expect_error(c: &mut Client, prefix: &str) {
    let mut g = c.read(Duration::from_secs(10)).await;
    while matches!(g, Got::Frame('P' | 'X' | 'U' | 'Y' | 'W' | 'R', _)) {
        g = c.read(Duration::from_secs(10)).await;
    }
    match g {
        Got::Frame('E', p) => {
            let t = String::from_utf8(p).unwrap();
            assert!(t.starts_with(prefix), "got E {t:?}, wanted {prefix:?}");
        }
        g => panic!("wanted E {prefix}, got {g:?}"),
    }
    assert_eq!(c.read(Duration::from_secs(3)).await, Got::Closed);
}

/// The next frame of `kind`, skipping others.
async fn next(c: &mut Client, kind: char) -> String {
    let p = c
        .until(kind, Duration::from_secs(5), |_, _| {})
        .await
        .unwrap();
    String::from_utf8(p).unwrap()
}

async fn nothing(c: &mut Client, kind: char) {
    let end = tokio::time::Instant::now() + Duration::from_millis(300);
    loop {
        match c
            .read(end.saturating_duration_since(tokio::time::Instant::now()))
            .await
        {
            Got::Frame(k, p) if k == kind => {
                panic!("unexpected {k} {:?}", String::from_utf8_lossy(&p))
            }
            Got::Frame(..) => {}
            _ => return,
        }
    }
}

fn is_key(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[tokio::test]
async fn create_start_join_rejoin() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let mut o = conn(&a).await;
    let (code, owner, _) = o.create(TWO, GEN).await.unwrap();
    assert_eq!(code.len(), 6);
    assert!(is_key(&owner));
    // stored, not joinable yet
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} Ash", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "not-started").await;
    assert_eq!(
        o.owner(&code, &owner, "start").await.unwrap(),
        format!("{code} started")
    );
    // join by name: player 1, a new key, the player's data
    let mut ash = conn(&a).await;
    let p1 = ash.join(&code.to_lowercase(), "Ash", GEN).await.unwrap();
    assert_eq!((p1.player, p1.name.as_str()), (1, "Ash"));
    assert!(is_key(&p1.key));
    assert!(p1.items.is_empty());
    assert_eq!(p1.data["world"], code.as_str());
    assert_eq!(p1.data["player"], 1);
    assert_eq!(p1.data["players"][1]["name"], "Misty");
    assert_eq!(p1.data["data"]["slot"], 1);
    assert_eq!(p1.data["data"]["version"], "black");
    let (locs, _) = locations(&p1.data);
    assert!(locs.len() > 500, "{} locations", locs.len());
    ash.confirm().await.unwrap();
    let mut misty = conn(&a).await;
    let p2 = misty.join(&code, "Misty", GEN).await.unwrap();
    misty.confirm().await.unwrap();
    assert_eq!(p2.player, 2);
    assert_ne!(p2.key, p1.key);
    assert_eq!(next(&mut misty, 'P').await, "1 Ash");
    assert_eq!(next(&mut ash, 'P').await, "2 Misty");
    // a drop: X to the other; a rejoin by key keeps the number and key
    drop(ash);
    assert_eq!(next(&mut misty, 'X').await, "1");
    let mut ash = conn(&a).await;
    let p1b = ash.join(&code, &p1.key, GEN).await.unwrap();
    assert_eq!((p1b.player, p1b.key.as_str()), (1, p1.key.as_str()));
    assert_eq!(next(&mut misty, 'P').await, "1 Ash");
    // a rejoin over a live connection closes the old one, without X
    let mut ash2 = conn(&a).await;
    ash2.join(&code, &p1.key, GEN).await.unwrap();
    assert_eq!(next(&mut ash, 'P').await, "2 Misty");
    assert_eq!(ash.read(Duration::from_secs(3)).await, Got::Closed);
    nothing(&mut misty, 'X').await;
    // B leaves
    ash2.send('B', b"").await.unwrap();
    assert_eq!(next(&mut misty, 'X').await, "1");
    // no key in the store, only hashes
    let wj = std::fs::read_to_string(dir.path().join(&code).join("world.json")).unwrap();
    let log = std::fs::read_to_string(dir.path().join(&code).join("log")).unwrap();
    for k in [&owner, &p1.key, &p2.key] {
        assert!(!wj.contains(k.as_str()) && !log.contains(k.as_str()));
    }
    assert!(wj.contains("\"seed\": 4242"));
    assert!(wj.contains("\"world_hash\": \"1b98415ea2bc4583\""));
    assert_eq!(log.lines().filter(|l| l.contains("\"claim\"")).count(), 2);
}

#[tokio::test]
async fn refusals() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let (code, owner) = world(&a, TWO).await;
    let mut ash = conn(&a).await;
    let p1 = ash.join(&code, "Ash", GEN).await.unwrap();
    // name taken
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} Ash", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "name-taken").await;
    // no world
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} ZZZZZZ Ash", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "no-world").await;
    // bad key, unknown name
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} {}", c.token, "ab".repeat(16)).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "bad-key").await;
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} Brock", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "bad-key").await;
    // not owner
    let mut c = conn(&a).await;
    c.send(
        'O',
        format!("eidos-net/2 {code} {} close", "cd".repeat(16)).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "not-owner").await;
    let mut c = conn(&a).await;
    c.send('O', format!("eidos-net/2 ZZZZZZ {owner} close").as_bytes())
        .await
        .unwrap();
    expect_error(&mut c, "no-world").await;
    // version, bad frames, bad state
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/1 {} {code} Misty", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "version").await;
    let mut c = conn(&a).await;
    c.send('V', b"1 2 3").await.unwrap();
    expect_error(&mut c, "bad-state V before joining a world").await;
    let mut c = conn(&a).await;
    c.send('U', b"1 2 3 4 5").await.unwrap();
    expect_error(&mut c, "bad-state U before joining a world").await;
    let mut c = conn(&a).await;
    c.send('O', format!("eidos-net/2 {code} {owner} pause").as_bytes())
        .await
        .unwrap();
    expect_error(&mut c, "bad-frame").await;
    let mut c = conn(&a).await;
    c.send('F', b"1 0").await.unwrap();
    expect_error(&mut c, "bad-frame").await;
    let mut c = conn(&a).await;
    c.send('N', format!("eidos-net/2 {}\n@3", c.token).as_bytes())
        .await
        .unwrap();
    expect_error(&mut c, "bad-frame no whole F message 3").await;
    // relay frames inside a world session, world frames inside a relay session
    let mut c = conn(&a).await;
    c.join(&code, "Misty", GEN).await.unwrap();
    c.send('H', format!("eidos-net/1 {} M", c.token).as_bytes())
        .await
        .unwrap();
    expect_error(&mut c, "bad-state H in a world session").await;
    let mut ash2 = conn(&a).await;
    ash2.join(&code, &p1.key, GEN).await.unwrap();
    ash2.send('D', b"0\nk\nx").await.unwrap();
    expect_error(&mut ash2, "bad-state D in a world session").await;
    let mut h = conn(&a).await;
    h.send('H', format!("eidos-net/1 {} Host", h.token).as_bytes())
        .await
        .unwrap();
    let _code = next(&mut h, 'C').await;
    assert_eq!(next(&mut h, 'W').await, "1 Host");
    h.send(
        'I',
        format!("eidos-net/2 {} {code} Misty", h.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut h, "bad-state I after joining").await;
    // closed: no more joins, even with a key
    let mut o = conn(&a).await;
    assert_eq!(
        o.owner(&code, &owner, "close").await.unwrap(),
        format!("{code} closed")
    );
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} {}", c.token, p1.key).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "closed").await;
    // start opens it again
    assert_eq!(
        o.owner(&code, &owner, "start").await.unwrap(),
        format!("{code} started")
    );
    let mut c = conn(&a).await;
    assert_eq!(c.join(&code, &p1.key, GEN).await.unwrap().player, 1);
}

#[tokio::test]
async fn world_sessions_off() {
    let mut c = wcfg(Path::new(""));
    c.world_store = String::new();
    let (s, a) = start(c).await;
    assert!(!s.worlds_on());
    let mut c = conn(&a).await;
    c.send('N', format!("eidos-net/2 {}\n{TWO}", c.token).as_bytes())
        .await
        .unwrap();
    expect_error(&mut c, "unavailable").await;
    // the relay works as before
    let mut h = conn(&a).await;
    h.send('H', format!("eidos-net/1 {} Host", h.token).as_bytes())
        .await
        .unwrap();
    next(&mut h, 'C').await;
    assert_eq!(next(&mut h, 'W').await, "1 Host");
}

#[tokio::test]
async fn unwritable_store_falls_back_to_temp() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let mut c = wcfg(&file.join("worlds"));
    c.gen_workers = 1;
    let (s, a) = start(c).await;
    assert!(s.worlds_on());
    let mut h = conn(&a).await;
    h.send('H', format!("eidos-net/1 {} Host", h.token).as_bytes())
        .await
        .unwrap();
    next(&mut h, 'C').await;
}

#[tokio::test]
async fn items_for_an_absent_player_wait_with_their_index() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let (code, _) = world(&a, TWO).await;
    let mut ash = conn(&a).await;
    let p1 = ash.join(&code, "Ash", GEN).await.unwrap();
    let mut misty = conn(&a).await;
    let p2 = misty.join(&code, "Misty", GEN).await.unwrap();
    misty.confirm().await.unwrap();
    sleep(Duration::from_millis(100)).await;
    drop(misty);
    let (locs1, to1) = locations(&p1.data);
    let for_misty = to1[&2];
    assert!(for_misty > 100);
    // Ash checks everything while Misty is away, then again (duplicates)
    ash.checks(&locs1).await.unwrap();
    ash.checks(&locs1).await.unwrap();
    // unknown locations are ignored
    ash.checks(&[1, 2, 3]).await.unwrap();
    nothing(&mut ash, 'Y').await;
    // Misty comes back: every item, index 1 up, from player 1
    let mut misty = conn(&a).await;
    let p2b = misty.join(&code, &p2.key, GEN).await.unwrap();
    assert_eq!(p2b.items.len(), for_misty);
    for (i, (idx, _, from)) in p2b.items.iter().enumerate() {
        assert_eq!((*idx, *from), (i as u32 + 1, 1));
    }
    let want: Vec<u32> = p1.data["data"]["locations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l[2] == 2)
        .map(|l| l[1].as_u64().unwrap() as u32)
        .collect();
    let got: Vec<u32> = p2b.items.iter().map(|x| x.1).collect();
    let (mut w, mut g) = (want.clone(), got.clone());
    w.sort();
    g.sort();
    assert_eq!(w, g);
    // Misty checks one of Ash's items: Ash gets it at once, index 1
    let (locs2, _) = locations(&p2.data);
    let arr = p2.data["data"]["locations"].as_array().unwrap();
    let for_ash = arr.iter().find(|l| l[2] == 1).unwrap();
    let own = arr.iter().find(|l| l[2] == 2).unwrap();
    let loc = for_ash[0].as_u64().unwrap() as u32;
    misty.checks(&[loc, loc]).await.unwrap();
    let y = next(&mut ash, 'Y').await;
    assert_eq!(
        parse_items(y.as_bytes()),
        vec![(1, for_ash[1].as_u64().unwrap() as u32, 2)]
    );
    misty.checks(&[loc]).await.unwrap();
    // an own item is recorded, nobody gets a Y
    misty
        .checks(&[own[0].as_u64().unwrap() as u32])
        .await
        .unwrap();
    nothing(&mut ash, 'Y').await;
    // the rest: Ash gets indexes 2 up
    misty.checks(&locs2).await.unwrap();
    let rest = ash
        .items(arr.iter().filter(|l| l[2] == 1).count() - 1, SEC * 10)
        .await;
    for (i, (idx, _, from)) in rest.iter().enumerate() {
        assert_eq!((*idx, *from), (i as u32 + 2, 2));
    }
    // goals
    ash.send('V', b"goal").await.unwrap();
    misty.send('V', b"goal").await.unwrap();
    misty.send('V', b"goal").await.unwrap();
    sleep(Duration::from_millis(300)).await;
    let log = std::fs::read_to_string(dir.path().join(&code).join("log")).unwrap();
    assert_eq!(log.lines().filter(|l| l.contains("\"goal\"")).count(), 2);
    let checks = log.lines().filter(|l| l.contains("\"check\"")).count();
    assert_eq!(checks, locs1.len() + locs2.len());
}

#[tokio::test]
async fn restart_rebuilds_from_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let (s, a) = start(wcfg(dir.path())).await;
    let (code, owner) = world(&a, TWO).await;
    let mut ash = conn(&a).await;
    let p1 = ash.join(&code, "Ash", GEN).await.unwrap();
    let (locs1, to1) = locations(&p1.data);
    ash.checks(&locs1[..300]).await.unwrap();
    ash.send('V', b"goal").await.unwrap();
    sleep(Duration::from_millis(300)).await;
    let half: usize = p1.data["data"]["locations"].as_array().unwrap()[..300]
        .iter()
        .filter(|l| l[2] == 2)
        .count();
    s.shutdown().await;
    // the log may end in a cut line (a crash during a write)
    let log = dir.path().join(&code).join("log");
    let mut text = std::fs::read(&log).unwrap();
    text.extend_from_slice(b"{\"e\":\"check\",\"p\":1,");
    std::fs::write(&log, text).unwrap();
    let (s, a) = start(wcfg(dir.path())).await;
    assert_eq!(s.world_count(), 1);
    // Misty joins for the first time: the items found before the restart
    let mut misty = conn(&a).await;
    let p2 = misty.join(&code, "Misty", GEN).await.unwrap();
    assert_eq!(p2.items.len(), half);
    assert_eq!(p2.data["world_hash"], "1b98415ea2bc4583");
    // Ash's name is claimed; the key still works; checks again are ignored
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} Ash", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "name-taken").await;
    let mut ash = conn(&a).await;
    ash.join(&code, &p1.key, GEN).await.unwrap();
    ash.checks(&locs1).await.unwrap();
    let more = misty.items(to1[&2] - half, SEC * 10).await;
    assert_eq!(more.len(), to1[&2] - half);
    assert_eq!(more[0].0 as usize, half + 1);
    // the owner key still works after the restart
    let mut o = conn(&a).await;
    assert_eq!(
        o.owner(&code, &owner, "close").await.unwrap(),
        format!("{code} closed")
    );
}

#[tokio::test]
async fn generator_mismatch_is_not_served() {
    let dir = tempfile::tempdir().unwrap();
    let (s, a) = start(wcfg(dir.path())).await;
    let (c1, _) = world(&a, TWO).await;
    let (c2, _) = world(&a, &players(1, 7)).await;
    s.shutdown().await;
    let edit = |code: &str, from: &str, to: &str| {
        let p = dir.path().join(code).join("world.json");
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains(from));
        std::fs::write(&p, t.replacen(from, to, 1)).unwrap();
    };
    // another stored hash; another generator version
    edit(
        &c1,
        "\"world_hash\": \"1b98415ea2bc4583\"",
        "\"world_hash\": \"0000000000000000\"",
    );
    edit(&c2, "\"version\": \"0.1.0\"", "\"version\": \"0.0.9\"");
    let (_s, a) = start(wcfg(dir.path())).await;
    for (code, name) in [(&c1, "Ash"), (&c2, "P1")] {
        let mut c = conn(&a).await;
        c.send(
            'I',
            format!("eidos-net/2 {} {code} {name}", c.token).as_bytes(),
        )
        .await
        .unwrap();
        expect_error(&mut c, "gen-mismatch").await;
    }
}

#[tokio::test]
async fn positions_go_by_zone() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let (code, _) = world(&a, &players(3, 11)).await;
    let mut p = Vec::new();
    let mut keys = Vec::new();
    for i in 1..=3 {
        let mut c = conn(&a).await;
        keys.push(c.join(&code, &format!("P{i}"), GEN).await.unwrap().key);
        p.push(c);
    }
    sleep(Duration::from_millis(200)).await;
    // P1 enters zone 5: a zone change goes to everyone
    p[0].send('U', b"5 10 12 0 1").await.unwrap();
    assert_eq!(next(&mut p[1], 'U').await, "1 5 10 12 0 1");
    assert_eq!(next(&mut p[2], 'U').await, "1 5 10 12 0 1");
    p[1].send('U', b"5 3 4 0 0").await.unwrap();
    assert_eq!(next(&mut p[0], 'U').await, "2 5 3 4 0 0");
    assert_eq!(next(&mut p[2], 'U').await, "2 5 3 4 0 0");
    p[2].send('U', b"9 -1 2 0 3").await.unwrap();
    assert_eq!(next(&mut p[0], 'U').await, "3 9 -1 2 0 3");
    assert_eq!(next(&mut p[1], 'U').await, "3 9 -1 2 0 3");
    // a step inside zone 5 goes to P2 only
    p[0].send('U', b"5 11 12 0 1").await.unwrap();
    assert_eq!(next(&mut p[1], 'U').await, "1 5 11 12 0 1");
    nothing(&mut p[2], 'U').await;
    // a joiner sees everyone's last position after the P frames
    let mut c = p.pop().unwrap();
    c.send('B', b"").await.unwrap();
    assert_eq!(next(&mut p[0], 'X').await, "3");
    let mut c = conn(&a).await;
    c.join(&code, &keys[2], GEN).await.unwrap();
    let mut seen = HashMap::new();
    for _ in 0..4 {
        match c.read(SEC * 3).await {
            Got::Frame(k, v) => {
                seen.insert(String::from_utf8(v).unwrap(), k);
            }
            g => panic!("{g:?}"),
        }
    }
    for (f, k) in [
        ("1 P1", 'P'),
        ("2 P2", 'P'),
        ("1 5 11 12 0 1", 'U'),
        ("2 5 3 4 0 0", 'U'),
    ] {
        assert_eq!(seen.get(f), Some(&k), "{f} in {seen:?}");
    }
    // tile positions are rate limited, zone changes are not
    for i in 0..60 {
        p[0].send('U', format!("5 {i} 0 0 0").as_bytes())
            .await
            .unwrap();
    }
    let mut n = 0;
    while let Got::Frame(k, _) = p[1].read(Duration::from_millis(400)).await {
        n += usize::from(k == 'U');
    }
    assert!((10..=30).contains(&n), "{n} tile positions passed");
}

#[tokio::test]
async fn limits() {
    let dir = tempfile::tempdir().unwrap();
    // generations per address per hour
    let mut cfg = wcfg(dir.path());
    cfg.gen_per_hour = 1;
    let (_s, a) = start(cfg).await;
    world(&a, &players(1, 1)).await;
    let mut c = conn(&a).await;
    c.send(
        'N',
        format!("eidos-net/2 {}\n{}", c.token, players(1, 2)).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "busy").await;
    // open worlds per address
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = wcfg(dir.path());
    cfg.worlds_per_ip = 1;
    let (_s, a) = start(cfg).await;
    let (code, owner) = world(&a, &players(1, 1)).await;
    let mut c = conn(&a).await;
    c.send(
        'N',
        format!("eidos-net/2 {}\n{}", c.token, players(1, 2)).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "busy").await;
    // a closed world does not count
    conn(&a).await.owner(&code, &owner, "close").await.unwrap();
    world(&a, &players(1, 3)).await;
    // the queue
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = wcfg(dir.path());
    cfg.gen_workers = 1;
    cfg.gen_queue = 1;
    let (_s, a) = start(cfg).await;
    let mut c1 = conn(&a).await;
    c1.send(
        'N',
        format!("eidos-net/2 {}\n{}", c1.token, players(8, 1)).as_bytes(),
    )
    .await
    .unwrap();
    sleep(Duration::from_millis(100)).await;
    let mut c2 = conn(&a).await;
    let a2 = a.clone();
    let t2 = tokio::spawn(async move {
        let _ = a2;
        c2.create(&players(1, 2), GEN).await
    });
    sleep(Duration::from_millis(100)).await;
    let mut c3 = conn(&a).await;
    c3.send(
        'N',
        format!("eidos-net/2 {}\n{}", c3.token, players(1, 3)).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c3, "gen-busy").await;
    let r1 = next(&mut c1, 'R').await;
    assert_eq!(r1.len(), 6 + 1 + 32);
    let (_, _, places) = t2.await.unwrap().unwrap();
    assert_eq!(places, vec![1]);
    // sizes and checks of the configuration
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = wcfg(dir.path());
    cfg.world_config_max = 2000;
    let (_s, a) = start(cfg).await;
    for (conf, err) in [
        (players(17, 1), "gen-failed"),
        ("{".to_string(), "gen-failed"),
        ("[]".to_string(), "gen-failed"),
        (r#"{"schema": 1, "players": []}"#.to_string(), "gen-failed"),
        (r#"{"schema": 2, "players": [{"name": "A", "version": "black"}]}"#.to_string(), "gen-failed"),
        (r#"{"schema": 1, "players": [{"name": "A", "version": "blue"}]}"#.to_string(), "gen-failed"),
        (r#"{"schema": 1, "players": [{"name": "A", "version": "black"}, {"name": "A", "version": "white"}]}"#.to_string(), "gen-failed"),
        (format!("{}{}", players(1, 1), " ".repeat(2000)), "too-big"),
    ] {
        let mut c = conn(&a).await;
        c.send_big('N', &format!("eidos-net/2 {}", c.token), conf.as_bytes()).await.unwrap();
        expect_error(&mut c, err).await;
    }
    // F pieces: a configuration bigger than a frame
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let big = format!("{}{}", players(2, 5), " ".repeat(9000));
    let mut c = conn(&a).await;
    let (code, _, _) = c.create(&big, GEN).await.unwrap();
    assert_eq!(code.len(), 6);
    // a seed is drawn when there is none
    let mut c = conn(&a).await;
    c.create(
        r#"{"schema": 1, "players": [{"name": "A", "version": "black"}]}"#,
        GEN,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn delete_and_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = wcfg(dir.path());
    cfg.world_sweep_ms = 100;
    cfg.world_idle_s = 2;
    cfg.world_done_s = 1;
    let (s, a) = start(cfg).await;
    // delete: players get E no-world, the folder goes at once
    let (code, owner) = world(&a, &players(1, 1)).await;
    let mut p = conn(&a).await;
    p.join(&code, "P1", GEN).await.unwrap();
    let mut o = conn(&a).await;
    assert_eq!(
        o.owner(&code, &owner, "delete").await.unwrap(),
        format!("{code} deleted")
    );
    expect_error(&mut p, "no-world").await;
    assert!(!dir.path().join(&code).exists());
    let mut c = conn(&a).await;
    c.send('I', format!("eidos-net/2 {} {code} P1", c.token).as_bytes())
        .await
        .unwrap();
    expect_error(&mut c, "no-world").await;
    // a world nobody joins expires; a finished one expires after its end
    let (idle, _) = world(&a, &players(1, 2)).await;
    let (done, _) = world(&a, &players(1, 3)).await;
    let mut p = conn(&a).await;
    p.join(&done, "P1", GEN).await.unwrap();
    p.send('V', b"goal").await.unwrap();
    sleep(Duration::from_millis(200)).await;
    // nothing expires while a player is connected
    sleep(SEC * 2).await;
    assert!(dir.path().join(&done).exists());
    drop(p);
    sleep(Duration::from_millis(1600)).await;
    assert!(!dir.path().join(&idle).exists());
    assert!(!dir.path().join(&done).exists());
    assert_eq!(s.world_count(), 0);
}

#[tokio::test]
async fn unloaded_world_is_made_again_on_join() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = wcfg(dir.path());
    cfg.world_sweep_ms = 50;
    cfg.world_unload_ms = 100;
    let (_s, a) = start(cfg).await;
    let (code, _) = world(&a, TWO).await;
    let mut c = conn(&a).await;
    let p1 = c.join(&code, "Ash", GEN).await.unwrap();
    c.confirm().await.unwrap();
    sleep(Duration::from_millis(100)).await;
    drop(c);
    sleep(Duration::from_millis(500)).await;
    let mut c = conn(&a).await;
    let again = c.join(&code, &p1.key, GEN).await.unwrap();
    assert_eq!(again.data, p1.data);
}

#[tokio::test]
async fn coop_relay_works_beside_worlds() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let (code, _) = world(&a, TWO).await;
    let mut w = conn(&a).await;
    w.join(&code, "Ash", GEN).await.unwrap();
    let mut h = conn(&a).await;
    h.send('H', format!("eidos-net/1 {} Host", h.token).as_bytes())
        .await
        .unwrap();
    let rc = next(&mut h, 'C').await;
    next(&mut h, 'W').await;
    let mut j = conn(&a).await;
    j.send('J', format!("eidos-net/1 {} {rc} Joe", j.token).as_bytes())
        .await
        .unwrap();
    assert_eq!(next(&mut j, 'W').await, "2 Joe");
    j.send('D', b"1\nk\nhello").await.unwrap();
    assert_eq!(next(&mut h, 'D').await, "2\nk\nhello");
    // the world player still gets its frames
    w.send('V', b"goal").await.unwrap();
    w.send('K', b"").await.unwrap();
    nothing(&mut w, 'E').await;
}

/// `C`: a game's Join box asks what a code names before it joins.
#[tokio::test]
async fn lookup_names_session_world_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let ask = |code: String| {
        let a = a.clone();
        async move {
            let mut c = conn(&a).await;
            c.send('C', format!("eidos-net/2 {code}").as_bytes()).await.unwrap();
            let p = next(&mut c, 'C').await;
            assert_eq!(c.read(Duration::from_secs(3)).await, Got::Closed);
            p
        }
    };
    // a world: its state, its players and its options
    let (code, key) = world(&a, TWO).await;
    let p = ask(code.to_lowercase()).await;
    let (kind, json) = p.split_once('\n').unwrap();
    assert_eq!(kind, "world");
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(v["state"], "open");
    assert_eq!(v["free"], 2);
    assert_eq!(v["players"][0]["name"], "Ash");
    assert_eq!(v["players"][0]["claimed"], false);
    assert_eq!(v["options"]["goal"], "ghetsis");
    // a claim shows
    let mut w = conn(&a).await;
    w.join(&code, "Ash", GEN).await.unwrap();
    w.send('V', b"").await.unwrap();
    sleep(Duration::from_millis(300)).await;
    let v: serde_json::Value =
        serde_json::from_str(ask(code.clone()).await.split_once('\n').unwrap().1).unwrap();
    assert_eq!(v["free"], 1);
    assert_eq!(v["players"][0]["claimed"], true);
    // a closed world says so
    let mut o = conn(&a).await;
    assert_eq!(o.owner(&code, &key, "close").await.unwrap(), format!("{code} closed"));
    let v: serde_json::Value =
        serde_json::from_str(ask(code.clone()).await.split_once('\n').unwrap().1).unwrap();
    assert_eq!(v["state"], "closed");
    // a live co-op session
    let mut h = conn(&a).await;
    h.send('H', format!("eidos-net/1 {} Host", h.token).as_bytes()).await.unwrap();
    let rc = next(&mut h, 'C').await;
    assert_eq!(ask(rc.clone()).await, "session");
    // nothing, and a malformed code
    assert_eq!(ask("ZZZZZZ".into()).await, "none");
    assert_eq!(ask("ab".into()).await, "none");
    // the session code is no world's and the world code is no session's
    assert_ne!(rc, code);
}

/// 16 players, all online: every player checks every location; each gets
/// exactly the items placed for them, with indexes 1 up and no gaps.
#[tokio::test]
async fn soak_sixteen_players() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let (code, _) = world(&a, &players(16, 99)).await;
    let mut cs = Vec::new();
    let mut want: HashMap<u8, usize> = HashMap::new();
    let mut all_locs = Vec::new();
    for i in 1..=16u8 {
        let mut c = conn(&a).await;
        let p = c.join(&code, &format!("P{i}"), GEN).await.unwrap();
        assert_eq!(p.player, i);
        let (locs, to) = locations(&p.data);
        for (r, n) in to {
            if r != i {
                *want.entry(r).or_default() += n;
            }
        }
        all_locs.push(locs);
        cs.push(c);
    }
    let mut tasks = Vec::new();
    for (i, mut c) in cs.into_iter().enumerate() {
        let locs = all_locs[i].clone();
        let n = want.get(&(i as u8 + 1)).copied().unwrap_or(0);
        tasks.push(tokio::spawn(async move {
            // send in four parts, with positions between
            for (k, part) in locs.chunks(locs.len() / 4 + 1).enumerate() {
                c.checks(part).await.unwrap();
                c.send('U', format!("{} {k} 0 0 0", 100 + i % 3).as_bytes())
                    .await
                    .unwrap();
            }
            c.send('V', b"goal").await.unwrap();
            let got = c.items(n, Duration::from_secs(60)).await;
            (i as u8 + 1, n, got)
        }));
    }
    for t in tasks {
        let (p, n, got) = t.await.unwrap();
        assert_eq!(got.len(), n, "player {p}");
        let mut idx: Vec<u32> = got.iter().map(|x| x.0).collect();
        idx.sort();
        assert_eq!(idx, (1..=n as u32).collect::<Vec<_>>(), "player {p}");
        assert!(got.iter().all(|x| x.2 != p));
    }
    sleep(Duration::from_millis(300)).await;
    let log = std::fs::read_to_string(dir.path().join(&code).join("log")).unwrap();
    assert_eq!(log.lines().filter(|l| l.contains("\"goal\"")).count(), 16);
}

/// Every randomizer on (arkhipelagos' tests/configs/all-random.json): the
/// player data is far bigger than a frame and arrives whole in F pieces.
#[tokio::test]
async fn every_randomizer_on() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let (code, _) = world(&a, include_str!("all-random.json")).await;
    let mut c = conn(&a).await;
    let p = c.join(&code, "Misty", GEN).await.unwrap();
    let size = serde_json::to_vec(&p.data).unwrap().len();
    assert!(size > 100_000 && size < 1_048_576, "{size} bytes");
    let tables = p.data["data"]["tables"].as_object().unwrap();
    assert!(!tables.is_empty());
    assert_eq!(p.data["data"]["options"]["randomize_types"][0], "randomize");
    assert_eq!(p.data["players"][0]["name"], "Ash");
}

/// A generation over the time limit is gen-failed; the thread finishes it,
/// and the next configuration is made.
#[tokio::test]
async fn generation_time_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = wcfg(dir.path());
    cfg.gen_workers = 1;
    cfg.gen_time_ms = 1500;
    let (_s, a) = start(cfg).await;
    // the logic builds on first use: not inside the timed run below
    world(&a, &players(1, 1)).await;
    let mut c = conn(&a).await;
    c.send(
        'N',
        format!("eidos-net/2 {}\n{}", c.token, players(16, 5)).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "gen-failed over the time limit").await;
    let (code, _) = world(&a, TWO).await;
    let mut c = conn(&a).await;
    assert_eq!(c.join(&code, "Ash", GEN).await.unwrap().player, 1);
}

fn log_lines(dir: &Path, code: &str, e: &str) -> usize {
    std::fs::read_to_string(dir.join(code).join("log"))
        .unwrap()
        .lines()
        .filter(|l| l.contains(&format!("\"e\":\"{e}\"")))
        .count()
}

/// A first join that ends before the game confirmed its key (its first
/// `V` or `U`) frees the name; after the confirmation the name stays
/// claimed.
#[tokio::test]
async fn claims_are_provisional_until_confirmed() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    let (code, _) = world(&a, TWO).await;
    // a drop before the confirmation: the name is free again, with a new key
    let mut c = conn(&a).await;
    let first = c.join(&code, "Ash", GEN).await.unwrap();
    drop(c);
    sleep(Duration::from_millis(300)).await;
    assert_eq!(log_lines(dir.path(), &code, "release"), 1);
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} {}", c.token, first.key).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "bad-key").await;
    let mut c = conn(&a).await;
    let second = c.join(&code, "Ash", GEN).await.unwrap();
    assert_ne!(second.key, first.key);
    // B before the confirmation also frees it
    c.send('B', b"").await.unwrap();
    sleep(Duration::from_millis(300)).await;
    assert_eq!(log_lines(dir.path(), &code, "release"), 2);
    // a U confirms
    let mut c = conn(&a).await;
    let third = c.join(&code, "Ash", GEN).await.unwrap();
    c.send('U', b"1 0 0 0 0").await.unwrap();
    sleep(Duration::from_millis(200)).await;
    drop(c);
    sleep(Duration::from_millis(300)).await;
    assert_eq!(log_lines(dir.path(), &code, "confirm"), 1);
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} Ash", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "name-taken").await;
    let mut c = conn(&a).await;
    assert_eq!(c.join(&code, &third.key, GEN).await.unwrap().player, 1);
    // a rejoin with the key over a provisional claim confirms it
    let mut m = conn(&a).await;
    let pm = m.join(&code, "Misty", GEN).await.unwrap();
    let mut m2 = conn(&a).await;
    m2.join(&code, &pm.key, GEN).await.unwrap();
    drop(m);
    drop(m2);
    sleep(Duration::from_millis(300)).await;
    assert_eq!(log_lines(dir.path(), &code, "confirm"), 2);
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} Misty", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "name-taken").await;
}

/// A restart between the claim and the confirmation frees the name too:
/// a graceful stop releases it at the drop, a crash releases it at load.
#[tokio::test]
async fn unconfirmed_claims_are_released_over_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (s, a) = start(wcfg(dir.path())).await;
    let (code, _) = world(&a, &players(3, 4)).await;
    // P1 confirmed, P2 provisional at the stop
    let mut p1 = conn(&a).await;
    let k1 = p1.join(&code, "P1", GEN).await.unwrap().key;
    p1.confirm().await.unwrap();
    let mut p2 = conn(&a).await;
    let k2 = p2.join(&code, "P2", GEN).await.unwrap().key;
    sleep(Duration::from_millis(200)).await;
    s.shutdown().await;
    drop((p1, p2));
    // a crash right after P3's claim line was written
    let log = dir.path().join(&code).join("log");
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str("{\"e\":\"claim\",\"p\":3,\"key\":\"00\",\"t\":1}\n");
    std::fs::write(&log, text).unwrap();
    let (_s, a) = start(wcfg(dir.path())).await;
    assert_eq!(log_lines(dir.path(), &code, "release"), 2);
    let mut c = conn(&a).await;
    assert_eq!(c.join(&code, &k1, GEN).await.unwrap().player, 1);
    let mut c = conn(&a).await;
    c.send(
        'I',
        format!("eidos-net/2 {} {code} {k2}", c.token).as_bytes(),
    )
    .await
    .unwrap();
    expect_error(&mut c, "bad-key").await;
    for name in ["P2", "P3"] {
        let mut c = conn(&a).await;
        let p = c.join(&code, name, GEN).await.unwrap();
        assert_ne!(p.key, k2);
    }
}
