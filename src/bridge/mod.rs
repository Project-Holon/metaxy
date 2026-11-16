//! The Archipelago bridge (docs/SPEC.md, Part D): one outbound WebSocket
//! per bridged player to an Archipelago server, messages passed both ways
//! as `T` frames. metaxy reads only the server's first message, to check
//! that it is `RoomInfo`; nothing the player sends passes before that.

pub mod room;
pub mod ws;

use crate::codec::{encode, Fail};
use crate::config::Config;
use crate::outbox::Outbox;
use room::Plan;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::info;

/// True for addresses on the public internet; loopback, private,
/// link-local, shared and other special ranges are not.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let o = a.octets();
            !(a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_multicast()
                || a.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || o[0] >= 240)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            !(a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// Bridge links open per address.
#[derive(Default)]
pub struct Bridge {
    per_ip: Mutex<HashMap<IpAddr, usize>>,
}

/// Held while a link is open; frees the address's place when dropped.
pub struct Slot {
    bridge: Arc<Bridge>,
    ip: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut m = self.bridge.per_ip.lock().unwrap();
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

impl Bridge {
    /// Take a place for a new link from `ip`, or `E busy`.
    pub fn slot(self: &Arc<Self>, ip: IpAddr, limit: usize) -> Result<Slot, Fail> {
        let mut m = self.per_ip.lock().unwrap();
        let n = m.entry(ip).or_default();
        if *n >= limit {
            return Err(Fail::new(
                "busy",
                format!("this address has {n} bridge links (limit {limit})"),
            ));
        }
        *n += 1;
        Ok(Slot {
            bridge: self.clone(),
            ip,
        })
    }
}

/// Is this the server's `RoomInfo`? Archipelago sends a JSON array of
/// commands; the first command of the first message must be `RoomInfo`.
fn is_room_info(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get(0).and_then(|c| c.get("cmd")).cloned())
        .is_some_and(|c| c == "RoomInfo")
}

/// Connect to what the link names and wait for `RoomInfo`. Returns the
/// two sides and the first message.
async fn connect(link: &str, cfg: &Config) -> Result<(ws::Reader, ws::Writer, String), String> {
    let allow_private = cfg.bridge_private;
    let check = move |ip: IpAddr| allow_private || public(ip);
    let targets = match room::plan(link)? {
        Plan::Direct(t) => t,
        Plan::Room {
            tls,
            host,
            port,
            room,
        } => {
            let p = room::room_port(tls, &host, port, &room, &check).await?;
            vec![room::Target {
                tls,
                host,
                port: p,
                path: "/".into(),
            }]
        }
    };
    let mut last = String::new();
    let n = targets.len();
    // an attempt that is not the last may take a third of the set-up time
    // (a TLS hello to a plain server may get no answer at all)
    let part = Duration::from_millis(cfg.bridge_setup_ms / 3);
    for (i, t) in targets.into_iter().enumerate() {
        let r = if i + 1 < n {
            tokio::time::timeout(part, attempt(&t, cfg, &check))
                .await
                .unwrap_or_else(|_| Err(format!("{}:{}: no answer", t.host, t.port)))
        } else {
            attempt(&t, cfg, &check).await
        };
        match r {
            Ok(x) => return Ok(x),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// One target: TCP (and TLS), the upgrade, and the first message.
async fn attempt(
    t: &room::Target,
    cfg: &Config,
    check: &(dyn Fn(IpAddr) -> bool + Sync),
) -> Result<(ws::Reader, ws::Writer, String), String> {
    let s = ws::open(t.tls, &t.host, t.port, check).await?;
    let host = if t.port == if t.tls { 443 } else { 80 } {
        t.host.clone()
    } else {
        format!("{}:{}", t.host, t.port)
    };
    let (mut r, w) = ws::upgrade(s, &host, &t.path, cfg.bridge_msg_max).await?;
    let first = match r.next().await? {
        ws::Msg::Text(t) => t,
        m => {
            return Err(format!(
                "the server's first message is not RoomInfo ({m:?})"
            ))
        }
    };
    if !is_room_info(&first) {
        return Err("the server's first message is not RoomInfo".into());
    }
    Ok((r, w, first))
}

/// An open link.
pub struct Link {
    tx: mpsc::UnboundedSender<Out>,
    reader: JoinHandle<()>,
    _slot: Slot,
}

enum Out {
    Text(String),
    Pong(Vec<u8>),
}

impl Drop for Link {
    fn drop(&mut self) {
        // the reader holds the other sender: the writer then closes
        self.reader.abort();
    }
}

impl Link {
    /// Pass one message from the player to the server.
    pub fn send(&self, text: String) {
        let _ = self.tx.send(Out::Text(text));
    }
}

/// Send `T` (in `F` pieces when big) to the player, waiting while the
/// player's queue is full instead of dropping or closing.
async fn to_player(out: &Outbox, cfg: &Config, text: &str, next_id: &mut u32) -> bool {
    let frames = if text.len() <= cfg.max_payload {
        vec![encode(b'T', text.as_bytes())]
    } else {
        *next_id += 1;
        let id = *next_id;
        let mut v = Vec::new();
        let size = crate::world::frames::PIECE;
        let n = text.len().div_ceil(size);
        for (i, c) in text.as_bytes().chunks(size).enumerate() {
            let mut p = format!("{id} {}\n", u8::from(i + 1 == n)).into_bytes();
            p.extend_from_slice(c);
            v.push(encode(b'F', &p));
        }
        v.push(encode(b'T', format!("@{id}").as_bytes()));
        v
    };
    let room = cfg.queue_cap / 2;
    for f in frames {
        let since = Instant::now();
        while out.queued.load(Ordering::Relaxed) + f.len() > room {
            if out.ctl.killed.load(Ordering::Relaxed) || since.elapsed() > Duration::from_secs(30) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if out.send(f, cfg.queue_cap) != crate::outbox::Sent::Ok {
            return false;
        }
    }
    true
}

/// Open a link for a player: connect within the set-up time, then pass
/// messages both ways. The server's `RoomInfo` goes to the player first.
pub async fn open(
    link: &str,
    cfg: Arc<Config>,
    out: Outbox,
    slot: Slot,
    conn: u64,
) -> Result<Link, Fail> {
    let setup = Duration::from_millis(cfg.bridge_setup_ms);
    let (mut r, mut w, first) = match tokio::time::timeout(setup, connect(link, &cfg)).await {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return Err(Fail::new("ap-unreachable", e)),
        Err(_) => {
            return Err(Fail::new(
                "ap-unreachable",
                format!("no RoomInfo within {} s", setup.as_secs_f64()),
            ))
        }
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let r = match m {
                Out::Text(t) => w.text(&t).await,
                Out::Pong(p) => w.pong(&p).await,
            };
            if r.is_err() {
                break;
            }
        }
        w.close().await;
    });
    out.send(encode(b'L', b"ok"), cfg.queue_cap);
    let tx2 = tx.clone();
    let reader = tokio::spawn(async move {
        let mut next_id = 0u32;
        if !to_player(&out, &cfg, &first, &mut next_id).await {
            return;
        }
        let why = loop {
            match r.next().await {
                Ok(ws::Msg::Text(t)) => {
                    if !to_player(&out, &cfg, &t, &mut next_id).await {
                        return;
                    }
                }
                Ok(ws::Msg::Ping(p)) => {
                    let _ = tx2.send(Out::Pong(p));
                }
                Ok(ws::Msg::Close) => break "the server closed the link".to_string(),
                Err(e) => break e,
            }
        };
        let f = Fail::new("ap-closed", why);
        info!(
            conn,
            reason = f.reason,
            detail = f.detail.as_str(),
            "E sent"
        );
        out.kill(Some(f.frame()), "ap-closed");
    });
    Ok(Link {
        tx,
        reader,
        _slot: slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "172.20.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(!public(a.parse().unwrap()), "{a}");
        }
        for a in ["8.8.8.8", "104.21.0.1", "2606:4700::1"] {
            assert!(public(a.parse().unwrap()), "{a}");
        }
    }

    #[test]
    fn room_info_check() {
        assert!(is_room_info(r#"[{"cmd": "RoomInfo", "version": {}}]"#));
        assert!(!is_room_info(r#"[{"cmd": "Print"}]"#));
        assert!(!is_room_info(r#"{"cmd": "RoomInfo"}"#));
        assert!(!is_room_info("RoomInfo"));
    }
}
