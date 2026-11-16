//! A small world-session client, for tests and the `world_play` example.
//! It is not used by the server.

use super::frames::{self, PROTO2};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};

/// What a read gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Got {
    /// A frame; a payload that named `@<id>` holds the pieces' bytes after
    /// its first line.
    Frame(char, Vec<u8>),
    Closed,
    Nothing,
}

/// The push on join.
#[derive(Debug, Clone)]
pub struct Push {
    pub player: u8,
    pub key: String,
    pub data: Value,
    /// (index, item, from player)
    pub items: Vec<(u32, u32, u8)>,
    pub name: String,
}

pub struct Client {
    s: TcpStream,
    buf: Vec<u8>,
    pieces: HashMap<u32, Vec<u8>>,
    next_id: u32,
    pub token: String,
}

fn frame(kind: char, payload: &[u8]) -> Vec<u8> {
    let mut v = format!("{kind} {}\n", payload.len()).into_bytes();
    v.extend_from_slice(payload);
    v
}

/// Parse a `Y` payload.
pub fn parse_items(p: &[u8]) -> Vec<(u32, u32, u8)> {
    let t = std::str::from_utf8(p).unwrap_or("");
    let n: Vec<u64> = t.split(' ').filter_map(|w| w.parse().ok()).collect();
    n.chunks(3)
        .filter(|c| c.len() == 3)
        .map(|c| (c[0] as u32, c[1] as u32, c[2] as u8))
        .collect()
}

impl Client {
    pub async fn connect(addr: &str) -> std::io::Result<Client> {
        let s = TcpStream::connect(addr).await?;
        s.set_nodelay(true)?;
        let mut raw = [0u8; 16];
        getrandom::getrandom(&mut raw).expect("OS random source");
        Ok(Client {
            s,
            buf: Vec::new(),
            pieces: HashMap::new(),
            next_id: 0,
            token: raw.iter().map(|b| format!("{b:02x}")).collect(),
        })
    }

    pub async fn send(&mut self, kind: char, payload: &[u8]) -> std::io::Result<()> {
        self.s.write_all(&frame(kind, payload)).await
    }

    pub async fn send_raw(&mut self, b: &[u8]) -> std::io::Result<()> {
        self.s.write_all(b).await
    }

    /// `<kind> <head>\n<body>`, the body in `F` pieces when it is big.
    pub async fn send_big(&mut self, kind: char, head: &str, body: &[u8]) -> std::io::Result<()> {
        let b = frames::encode_big(kind as u8, head, body, 4224, &mut self.next_id);
        self.s.write_all(&b).await
    }

    fn take(&mut self) -> Option<(char, Vec<u8>)> {
        let nl = self.buf.iter().position(|&b| b == b'\n')?;
        let head = std::str::from_utf8(&self.buf[..nl]).ok()?.to_string();
        let kind = head.chars().next()?;
        let len: usize = head.get(2..)?.parse().ok()?;
        if self.buf.len() < nl + 1 + len {
            return None;
        }
        let p = self.buf[nl + 1..nl + 1 + len].to_vec();
        self.buf.drain(..nl + 1 + len);
        Some((kind, p))
    }

    /// The next frame, `F` pieces joined into the frame that uses them.
    pub async fn read(&mut self, wait: Duration) -> Got {
        let end = Instant::now() + wait;
        loop {
            while let Some((k, p)) = self.take() {
                if k == 'F' {
                    if let Some(nl) = p.iter().position(|&b| b == b'\n') {
                        let h = String::from_utf8_lossy(&p[..nl]).to_string();
                        let id: u32 = h
                            .split(' ')
                            .next()
                            .and_then(|x| x.parse().ok())
                            .unwrap_or(0);
                        self.pieces
                            .entry(id)
                            .or_default()
                            .extend_from_slice(&p[nl + 1..]);
                    }
                    continue;
                }
                // `<head>\n@<id>`: put the pieces back in place
                if let Some(nl) = p.iter().position(|&b| b == b'\n') {
                    if let Some(id) = std::str::from_utf8(&p[nl + 1..])
                        .ok()
                        .and_then(|r| r.strip_prefix('@'))
                        .and_then(|r| r.parse::<u32>().ok())
                    {
                        if let Some(body) = self.pieces.remove(&id) {
                            let mut q = p[..=nl].to_vec();
                            q.extend_from_slice(&body);
                            return Got::Frame(k, q);
                        }
                    }
                }
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

    /// Read until a frame of `kind`, or an error. Other frames are given
    /// to `other`.
    pub async fn until(
        &mut self,
        kind: char,
        wait: Duration,
        mut other: impl FnMut(char, &[u8]),
    ) -> Result<Vec<u8>, String> {
        let end = Instant::now() + wait;
        loop {
            match self
                .read(end.saturating_duration_since(Instant::now()))
                .await
            {
                Got::Frame(k, p) if k == kind => return Ok(p),
                Got::Frame('E', p) => return Err(String::from_utf8_lossy(&p).into_owned()),
                Got::Frame(k, p) => other(k, &p),
                Got::Closed => return Err("closed".into()),
                Got::Nothing => return Err(format!("no {kind} in time")),
            }
        }
    }

    /// `N`: make a world. Returns (code, owner key, the `R wait` places seen).
    pub async fn create(
        &mut self,
        config: &str,
        wait: Duration,
    ) -> Result<(String, String, Vec<usize>), String> {
        let head = format!("{PROTO2} {}", self.token);
        self.send_big('N', &head, config.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut places = Vec::new();
        let end = Instant::now() + wait;
        loop {
            let p = self
                .until(
                    'R',
                    end.saturating_duration_since(Instant::now()),
                    |_, _| {},
                )
                .await?;
            let t = String::from_utf8_lossy(&p).into_owned();
            let (a, b) = t.split_once(' ').ok_or("bad R")?;
            if a == "wait" {
                places.push(b.parse().map_err(|_| "bad R wait")?);
                continue;
            }
            return Ok((a.to_string(), b.to_string(), places));
        }
    }

    /// `O`: an owner action. Returns the relay's `O` payload.
    pub async fn owner(&mut self, code: &str, key: &str, action: &str) -> Result<String, String> {
        self.send('O', format!("{PROTO2} {code} {key} {action}").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let p = self.until('O', Duration::from_secs(10), |_, _| {}).await?;
        Ok(String::from_utf8_lossy(&p).into_owned())
    }

    /// `I`: join by name or key, and read the push up to `W`.
    pub async fn join(&mut self, code: &str, who: &str, wait: Duration) -> Result<Push, String> {
        self.send(
            'I',
            format!("{PROTO2} {} {code} {who}", self.token).as_bytes(),
        )
        .await
        .map_err(|e| e.to_string())?;
        let p = self.until('S', wait, |_, _| {}).await?;
        let nl = p
            .iter()
            .position(|&b| b == b'\n')
            .ok_or("S has no newline")?;
        let head = String::from_utf8_lossy(&p[..nl]).into_owned();
        let (pl, key) = head.split_once(' ').ok_or("bad S")?;
        let data: Value =
            serde_json::from_slice(&p[nl + 1..]).map_err(|e| format!("S data: {e}"))?;
        let mut items = Vec::new();
        let w = self
            .until('W', Duration::from_secs(10), |k, p| {
                if k == 'Y' {
                    items.extend(parse_items(p));
                }
            })
            .await?;
        let w = String::from_utf8_lossy(&w).into_owned();
        Ok(Push {
            player: pl.parse().map_err(|_| "bad S player")?,
            key: key.to_string(),
            data,
            items,
            name: w
                .split_once(' ')
                .map(|x| x.1.to_string())
                .unwrap_or_default(),
        })
    }

    /// An empty `V`: no checks, but it confirms a new claim (the game kept
    /// its key).
    pub async fn confirm(&mut self) -> std::io::Result<()> {
        self.send('V', b"").await
    }

    /// `V` checks, in frames of at most 4096 bytes.
    pub async fn checks(&mut self, locs: &[u32]) -> std::io::Result<()> {
        let mut cur = String::new();
        for l in locs {
            let t = l.to_string();
            if !cur.is_empty() && cur.len() + 1 + t.len() > 4096 {
                self.send('V', cur.as_bytes()).await?;
                cur.clear();
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(&t);
        }
        if !cur.is_empty() {
            self.send('V', cur.as_bytes()).await?;
        }
        Ok(())
    }

    /// Read `Y` frames until `n` items came or `wait` passed. Other frames
    /// are skipped. Stops early on `E` or a close.
    pub async fn items(&mut self, n: usize, wait: Duration) -> Vec<(u32, u32, u8)> {
        let mut v = Vec::new();
        let end = Instant::now() + wait;
        while v.len() < n {
            match self
                .read(end.saturating_duration_since(Instant::now()))
                .await
            {
                Got::Frame('Y', p) => v.extend(parse_items(&p)),
                Got::Frame('E', _) | Got::Closed | Got::Nothing => break,
                Got::Frame(..) => {}
            }
        }
        v
    }
}

/// From a player's data (`S`): every location id, and how many items each
/// receiver gets from them: (locations, receiver -> count).
pub fn locations(data: &Value) -> (Vec<u32>, HashMap<u8, usize>) {
    let mut locs = Vec::new();
    let mut to = HashMap::new();
    if let Some(a) = data["data"]["locations"].as_array() {
        for l in a {
            let loc = l[0].as_u64().unwrap_or(0) as u32;
            let r = l[2].as_u64().unwrap_or(0) as u8;
            locs.push(loc);
            *to.entry(r).or_insert(0) += 1;
        }
    }
    (locs, to)
}
