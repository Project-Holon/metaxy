//! Sessions, players and the rules between them.
//!
//! All state sits behind one lock in `server.rs`. Every method here runs
//! to the end without waiting: frames go into per-connection queues.

use crate::codec::{self, encode, Frame};
use crate::config::Config;
use crate::outbox::Outbox;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// Print one log line. Never call this with message contents.
pub fn log(msg: &str) {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    eprintln!("[{t}] {msg}");
}

/// What a frame did, for the connection task.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Carry on.
    Ok,
    /// The connection is now in a session.
    Joined,
    /// The player left: close without a frame.
    Bye,
    /// Send `E <reason>` and close.
    Error(&'static str),
}

#[derive(Debug)]
struct Slot {
    token: String,
    name: String,
    /// The connection of the player, or none after a drop. A dropped
    /// player keeps the number while the session is in memory.
    conn: Option<u64>,
}

#[derive(Debug)]
struct Session {
    slots: BTreeMap<u8, Slot>,
    host_ip: Option<IpAddr>,
    /// Set while no player is connected.
    empty_since: Option<Instant>,
}

impl Session {
    fn connected(&self) -> Vec<(u8, u64)> {
        self.slots
            .iter()
            .filter_map(|(p, s)| s.conn.map(|c| (*p, c)))
            .collect()
    }
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn refill(&mut self, rate: f64, burst: f64, now: Instant) {
        let dt = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * rate).min(burst);
        self.last = now;
    }
}

#[derive(Debug)]
struct Rate {
    msgs: Bucket,
    bytes: Bucket,
    win_start: Instant,
    win_msgs: f64,
    win_bytes: f64,
    streak: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Drop,
    Close,
}

impl Rate {
    fn new(cfg: &Config, now: Instant) -> Rate {
        Rate {
            msgs: Bucket {
                tokens: cfg.msg_burst,
                last: now,
            },
            bytes: Bucket {
                tokens: cfg.byte_burst,
                last: now,
            },
            win_start: now,
            win_msgs: 0.0,
            win_bytes: 0.0,
            streak: 0,
        }
    }

    /// Account for one `D` frame of `len` bytes.
    fn offer(&mut self, len: usize, cfg: &Config, now: Instant) -> Verdict {
        let win = cfg.rate_window_ms as f64 / 1000.0;
        let elapsed = now.duration_since(self.win_start).as_secs_f64();
        if elapsed >= win {
            let over = self.win_msgs > cfg.rate_factor * cfg.msg_rate * win
                || self.win_bytes > cfg.rate_factor * cfg.byte_rate * win;
            if over && elapsed < 2.0 * win {
                self.streak += 1;
            } else {
                self.streak = 0;
            }
            self.win_start = now;
            self.win_msgs = 0.0;
            self.win_bytes = 0.0;
        }
        self.win_msgs += 1.0;
        self.win_bytes += len as f64;
        if self.streak >= cfg.rate_close_windows {
            return Verdict::Close;
        }
        self.msgs.refill(cfg.msg_rate, cfg.msg_burst, now);
        self.bytes.refill(cfg.byte_rate, cfg.byte_burst, now);
        if self.msgs.tokens >= 1.0 && self.bytes.tokens >= len as f64 {
            self.msgs.tokens -= 1.0;
            self.bytes.tokens -= len as f64;
            Verdict::Pass
        } else {
            Verdict::Drop
        }
    }
}

#[derive(Debug)]
struct Conn {
    out: Outbox,
    ip: IpAddr,
    session: Option<(String, u8)>,
    rate: Rate,
}

/// Everything the relay knows.
pub struct Registry {
    cfg: Arc<Config>,
    conns: HashMap<u64, Conn>,
    sessions: HashMap<String, Session>,
    ip_hist: HashMap<IpAddr, VecDeque<Instant>>,
    next_id: u64,
}

fn tell(conns: &HashMap<u64, Conn>, cap: usize, id: u64, frame: &[u8]) {
    if let Some(c) = conns.get(&id) {
        c.out.send(frame.to_vec(), cap);
    }
}

fn peer_name(kind: u8, peer: u8, name: &str) -> Vec<u8> {
    encode(kind, format!("{peer} {name}").as_bytes())
}

impl Registry {
    /// Make an empty registry.
    pub fn new(cfg: Arc<Config>) -> Registry {
        Registry {
            cfg,
            conns: HashMap::new(),
            sessions: HashMap::new(),
            ip_hist: HashMap::new(),
            next_id: 1,
        }
    }

    /// Count of sessions kept.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Count of open connections.
    pub fn conn_count(&self) -> usize {
        self.conns.len()
    }

    /// Admit a new connection unless the address is over its rate.
    pub fn admit(&mut self, ip: IpAddr, out: Outbox) -> Option<u64> {
        let now = Instant::now();
        let window = Duration::from_millis(self.cfg.conn_window_ms);
        let hist = self.ip_hist.entry(ip).or_default();
        while hist
            .front()
            .is_some_and(|t| now.duration_since(*t) > window)
        {
            hist.pop_front();
        }
        if hist.len() >= self.cfg.conn_per_window {
            return None;
        }
        hist.push_back(now);
        let id = self.next_id;
        self.next_id += 1;
        self.conns.insert(
            id,
            Conn {
                out,
                ip,
                session: None,
                rate: Rate::new(&self.cfg, now),
            },
        );
        Some(id)
    }

    /// Forget old per-address records and end sessions that expired.
    pub fn sweep(&mut self) {
        let now = Instant::now();
        let window = Duration::from_millis(self.cfg.conn_window_ms);
        self.ip_hist.retain(|_, h| {
            while h.front().is_some_and(|t| now.duration_since(*t) > window) {
                h.pop_front();
            }
            !h.is_empty()
        });
        let expiry = Duration::from_millis(self.cfg.session_expiry_ms);
        self.sessions.retain(|code, s| {
            let keep = s
                .empty_since
                .is_none_or(|t| now.duration_since(t) <= expiry);
            if !keep {
                log(&format!("session {code} expired"));
            }
            keep
        });
    }

    /// Ask every connection to stop with `E shutdown`.
    pub fn shutdown_all(&mut self) {
        for c in self.conns.values() {
            c.out.kill(Some(encode(b'E', b"shutdown")));
        }
    }

    /// A connection ended: drop its player and forget it.
    pub fn disconnect(&mut self, id: u64) {
        self.leave(id, true);
        self.conns.remove(&id);
    }

    /// Handle one frame from connection `id`.
    pub fn on_frame(&mut self, id: u64, f: Frame) -> Outcome {
        let in_session = match self.conns.get(&id) {
            Some(c) => c.session.is_some(),
            None => return Outcome::Bye,
        };
        match f.kind {
            b'K' => Outcome::Ok,
            b'H' | b'J' => {
                if in_session {
                    return Outcome::Error("bad-state");
                }
                let join = f.kind == b'J';
                let h = match codec::parse_hello(&f.payload, join) {
                    Ok(h) => h,
                    Err(r) => return Outcome::Error(r.reason()),
                };
                let r = if join {
                    self.join(id, h)
                } else {
                    self.host(id, h)
                };
                match r {
                    Ok(()) => Outcome::Joined,
                    Err(e) => Outcome::Error(e),
                }
            }
            b'B' => {
                self.leave(id, false);
                Outcome::Bye
            }
            b'Q' => {
                if !in_session {
                    return Outcome::Error("bad-state");
                }
                self.remove(id, &f.payload)
            }
            b'D' => {
                if !in_session {
                    return Outcome::Error("bad-state");
                }
                self.data(id, &f.payload)
            }
            _ => Outcome::Error("bad-frame"),
        }
    }

    fn new_code(&self) -> String {
        loop {
            let mut raw = [0u8; 16];
            getrandom::getrandom(&mut raw).expect("OS random source");
            // Bytes under 248 map evenly onto the 31 characters.
            let code: String = raw
                .iter()
                .filter(|b| **b < 248)
                .take(6)
                .map(|b| CODE_ALPHABET[(*b % 31) as usize] as char)
                .collect();
            if code.len() == 6 && !self.sessions.contains_key(&code) {
                return code;
            }
        }
    }

    fn host(&mut self, id: u64, h: codec::Hello) -> Result<(), &'static str> {
        let ip = self.conns[&id].ip;
        let hosted = self
            .sessions
            .values()
            .filter(|s| s.host_ip == Some(ip))
            .count();
        if hosted >= self.cfg.sessions_per_ip || self.sessions.len() >= self.cfg.max_sessions {
            return Err("busy");
        }
        let code = self.new_code();
        let mut slots = BTreeMap::new();
        slots.insert(
            1,
            Slot {
                token: h.token,
                name: h.name.clone(),
                conn: Some(id),
            },
        );
        self.sessions.insert(
            code.clone(),
            Session {
                slots,
                host_ip: Some(ip),
                empty_since: None,
            },
        );
        self.conns.get_mut(&id).unwrap().session = Some((code.clone(), 1));
        let cap = self.cfg.queue_cap;
        tell(&self.conns, cap, id, &encode(b'C', code.as_bytes()));
        tell(&self.conns, cap, id, &peer_name(b'W', 1, &h.name));
        log(&format!("session {code} created from {ip}"));
        Ok(())
    }

    fn join(&mut self, id: u64, h: codec::Hello) -> Result<(), &'static str> {
        let cap = self.cfg.queue_cap;
        let max_players = self.cfg.max_players;
        let Registry {
            conns, sessions, ..
        } = self;
        let sess = sessions.get_mut(&h.code).ok_or("unknown-code")?;
        let known = sess
            .slots
            .iter()
            .find(|(_, s)| s.token == h.token)
            .map(|(p, _)| *p);
        let peer = match known {
            Some(p) => p,
            None => {
                let free = (1..=16u8).find(|p| !sess.slots.contains_key(p));
                match free {
                    Some(p) if sess.slots.len() < max_players => p,
                    _ => return Err("full"),
                }
            }
        };
        let mut old_was_open = false;
        if known.is_some() {
            // Same player on a new connection: close the old one quietly.
            // No X was sent for it, so no P is sent for the new one.
            if let Some(old) = sess.slots.get_mut(&peer).and_then(|s| s.conn.take()) {
                if let Some(c) = conns.get_mut(&old) {
                    c.session = None;
                    c.out.kill(None);
                    old_was_open = true;
                }
            }
        } else {
            sess.slots.insert(
                peer,
                Slot {
                    token: h.token.clone(),
                    name: h.name.clone(),
                    conn: None,
                },
            );
        }
        let slot = sess.slots.get_mut(&peer).unwrap();
        slot.conn = Some(id);
        slot.name = h.name.clone();
        let name = slot.name.clone();
        sess.empty_since = None;
        conns.get_mut(&id).unwrap().session = Some((h.code.clone(), peer));
        tell(conns, cap, id, &peer_name(b'W', peer, &name));
        for (p, c) in sess.connected() {
            if p == peer {
                continue;
            }
            let other = &sess.slots[&p].name;
            tell(conns, cap, id, &peer_name(b'P', p, other));
            if !old_was_open {
                tell(conns, cap, c, &peer_name(b'P', peer, &name));
            }
        }
        log(&format!("peer {peer} joined {}", h.code));
        Ok(())
    }

    /// The player on `id` leaves (`keep` false) or drops (`keep` true,
    /// the slot and its number stay while the session is in memory).
    fn leave(&mut self, id: u64, keep: bool) {
        let cap = self.cfg.queue_cap;
        let Registry {
            conns, sessions, ..
        } = self;
        let Some((code, peer)) = conns.get_mut(&id).and_then(|c| c.session.take()) else {
            return;
        };
        let Some(sess) = sessions.get_mut(&code) else {
            return;
        };
        if keep {
            if let Some(s) = sess.slots.get_mut(&peer) {
                s.conn = None;
            }
        } else {
            sess.slots.remove(&peer);
        }
        let others = sess.connected();
        for (_, c) in &others {
            tell(conns, cap, *c, &encode(b'X', peer.to_string().as_bytes()));
        }
        if others.is_empty() {
            sess.empty_since = Some(Instant::now());
        }
        log(&format!("peer {peer} left {code}"));
    }

    /// `Q <peer>`: the host removes a player. Ignored from anyone else.
    fn remove(&mut self, id: u64, payload: &[u8]) -> Outcome {
        let cap = self.cfg.queue_cap;
        let Registry {
            conns, sessions, ..
        } = self;
        let Some((code, from)) = conns.get(&id).and_then(|c| c.session.clone()) else {
            return Outcome::Ok;
        };
        if from != 1 {
            return Outcome::Ok;
        }
        let target = match codec::parse_peer(payload) {
            Ok(t) => t,
            Err(e) => return Outcome::Error(e.reason()),
        };
        if target == 1 || !(1..=16).contains(&target) {
            return Outcome::Ok;
        }
        let target = target as u8;
        let Some(sess) = sessions.get_mut(&code) else {
            return Outcome::Ok;
        };
        let Some(slot) = sess.slots.remove(&target) else {
            return Outcome::Ok;
        };
        if let Some(c) = slot.conn.and_then(|c| conns.get_mut(&c)) {
            c.session = None;
            c.out.kill(Some(encode(b'E', b"removed")));
            for (_, o) in sess.connected() {
                tell(conns, cap, o, &encode(b'X', target.to_string().as_bytes()));
            }
        }
        log(&format!("peer {target} removed from {code}"));
        Outcome::Ok
    }

    fn data(&mut self, id: u64, payload: &[u8]) -> Outcome {
        let d = match codec::parse_data(payload) {
            Ok(d) => d,
            Err(e) => return Outcome::Error(e.reason()),
        };
        let cap = self.cfg.queue_cap;
        let Registry {
            conns,
            sessions,
            cfg,
            ..
        } = self;
        let conn = conns.get_mut(&id).unwrap();
        match conn.rate.offer(payload.len(), cfg, Instant::now()) {
            Verdict::Close => return Outcome::Error("rate"),
            Verdict::Drop => return Outcome::Ok,
            Verdict::Pass => {}
        }
        let Some((code, from)) = conn.session.clone() else {
            return Outcome::Ok;
        };
        let Some(sess) = sessions.get(&code) else {
            return Outcome::Ok;
        };
        let mut body = from.to_string().into_bytes();
        body.push(b'\n');
        body.extend_from_slice(&d.rest);
        let frame = encode(b'D', &body);
        for (p, c) in sess.connected() {
            let wanted = if d.peer == 0 {
                p != from
            } else {
                u32::from(p) == d.peer && p != from
            };
            if wanted {
                tell(conns, cap, c, &frame);
            }
        }
        Outcome::Ok
    }
}
