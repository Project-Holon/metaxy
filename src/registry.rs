//! Sessions, players and the rules between them.
//!
//! All state sits behind one lock in `server.rs`. Every method here runs
//! to the end without waiting: frames go into per-connection queues.

use crate::codec::{self, encode, Fail, Frame};
use crate::config::Config;
use crate::outbox::{Outbox, Sent};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// A session code or peer number for a log line, left out when absent.
fn opt<T: std::fmt::Display>(v: Option<T>) -> Option<tracing::field::DisplayValue<T>> {
    v.map(tracing::field::display)
}

/// Round for a log or error text: whole numbers print without a dot.
fn num(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
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
    /// Send `E <reason> <detail>` and close.
    Error(Fail),
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
    created: Instant,
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
    /// Messages and bytes per second of the last window that was over.
    over_msgs: f64,
    over_bytes: f64,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    /// Dropped: which rate was exceeded.
    Drop(&'static str),
    /// Close with this detail.
    Close(String),
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
            over_msgs: 0.0,
            over_bytes: 0.0,
        }
    }

    fn close_detail(&self, cfg: &Config) -> String {
        let win = cfg.rate_window_ms as f64 / 1000.0;
        let secs = num(f64::from(self.streak) * win);
        let f = num(cfg.rate_factor);
        if self.over_msgs > cfg.rate_factor * cfg.msg_rate {
            format!(
                "over {f}x the message rate for {secs} s ({} per second, limit {})",
                self.over_msgs.round(),
                num(cfg.msg_rate)
            )
        } else {
            format!(
                "over {f}x the byte rate for {secs} s ({} bytes per second, limit {})",
                self.over_bytes.round(),
                num(cfg.byte_rate)
            )
        }
    }

    /// Account for one `D` frame of `len` bytes.
    fn offer(&mut self, len: usize, cfg: &Config, now: Instant) -> Verdict {
        let win = cfg.rate_window_ms as f64 / 1000.0;
        let elapsed = now.duration_since(self.win_start).as_secs_f64();
        if elapsed >= win {
            let over = self.win_msgs > cfg.rate_factor * cfg.msg_rate * win
                || self.win_bytes > cfg.rate_factor * cfg.byte_rate * win;
            if over {
                self.over_msgs = self.win_msgs / win;
                self.over_bytes = self.win_bytes / win;
            }
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
            return Verdict::Close(self.close_detail(cfg));
        }
        self.msgs.refill(cfg.msg_rate, cfg.msg_burst, now);
        self.bytes.refill(cfg.byte_rate, cfg.byte_burst, now);
        if self.msgs.tokens < 1.0 {
            Verdict::Drop("message rate")
        } else if self.bytes.tokens < len as f64 {
            Verdict::Drop("byte rate")
        } else {
            self.msgs.tokens -= 1.0;
            self.bytes.tokens -= len as f64;
            Verdict::Pass
        }
    }
}

/// Counts of what one client sent, for the debug log.
#[derive(Debug, Default)]
struct Traffic {
    frames_in: u64,
    bytes_in: u64,
    /// `D` frames dropped by the rate limits, and their payload bytes.
    dropped: u64,
    dropped_bytes: u64,
    /// Drops since the last debug line about them.
    pending: u64,
    last_drop_log: Option<Instant>,
}

#[derive(Debug)]
struct Conn {
    id: u64,
    out: Outbox,
    ip: IpAddr,
    listener_port: u16,
    tls: bool,
    session: Option<(String, u8)>,
    rate: Rate,
    traffic: Traffic,
}

impl Conn {
    /// Session code and peer number for a log line, none before a join.
    fn who(&self) -> (Option<&str>, Option<u8>) {
        match &self.session {
            Some((c, p)) => (Some(c.as_str()), Some(*p)),
            None => (None, None),
        }
    }

    fn log_traffic(&self, what: &str) {
        let (code, peer) = self.who();
        let o = &self.out.stats;
        debug!(
            conn = self.id,
            code = opt(code),
            peer,
            addr = %self.ip,
            frames_in = self.traffic.frames_in,
            bytes_in = self.traffic.bytes_in,
            frames_out = o.frames.load(Ordering::Relaxed),
            bytes_out = o.bytes.load(Ordering::Relaxed),
            d_dropped = self.traffic.dropped,
            d_dropped_bytes = self.traffic.dropped_bytes,
            queue_high = o.high.load(Ordering::Relaxed),
            "{what}"
        );
    }
}

/// Log that an `E` is being sent to this connection.
fn log_e(c: &Conn, reason: &str, detail: &str) {
    let (code, peer) = c.who();
    info!(conn = c.id, code = opt(code), peer, addr = %c.ip, reason, detail, "E sent");
}

/// Why a new connection was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused {
    /// Connections this address opened in the window.
    pub count: usize,
    pub window_ms: u64,
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
        if let Sent::TooSlow(detail) = c.out.send(frame.to_vec(), cap) {
            log_e(c, "too-slow", &detail);
        }
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
    pub fn admit(
        &mut self,
        ip: IpAddr,
        out: Outbox,
        listener_port: u16,
        tls: bool,
    ) -> Result<u64, Refused> {
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
            return Err(Refused {
                count: hist.len(),
                window_ms: self.cfg.conn_window_ms,
            });
        }
        hist.push_back(now);
        let id = self.next_id;
        self.next_id += 1;
        self.conns.insert(
            id,
            Conn {
                id,
                out,
                ip,
                listener_port,
                tls,
                session: None,
                rate: Rate::new(&self.cfg, now),
                traffic: Traffic::default(),
            },
        );
        Ok(id)
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
                info!(
                    code = %code,
                    age_s = now.duration_since(s.created).as_secs(),
                    why = %format!("empty for {} s", expiry.as_secs()),
                    "session forgotten"
                );
            }
            keep
        });
    }

    /// Debug lines with every connected peer's frame and byte counts.
    pub fn log_traffic(&self) {
        for c in self.conns.values().filter(|c| c.session.is_some()) {
            c.log_traffic("peer traffic");
        }
    }

    /// Ask every connection to stop with `E shutdown`.
    pub fn shutdown_all(&mut self) {
        let f = Fail::new("shutdown", "the relay is stopping");
        for c in self.conns.values() {
            if !c.out.ctl.killed.load(Ordering::Relaxed) {
                log_e(c, f.reason, &f.detail);
            }
            c.out.kill(Some(f.frame()), "server shutdown");
        }
    }

    /// Log an error that the connection task is about to send.
    pub fn report(&self, id: u64, e: &Fail) {
        if let Some(c) = self.conns.get(&id) {
            log_e(c, e.reason, &e.detail);
        }
    }

    /// A connection ended: drop its player and forget it. `cause` says
    /// why, for the log. Returns the session it was in, if any.
    pub fn disconnect(&mut self, id: u64, cause: &str) -> Option<(String, u8)> {
        let session = self.conns.get(&id).and_then(|c| c.session.clone());
        if let Some(c) = self.conns.get(&id) {
            c.log_traffic("peer traffic at disconnect");
        }
        self.leave(id, true, cause);
        self.conns.remove(&id);
        session
    }

    /// Handle one frame from connection `id`.
    pub fn on_frame(&mut self, id: u64, f: Frame) -> Outcome {
        let o = self.handle(id, f);
        if let Outcome::Error(e) = &o {
            self.report(id, e);
        }
        o
    }

    fn handle(&mut self, id: u64, f: Frame) -> Outcome {
        let in_session = match self.conns.get_mut(&id) {
            Some(c) => {
                c.traffic.frames_in += 1;
                c.traffic.bytes_in += f.payload.len() as u64;
                c.session.is_some()
            }
            None => return Outcome::Bye,
        };
        match f.kind {
            b'K' => Outcome::Ok,
            b'H' | b'J' => {
                let join = f.kind == b'J';
                if in_session {
                    return Outcome::Error(Fail::new(
                        "bad-state",
                        format!("{} after joining", if join { 'J' } else { 'H' }),
                    ));
                }
                let h = match codec::parse_hello(&f.payload, join) {
                    Ok(h) => h,
                    Err(r) => return Outcome::Error(r.into()),
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
                self.leave(id, false, "B");
                Outcome::Bye
            }
            b'Q' => {
                if !in_session {
                    return Outcome::Error(Fail::new("bad-state", "Q before joining"));
                }
                self.remove(id, &f.payload)
            }
            b'D' => {
                if !in_session {
                    return Outcome::Error(Fail::new("bad-state", "D before joining"));
                }
                self.data(id, &f.payload)
            }
            k => Outcome::Error(Fail::new(
                "bad-frame",
                format!("unknown frame type \"{}\"", codec::show(&[k], 1)),
            )),
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

    fn host(&mut self, id: u64, h: codec::Hello) -> Result<(), Fail> {
        let ip = self.conns[&id].ip;
        let hosted = self
            .sessions
            .values()
            .filter(|s| s.host_ip == Some(ip))
            .count();
        if hosted >= self.cfg.sessions_per_ip {
            return Err(Fail::new(
                "busy",
                format!(
                    "this address has {hosted} open sessions (limit {})",
                    self.cfg.sessions_per_ip
                ),
            ));
        }
        if self.sessions.len() >= self.cfg.max_sessions {
            return Err(Fail::new(
                "busy",
                format!(
                    "the relay holds {} sessions (limit {})",
                    self.sessions.len(),
                    self.cfg.max_sessions
                ),
            ));
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
                created: Instant::now(),
            },
        );
        self.conns.get_mut(&id).unwrap().session = Some((code.clone(), 1));
        let cap = self.cfg.queue_cap;
        tell(&self.conns, cap, id, &encode(b'C', code.as_bytes()));
        tell(&self.conns, cap, id, &peer_name(b'W', 1, &h.name));
        let c = &self.conns[&id];
        info!(
            conn = id,
            code = %code,
            peer = 1,
            host_addr = %ip,
            host_name = ?h.name,
            listener_port = c.listener_port,
            mode = if c.tls { "tls" } else { "plain" },
            "session created"
        );
        Ok(())
    }

    fn join(&mut self, id: u64, h: codec::Hello) -> Result<(), Fail> {
        let cap = self.cfg.queue_cap;
        let max_players = self.cfg.max_players;
        let Registry {
            conns, sessions, ..
        } = self;
        let ip = conns[&id].ip;
        let sess = sessions.get_mut(&h.code).ok_or_else(|| {
            Fail::new(
                "unknown-code",
                format!("no session has code {}", codec::show(h.code.as_bytes(), 16)),
            )
        })?;
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
                    _ => {
                        return Err(Fail::new(
                            "full",
                            format!(
                                "the session has {} of {max_players} players",
                                sess.slots.len()
                            ),
                        ))
                    }
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
                    c.out.kill(None, "replaced by a rejoin");
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
        info!(
            conn = id,
            code = %h.code,
            peer,
            name = ?name,
            addr = %ip,
            kind = if known.is_some() { "rejoin" } else { "new" },
            "join"
        );
        Ok(())
    }

    /// The player on `id` leaves (`keep` false) or drops (`keep` true,
    /// the slot and its number stay while the session is in memory).
    fn leave(&mut self, id: u64, keep: bool, cause: &str) {
        let cap = self.cfg.queue_cap;
        let Registry {
            conns, sessions, ..
        } = self;
        let Some(c) = conns.get_mut(&id) else {
            return;
        };
        let ip = c.ip;
        let Some((code, peer)) = c.session.take() else {
            return;
        };
        let Some(sess) = sessions.get_mut(&code) else {
            return;
        };
        let name = sess
            .slots
            .get(&peer)
            .map(|s| s.name.clone())
            .unwrap_or_default();
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
        if keep {
            info!(conn = id, code = %code, peer, name = ?name, addr = %ip, how = "drop", cause, "leave");
        } else {
            info!(conn = id, code = %code, peer, name = ?name, addr = %ip, how = "B", "leave");
        }
    }

    /// `Q <peer>`: the host removes a player. Ignored from anyone else.
    fn remove(&mut self, id: u64, payload: &[u8]) -> Outcome {
        let cap = self.cfg.queue_cap;
        let Registry {
            conns, sessions, ..
        } = self;
        let Some(c) = conns.get(&id) else {
            return Outcome::Ok;
        };
        let ip = c.ip;
        let Some((code, from)) = c.session.clone() else {
            return Outcome::Ok;
        };
        let ignored = |why: &str| {
            info!(conn = id, code = %code, peer = from, addr = %ip, why, "Q ignored");
            Outcome::Ok
        };
        if from != 1 {
            return ignored("the sender is not the host");
        }
        let target = match codec::parse_peer(payload) {
            Ok(t) => t,
            Err(e) => return Outcome::Error(e.into()),
        };
        if target == 1 {
            return ignored("the host cannot remove itself");
        }
        if !(1..=16).contains(&target) {
            return ignored("peer number is out of range");
        }
        let target = target as u8;
        let Some(sess) = sessions.get_mut(&code) else {
            return Outcome::Ok;
        };
        let Some(slot) = sess.slots.remove(&target) else {
            return ignored("no such peer in the session");
        };
        if let Some(c) = slot.conn.and_then(|c| conns.get_mut(&c)) {
            let f = Fail::new("removed", "by the host");
            log_e(c, f.reason, &f.detail);
            c.session = None;
            c.out.kill(Some(f.frame()), "removed by the host");
            for (_, o) in sess.connected() {
                tell(conns, cap, o, &encode(b'X', target.to_string().as_bytes()));
            }
        }
        info!(
            conn = id,
            code = %code,
            peer = from,
            addr = %ip,
            removed_peer = target,
            removed_name = ?slot.name,
            "Q removed a player"
        );
        Outcome::Ok
    }

    fn data(&mut self, id: u64, payload: &[u8]) -> Outcome {
        let d = match codec::parse_data(payload) {
            Ok(d) => d,
            Err(e) => return Outcome::Error(e.into()),
        };
        let cap = self.cfg.queue_cap;
        let Registry {
            conns,
            sessions,
            cfg,
            ..
        } = self;
        let conn = conns.get_mut(&id).unwrap();
        let now = Instant::now();
        match conn.rate.offer(payload.len(), cfg, now) {
            Verdict::Close(detail) => return Outcome::Error(Fail::new("rate", detail)),
            Verdict::Drop(why) => {
                let t = &mut conn.traffic;
                t.dropped += 1;
                t.dropped_bytes += payload.len() as u64;
                t.pending += 1;
                // One line a second at most, with the count since the last.
                if t.last_drop_log
                    .is_none_or(|l| now.duration_since(l) >= Duration::from_secs(1))
                {
                    let (code, peer) = conn.who();
                    debug!(
                        conn = id,
                        code = opt(code),
                        peer,
                        addr = %conn.ip,
                        dropped = conn.traffic.pending,
                        why,
                        "D frames dropped"
                    );
                    conn.traffic.pending = 0;
                    conn.traffic.last_drop_log = Some(now);
                }
                return Outcome::Ok;
            }
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
