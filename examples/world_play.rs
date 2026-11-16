//! Play a whole two-player world against a metaxy relay, from creation to
//! both goals, with the real generator on the relay's side.
//!
//!   cargo run --example world_play -- [host:port] [--config file.json] [--keep]
//!
//! The default address is 127.0.0.1:20609. Steps: create the world (N),
//! start it (O start), join both players by name (I), player 2 leaves,
//! player 1 checks every location, player 2 joins again by key and must get
//! every item waiting for it with indexes 1 up, player 2 checks every
//! location while player 1 is online, both send positions, both reach the
//! goal, the owner deletes the world (unless --keep). Exit code 0 when every
//! step held.

use metaxy::world::client::{locations, Client, Got};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const TWO: &str = r#"{"schema": 1,
 "options": {"goal": "ghetsis", "season_control": "vanilla"},
 "players": [{"name": "Ash", "version": "black"},
             {"name": "Misty", "version": "white"}]}"#;

fn want(data: &serde_json::Value, receiver: u8) -> Vec<u32> {
    let mut v: Vec<u32> = data["data"]["locations"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|l| l[2].as_u64() == Some(u64::from(receiver)))
                .map(|l| l[1].as_u64().unwrap_or(0) as u32)
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn check(ok: bool, what: &str) -> Result<(), String> {
    if ok {
        println!("ok    {what}");
        Ok(())
    } else {
        Err(format!("FAIL  {what}"))
    }
}

async fn run(addr: &str, config: &str, keep: bool) -> Result<(), String> {
    let t0 = Instant::now();
    let long = Duration::from_secs(120);
    let mut owner = Client::connect(addr)
        .await
        .map_err(|e| format!("connect {addr}: {e}"))?;
    let (code, key, waits) = owner.create(config, long).await?;
    println!(
        "world {code} made in {:.1} s (queue places seen: {waits:?})",
        t0.elapsed().as_secs_f64()
    );
    check(
        owner.owner(&code, &key, "start").await? == format!("{code} started"),
        "O start",
    )?;
    let names: Vec<String> = serde_json::from_str::<serde_json::Value>(config)
        .map_err(|e| e.to_string())?["players"]
        .as_array()
        .ok_or("no players")?
        .iter()
        .map(|p| p["name"].as_str().unwrap_or("").to_string())
        .collect();
    if names.len() != 2 {
        return Err("the configuration must have two players".into());
    }
    let mut a = Client::connect(addr).await.map_err(|e| e.to_string())?;
    let pa = a.join(&code, &names[0], long).await?;
    check(
        pa.player == 1 && pa.key.len() == 32,
        "player 1 joined by name, got a key",
    )?;
    let mut b = Client::connect(addr).await.map_err(|e| e.to_string())?;
    let pb = b.join(&code, &names[1], long).await?;
    check(
        pb.player == 2 && pb.key != pa.key,
        "player 2 joined by name, got a key",
    )?;
    let (la, _) = locations(&pa.data);
    let (lb, _) = locations(&pb.data);
    println!(
        "      player 1 has {} locations, player 2 has {}",
        la.len(),
        lb.len()
    );
    // both games keep their keys and say so (an empty V), as a game does
    // on every connect with the checks it has
    a.confirm().await.map_err(|e| e.to_string())?;
    b.confirm().await.map_err(|e| e.to_string())?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // player 2 leaves; player 1 checks everything
    drop(b);
    a.checks(&la).await.map_err(|e| e.to_string())?;
    a.checks(&la).await.map_err(|e| e.to_string())?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // player 2 comes back by key: every item waiting, index 1 up
    let mut b = Client::connect(addr).await.map_err(|e| e.to_string())?;
    let pb2 = b.join(&code, &pb.key, long).await?;
    check(pb2.player == 2, "player 2 joined again by key")?;
    let w2 = want(&pa.data, 2);
    let mut got: Vec<u32> = pb2.items.iter().map(|x| x.1).collect();
    got.sort();
    check(
        got == w2,
        &format!("player 2 got the {} items waiting for it", w2.len()),
    )?;
    check(
        pb2.items
            .iter()
            .enumerate()
            .all(|(i, x)| x.0 == i as u32 + 1 && x.2 == 1),
        "indexes 1 up, all from player 1",
    )?;
    // positions
    a.send('U', b"5 10 10 0 1")
        .await
        .map_err(|e| e.to_string())?;
    let u = b.until('U', Duration::from_secs(5), |_, _| {}).await?;
    check(u == b"1 5 10 10 0 1", "player 2 saw player 1 enter zone 5")?;
    // player 2 checks everything while player 1 is online
    b.checks(&lb).await.map_err(|e| e.to_string())?;
    let w1 = want(&pb.data, 1);
    let items = a.items(w1.len(), Duration::from_secs(60)).await;
    let mut got: Vec<u32> = items.iter().map(|x| x.1).collect();
    got.sort();
    check(
        got == w1,
        &format!("player 1 got its {} items at once", w1.len()),
    )?;
    let mut idx: Vec<u32> = items.iter().map(|x| x.0).collect();
    idx.sort();
    check(
        idx == (1..=w1.len() as u32).collect::<Vec<_>>(),
        "player 1's indexes 1 up, no gaps",
    )?;
    // goals
    a.send('V', b"goal").await.map_err(|e| e.to_string())?;
    b.send('V', b"goal").await.map_err(|e| e.to_string())?;
    a.send('K', b"").await.map_err(|e| e.to_string())?;
    let mut errs = HashMap::new();
    for (n, c) in [(1, &mut a), (2, &mut b)] {
        if let Got::Frame('E', p) = c.read(Duration::from_millis(500)).await {
            errs.insert(n, String::from_utf8_lossy(&p).into_owned());
        }
    }
    check(
        errs.is_empty(),
        &format!("both goals sent, no errors {errs:?}"),
    )?;
    if keep {
        println!("kept world {code}, owner key {key}");
        println!("      player keys: 1 {}  2 {}", pa.key, pb.key);
    } else {
        let mut o = Client::connect(addr).await.map_err(|e| e.to_string())?;
        check(
            o.owner(&code, &key, "delete").await? == format!("{code} deleted"),
            "O delete",
        )?;
    }
    println!("all steps held in {:.1} s", t0.elapsed().as_secs_f64());
    Ok(())
}

#[tokio::main]
async fn main() {
    let mut addr = "127.0.0.1:20609".to_string();
    let mut config = TWO.to_string();
    let mut keep = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--keep" => keep = true,
            "--config" => {
                let p = args.next().expect("--config needs a file");
                config = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
            }
            _ => addr = a,
        }
    }
    if let Err(e) = run(&addr, &config, keep).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
