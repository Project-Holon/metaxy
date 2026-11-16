//! World sessions: creation, owner actions, joins and keys, checks routed
//! to items, positions.
//!
//! Each world has its own lock; nothing here takes the relay's registry
//! lock. Calls that write to disk run on blocking threads (the server uses
//! `spawn_blocking`); generation runs on the pool in `gen`.

use super::frames::{self, Action, Checks, Pos};
use super::gen::{self, GenConfig, Pool, World};
use super::store::{key_hash, now_s, GenId, Line, Meta, State, Store, FORMAT};
use crate::codec::{encode, Fail};
use crate::config::Config;
use crate::outbox::{Outbox, Sent};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// 32 hex digits from the OS random source.
fn new_key() -> String {
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).expect("OS random source");
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_key(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn random_u64() -> u64 {
    let mut raw = [0u8; 8];
    getrandom::getrandom(&mut raw).expect("OS random source");
    u64::from_le_bytes(raw)
}

/// A world made again in memory, with each player's placements.
struct Built {
    world: Arc<World>,
    /// Per player (index 0 is player 1): location -> (item, receiver).
    place: Vec<HashMap<u32, (u32, u8)>>,
}

impl Built {
    fn new(world: Arc<World>) -> Built {
        let place = world
            .players
            .iter()
            .map(|p| {
                p.locations
                    .iter()
                    .map(|l| (l[0], (l[1], l[2] as u8)))
                    .collect()
            })
            .collect();
        Built { world, place }
    }
}

/// A connected player.
struct Seat {
    conn: u64,
    out: Outbox,
    pos: Option<Pos>,
    tokens: f64,
    last: Instant,
}

struct Inner {
    state: State,
    deleted: bool,
    built: Option<Built>,
    /// Set when the world made again does not match its stored hash.
    mismatch: Option<String>,
    /// Index 0 is player 1.
    seats: Vec<Option<Seat>>,
    empty_since: Instant,
}

impl Inner {
    fn connected(&self) -> impl Iterator<Item = (u8, &Seat)> {
        self.seats
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i as u8 + 1, s)))
    }

    fn nobody(&self) -> bool {
        self.seats.iter().all(Option::is_none)
    }
}

/// One stored world.
pub struct Entry {
    pub code: String,
    meta: Meta,
    inner: Mutex<Inner>,
    /// Held while the world is made again, so it is made once.
    load: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct IpBook {
    gens: HashMap<IpAddr, VecDeque<Instant>>,
    owned: HashMap<IpAddr, HashSet<String>>,
}

/// Every world, the store and the generation pool.
pub struct Worlds {
    cfg: Arc<Config>,
    store: Store,
    pub pool: Arc<Pool>,
    /// False when the store is a temporary folder.
    pub persistent: bool,
    map: Mutex<HashMap<String, Arc<Entry>>>,
    ips: Mutex<IpBook>,
}

/// A joined player: the world and the player number.
#[derive(Clone)]
pub struct Joined {
    pub entry: Arc<Entry>,
    pub player: u8,
}

fn send(out: &Outbox, cap: usize, frame: Vec<u8>) {
    if let Sent::TooSlow(detail) = out.send(frame, cap) {
        info!(reason = "too-slow", detail = detail.as_str(), "E sent");
    }
}

fn store_err(code: &str, e: std::io::Error) -> Fail {
    error!(code, error = %e, "world store write failed");
    Fail::new("unavailable", "the world store cannot be written")
}

impl Worlds {
    /// Open the store and load every world in it.
    pub fn open(cfg: Arc<Config>, store: Store, persistent: bool) -> Arc<Worlds> {
        let workers = if cfg.gen_workers == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1))
                .unwrap_or(1)
                .max(1)
        } else {
            cfg.gen_workers
        };
        let pool = Pool::start(
            workers,
            cfg.gen_queue,
            Duration::from_millis(cfg.gen_time_ms),
        );
        let mut map = HashMap::new();
        let mut skipped = 0;
        for l in store.load() {
            skipped += l.skipped;
            let n = l.meta.players.len();
            let code = l.meta.code.clone();
            map.insert(
                code.clone(),
                Arc::new(Entry {
                    code,
                    meta: l.meta,
                    inner: Mutex::new(Inner {
                        state: l.state,
                        deleted: false,
                        built: None,
                        mismatch: None,
                        seats: (0..n).map(|_| None).collect(),
                        empty_since: Instant::now(),
                    }),
                    load: tokio::sync::Mutex::new(()),
                }),
            );
        }
        info!(
            worlds = map.len(),
            lines_skipped = skipped,
            path = %store.root.display(),
            persistent,
            gen_workers = workers,
            generator = %format!("{} {}", gen::CRATE, gen::version()),
            "world store loaded"
        );
        Arc::new(Worlds {
            cfg,
            store,
            pool,
            persistent,
            map: Mutex::new(map),
            ips: Mutex::new(IpBook::default()),
        })
    }

    /// Worlds kept now.
    pub fn count(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    fn get(&self, code: &str) -> Result<Arc<Entry>, Fail> {
        self.map.lock().unwrap().get(code).cloned().ok_or_else(|| {
            Fail::new(
                "no-world",
                format!(
                    "no world has code {}",
                    crate::codec::show(code.as_bytes(), 16)
                ),
            )
        })
    }

    fn new_code(&self) -> String {
        let map = self.map.lock().unwrap();
        loop {
            let mut raw = [0u8; 16];
            getrandom::getrandom(&mut raw).expect("OS random source");
            let code: String = raw
                .iter()
                .filter(|b| **b < 248)
                .take(6)
                .map(|b| CODE_ALPHABET[(*b % 31) as usize] as char)
                .collect();
            if code.len() == 6 && !map.contains_key(&code) && !self.store.exists(&code) {
                return code;
            }
        }
    }

    /// Check a configuration (C3): JSON, schema, 1 to 16 players with
    /// distinct names, Black or White. A missing seed is drawn here.
    fn check_config(&self, bytes: &[u8]) -> Result<(GenConfig, Value), Fail> {
        let failed = |d: String| Fail::new("gen-failed", d);
        if bytes.len() > self.cfg.world_config_max {
            return Err(Fail::new(
                "too-big",
                format!(
                    "configuration of {} bytes (limit {})",
                    bytes.len(),
                    self.cfg.world_config_max
                ),
            ));
        }
        let mut v: Value = serde_json::from_slice(bytes)
            .map_err(|e| failed(format!("the configuration is not JSON: {e}")))?;
        let obj = v
            .as_object_mut()
            .ok_or_else(|| failed("the configuration is not a JSON object".into()))?;
        if obj.get("seed").is_none_or(Value::is_null) {
            obj.insert("seed".into(), Value::from(random_u64()));
        }
        let players = obj
            .get("players")
            .and_then(Value::as_array)
            .ok_or_else(|| failed("the configuration has no players".into()))?;
        if players.is_empty() || players.len() > self.cfg.max_players.min(16) {
            return Err(failed(format!(
                "{} players (1 to {})",
                players.len(),
                self.cfg.max_players.min(16)
            )));
        }
        let mut names = HashSet::new();
        for p in players {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty()
                || name.len() > crate::codec::MAX_NAME
                || name.chars().any(char::is_control)
            {
                return Err(failed(
                    "each player needs a name of 1 to 64 bytes without control characters".into(),
                ));
            }
            if is_key(name) {
                return Err(failed(format!("the name {name} looks like a player key")));
            }
            if !names.insert(name) {
                return Err(failed(format!("two players named {name}")));
            }
        }
        let cfg: GenConfig = serde_json::from_value(v)
            .map_err(|e| failed(format!("the configuration does not read: {e}")))?;
        if cfg.schema != arkh_gen::gen::SCHEMA {
            return Err(failed(format!(
                "configuration schema {} (this generator reads {})",
                cfg.schema,
                arkh_gen::gen::SCHEMA
            )));
        }
        // stored as the generator reads it
        let canon = serde_json::to_value(&cfg).map_err(|e| failed(e.to_string()))?;
        Ok((cfg, canon))
    }

    /// `N`: check, queue, make and store a world. Sends `R wait <n>` while
    /// it waits and returns the final `R` frame.
    pub async fn create(
        self: &Arc<Self>,
        ip: IpAddr,
        out: &Outbox,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, Fail> {
        let (cfg, canon) = self.check_config(&body)?;
        let cap = self.cfg.queue_cap;
        {
            let mut ips = self.ips.lock().unwrap();
            let now = Instant::now();
            let hour = Duration::from_secs(3600);
            let h = ips.gens.entry(ip).or_default();
            while h.front().is_some_and(|t| now.duration_since(*t) > hour) {
                h.pop_front();
            }
            if h.len() >= self.cfg.gen_per_hour {
                return Err(Fail::new(
                    "busy",
                    format!(
                        "this address made {} worlds in the last hour (limit {})",
                        h.len(),
                        self.cfg.gen_per_hour
                    ),
                ));
            }
            let owned = ips.owned.get(&ip).map_or(0, HashSet::len);
            if owned >= self.cfg.worlds_per_ip {
                return Err(Fail::new(
                    "busy",
                    format!(
                        "this address has {owned} open worlds (limit {})",
                        self.cfg.worlds_per_ip
                    ),
                ));
            }
            ips.gens.entry(ip).or_default().push_back(now);
        }
        let o = out.clone();
        let on_wait: gen::OnWait =
            Box::new(move |n| send(&o, cap, encode(b'R', format!("wait {n}").as_bytes())));
        let ticket = self
            .pool
            .submit(cfg.clone(), false, Some(on_wait))
            .map_err(|f| {
                Fail::new(
                    "gen-busy",
                    format!(
                        "{} generations are waiting (limit {})",
                        f.queued, self.cfg.gen_queue
                    ),
                )
            })?;
        let started = Instant::now();
        let (world, hash) = ticket
            .wait(self.pool.time_limit)
            .await
            .map_err(|e| Fail::new("gen-failed", e))?;
        for p in &world.players {
            let n = serde_json::to_vec(p).map(|v| v.len()).unwrap_or(usize::MAX);
            if n > self.cfg.world_data_max {
                return Err(Fail::new(
                    "gen-failed",
                    format!(
                        "player {} has {n} bytes of data (limit {})",
                        p.slot, self.cfg.world_data_max
                    ),
                ));
            }
        }
        let code = self.new_code();
        let owner = new_key();
        let meta = Meta {
            format: FORMAT.into(),
            code: code.clone(),
            created: now_s(),
            generator: GenId {
                krate: gen::CRATE.into(),
                version: world.generator.clone(),
                data_hash: world.data_hash.clone(),
            },
            seed: cfg.seed,
            world_hash: hash,
            owner_key: key_hash(&owner),
            config: canon,
            players: cfg.players.iter().map(|p| p.name.clone()).collect(),
        };
        let me = self.clone();
        let m2 = meta.clone();
        tokio::task::spawn_blocking(move || me.store.create(&m2))
            .await
            .map_err(|e| Fail::new("unavailable", e.to_string()))?
            .map_err(|e| store_err(&code, e))?;
        let n = meta.players.len();
        let entry = Arc::new(Entry {
            code: code.clone(),
            inner: Mutex::new(Inner {
                state: State::new(&meta),
                deleted: false,
                built: Some(Built::new(world)),
                mismatch: None,
                seats: (0..n).map(|_| None).collect(),
                empty_since: Instant::now(),
            }),
            meta,
            load: tokio::sync::Mutex::new(()),
        });
        self.map.lock().unwrap().insert(code.clone(), entry);
        self.ips
            .lock()
            .unwrap()
            .owned
            .entry(ip)
            .or_default()
            .insert(code.clone());
        info!(
            code = %code,
            players = n,
            gen_ms = started.elapsed().as_millis() as u64,
            addr = %ip,
            "world created"
        );
        Ok(encode(b'R', format!("{code} {owner}").as_bytes()))
    }

    fn forget_owned(&self, code: &str) {
        let mut ips = self.ips.lock().unwrap();
        for s in ips.owned.values_mut() {
            s.remove(code);
        }
        ips.owned.retain(|_, s| !s.is_empty());
    }

    /// `O`: start, close or delete (blocking: writes to disk).
    pub fn owner(&self, code: &str, key: &str, action: Action) -> Result<Vec<u8>, Fail> {
        let entry = self.get(code)?;
        if key_hash(key) != entry.meta.owner_key {
            return Err(Fail::new("not-owner", "the owner key is wrong"));
        }
        let mut g = entry.inner.lock().unwrap();
        if g.deleted {
            return Err(Fail::new("no-world", format!("no world has code {code}")));
        }
        let t = now_s();
        match action {
            Action::Start => {
                if !g.state.started || g.state.closed {
                    let l = Line::Start { t };
                    self.store
                        .append(code, std::slice::from_ref(&l))
                        .map_err(|e| store_err(code, e))?;
                    g.state.apply(&l);
                }
            }
            Action::Close => {
                if !g.state.closed {
                    let l = Line::Close { t };
                    self.store
                        .append(code, std::slice::from_ref(&l))
                        .map_err(|e| store_err(code, e))?;
                    g.state.apply(&l);
                }
                drop(g);
                self.forget_owned(code);
            }
            Action::Delete => {
                self.drop_world(&entry, &mut g, "deleted by the owner");
                drop(g);
            }
        }
        info!(code, action = action.done(), "world owner action");
        Ok(encode(b'O', format!("{code} {}", action.done()).as_bytes()))
    }

    /// Delete a world now: players get `E no-world`, the folder goes.
    fn drop_world(&self, entry: &Entry, g: &mut Inner, why: &str) {
        g.deleted = true;
        g.built = None;
        let f = Fail::new("no-world", format!("the world was {why}"));
        for s in g.seats.iter_mut() {
            if let Some(s) = s.take() {
                s.out.kill(Some(f.frame()), "world deleted");
            }
        }
        self.map.lock().unwrap().remove(&entry.code);
        if let Err(e) = self.store.delete(&entry.code) {
            error!(code = %entry.code, error = %e, "world folder not removed");
        }
        self.forget_owned(&entry.code);
        info!(code = %entry.code, why, "world deleted");
    }

    /// Who an `I` names: the player number, the key to send back, and
    /// whether this is the first join (a claim).
    fn resolve(&self, entry: &Entry, g: &Inner, who: &str) -> Result<(u8, Option<String>), Fail> {
        if g.deleted {
            return Err(Fail::new(
                "no-world",
                format!("no world has code {}", entry.code),
            ));
        }
        if let Some(m) = &g.mismatch {
            return Err(Fail::new("gen-mismatch", m.clone()));
        }
        if g.state.closed {
            return Err(Fail::new("closed", "the owner closed the world"));
        }
        if !g.state.started {
            return Err(Fail::new(
                "not-started",
                "the owner has not started the world",
            ));
        }
        if let Some(i) = g.state.players.iter().position(|p| p.name == who) {
            if g.state.players[i].key.is_some() {
                return Err(Fail::new(
                    "name-taken",
                    "that player is claimed; join with the player key",
                ));
            }
            return Ok((i as u8 + 1, None));
        }
        if is_key(who) {
            let h = key_hash(who);
            if let Some(i) = g
                .state
                .players
                .iter()
                .position(|p| p.key.as_deref() == Some(h.as_str()))
            {
                return Ok((i as u8 + 1, Some(who.to_string())));
            }
            return Err(Fail::new("bad-key", "no player of this world has that key"));
        }
        Err(Fail::new(
            "bad-key",
            "no player of this world has that name",
        ))
    }

    /// Make the world again if it is not in memory, and check its hash.
    async fn ensure_built(&self, entry: &Arc<Entry>) -> Result<(), Fail> {
        let ready = |g: &Inner| -> Option<Result<(), Fail>> {
            if let Some(m) = &g.mismatch {
                return Some(Err(Fail::new("gen-mismatch", m.clone())));
            }
            g.built.as_ref().map(|_| Ok(()))
        };
        if let Some(r) = ready(&entry.inner.lock().unwrap()) {
            return r;
        }
        let _guard = entry.load.lock().await;
        if let Some(r) = ready(&entry.inner.lock().unwrap()) {
            return r;
        }
        let m = &entry.meta;
        let have = (gen::version(), gen::data_hash());
        let mismatch = |why: String| {
            error!(
                code = %entry.code,
                stored_version = %m.generator.version,
                stored_data_hash = %m.generator.data_hash,
                stored_hash = %m.world_hash,
                linked_version = have.0,
                linked_data_hash = %have.1,
                why = why.as_str(),
                "world not served: gen-mismatch"
            );
            let text = format!(
                "the world was made by {} {} and cannot be made again here",
                m.generator.krate, m.generator.version
            );
            entry.inner.lock().unwrap().mismatch = Some(text.clone());
            Fail::new("gen-mismatch", text)
        };
        if m.generator.version != have.0 || m.generator.data_hash != have.1 {
            return Err(mismatch("another generator version".into()));
        }
        let cfg: GenConfig = serde_json::from_value(m.config.clone())
            .map_err(|e| mismatch(format!("the stored configuration does not read: {e}")))?;
        let started = Instant::now();
        let ticket = self
            .pool
            .submit(cfg, true, None)
            .map_err(|_| Fail::new("gen-busy", "the generation queue is full"))?;
        let (world, hash) = match ticket.wait(self.pool.time_limit * 3).await {
            Ok(x) => x,
            Err(e) => return Err(mismatch(format!("the generator failed: {e}"))),
        };
        if hash != m.world_hash {
            return Err(mismatch(format!("made again with hash {hash}")));
        }
        info!(code = %entry.code, gen_ms = started.elapsed().as_millis() as u64, "world made again");
        entry.inner.lock().unwrap().built = Some(Built::new(world));
        Ok(())
    }

    /// `I`: join a world. Sends the push (S, Y, W, P, U) and returns the
    /// seat. `next_id` numbers the relay's big payloads on this connection.
    pub async fn join(
        self: &Arc<Self>,
        conn: u64,
        ip: IpAddr,
        out: &Outbox,
        code: &str,
        who: &str,
        next_id: &mut u32,
    ) -> Result<Joined, Fail> {
        let entry = self.get(code)?;
        {
            let g = entry.inner.lock().unwrap();
            self.resolve(&entry, &g, who)?;
        }
        self.ensure_built(&entry).await?;
        let me = self.clone();
        let e2 = entry.clone();
        let out2 = out.clone();
        let who2 = who.to_string();
        let mut nid = *next_id;
        let r = tokio::task::spawn_blocking(move || {
            let r = me.seat(&e2, conn, ip, &out2, &who2, &mut nid);
            (r, nid)
        })
        .await
        .map_err(|e| Fail::new("unavailable", e.to_string()))?;
        *next_id = r.1;
        let player = r.0?;
        Ok(Joined { entry, player })
    }

    fn seat(
        &self,
        entry: &Arc<Entry>,
        conn: u64,
        ip: IpAddr,
        out: &Outbox,
        who: &str,
        next_id: &mut u32,
    ) -> Result<u8, Fail> {
        let cap = self.cfg.queue_cap;
        let mut g = entry.inner.lock().unwrap();
        let (player, key) = self.resolve(entry, &g, who)?;
        let idx = usize::from(player - 1);
        let t = now_s();
        let (key, line, first) = match key {
            Some(k) => (k, Line::Join { p: player, t }, false),
            None => {
                let k = new_key();
                let l = Line::Claim {
                    p: player,
                    key: key_hash(&k),
                    t,
                };
                (k, l, true)
            }
        };
        self.store
            .append(&entry.code, std::slice::from_ref(&line))
            .map_err(|e| store_err(&entry.code, e))?;
        g.state.apply(&line);
        // a live connection of the same player is replaced, without X
        let mut replaced = false;
        if let Some(old) = g.seats[idx].take() {
            old.out.kill(None, "replaced by a rejoin");
            replaced = true;
        }
        let built = g
            .built
            .as_ref()
            .ok_or_else(|| Fail::new("unavailable", "the world is not in memory"))?;
        let w = &built.world;
        let pdata = w
            .players
            .get(idx)
            .ok_or_else(|| Fail::new("unavailable", "the world has no such player"))?;
        let players: Vec<Value> = w
            .players
            .iter()
            .map(|p| json!({"player": p.slot, "name": p.name, "version": p.version}))
            .collect();
        let body = json!({
            "world": entry.code,
            "player": player,
            "players": players,
            "generator": {"crate": entry.meta.generator.krate, "version": w.generator, "data_hash": w.data_hash},
            "seed": w.seed,
            "world_hash": entry.meta.world_hash,
            "data": pdata,
        });
        let body =
            serde_json::to_vec(&body).map_err(|e| Fail::new("unavailable", e.to_string()))?;
        let name = g.state.players[idx].name.clone();
        let mut push = frames::encode_big(
            b'S',
            &format!("{player} {key}"),
            &body,
            self.cfg.max_payload,
            next_id,
        );
        let items: Vec<(u32, u32, u8)> = g.state.players[idx]
            .items
            .iter()
            .enumerate()
            .map(|(i, (item, from))| (i as u32 + 1, *item, *from))
            .collect();
        push.extend_from_slice(&frames::item_frames(&items, crate::codec::MAX_DATA));
        push.extend_from_slice(&encode(b'W', format!("{player} {name}").as_bytes()));
        let p_frame = encode(b'P', format!("{player} {name}").as_bytes());
        for (p, s) in g.connected() {
            push.extend_from_slice(&encode(
                b'P',
                format!("{p} {}", g.state.players[usize::from(p - 1)].name).as_bytes(),
            ));
            if let Some(pos) = &s.pos {
                push.extend_from_slice(&frames::pos_frame(p, pos));
            }
            if !replaced {
                send(&s.out, cap, p_frame.clone());
            }
        }
        send(out, cap, push);
        g.seats[idx] = Some(Seat {
            conn,
            out: out.clone(),
            pos: None,
            tokens: self.cfg.pos_burst,
            last: Instant::now(),
        });
        info!(
            conn,
            world = %entry.code,
            player,
            name = ?name,
            addr = %ip,
            kind = if first { "new" } else { "rejoin" },
            items = items.len(),
            "world join"
        );
        Ok(player)
    }

    /// `V`: record checks and hand out items, or record the goal
    /// (blocking: writes to disk). Checks already recorded are ignored.
    pub fn check(&self, j: &Joined, c: Checks) -> Result<(), Fail> {
        let cap = self.cfg.queue_cap;
        let code = &j.entry.code;
        let mut g = j.entry.inner.lock().unwrap();
        if g.deleted {
            return Err(Fail::new("no-world", "the world was deleted"));
        }
        let me = usize::from(j.player - 1);
        let t = now_s();
        let locs = match c {
            Checks::Goal => {
                if !g.state.players[me].goal {
                    let l = Line::Goal { p: j.player, t };
                    self.store
                        .append(code, std::slice::from_ref(&l))
                        .map_err(|e| store_err(code, e))?;
                    g.state.apply(&l);
                    info!(world = %code, player = j.player, done = g.state.done_at.is_some(), "world goal");
                }
                return Ok(());
            }
            Checks::Locations(l) => l,
        };
        let Some(built) = g.built.as_ref() else {
            return Err(Fail::new("unavailable", "the world is not in memory"));
        };
        let mut lines = Vec::new();
        let mut seen = HashSet::new();
        let mut next: HashMap<u8, u32> = HashMap::new();
        let mut unknown = 0;
        for loc in locs {
            if g.state.players[me].checks.contains(&loc) || !seen.insert(loc) {
                continue;
            }
            let Some(&(item, to)) = built.place[me].get(&loc) else {
                unknown += 1;
                continue;
            };
            let i = if to == j.player || to == 0 || usize::from(to) > g.state.players.len() {
                0
            } else {
                let n = next
                    .entry(to)
                    .or_insert_with(|| g.state.players[usize::from(to - 1)].items.len() as u32);
                *n += 1;
                *n
            };
            lines.push(Line::Check {
                p: j.player,
                loc,
                item,
                to,
                i,
                t,
            });
        }
        if unknown > 0 {
            debug!(world = %code, player = j.player, unknown, "V locations not in the player's world ignored");
        }
        if lines.is_empty() {
            return Ok(());
        }
        self.store
            .append(code, &lines)
            .map_err(|e| store_err(code, e))?;
        let mut out: HashMap<u8, Vec<(u32, u32, u8)>> = HashMap::new();
        for l in &lines {
            g.state.apply(l);
            if let Line::Check { p, item, to, i, .. } = l {
                if *i > 0 {
                    out.entry(*to).or_default().push((*i, *item, *p));
                }
            }
        }
        for (to, items) in out {
            if let Some(s) = &g.seats[usize::from(to - 1)] {
                send(
                    &s.out,
                    cap,
                    frames::item_frames(&items, crate::codec::MAX_DATA),
                );
            }
        }
        Ok(())
    }

    /// `U`: a zone change goes to every other player of the world; a tile
    /// position in the same zone only to players in that zone, at most
    /// `pos_rate` a second.
    pub fn position(&self, j: &Joined, conn: u64, p: Pos) {
        let cap = self.cfg.queue_cap;
        let mut g = j.entry.inner.lock().unwrap();
        let idx = usize::from(j.player - 1);
        let (rate, burst) = (self.cfg.pos_rate, self.cfg.pos_burst);
        let Some(seat) = g.seats[idx].as_mut().filter(|s| s.conn == conn) else {
            return;
        };
        let zone_change = seat.pos.is_none_or(|old| old.zone != p.zone);
        if !zone_change {
            let now = Instant::now();
            seat.tokens =
                (seat.tokens + now.duration_since(seat.last).as_secs_f64() * rate).min(burst);
            seat.last = now;
            if seat.tokens < 1.0 {
                return;
            }
            seat.tokens -= 1.0;
        }
        seat.pos = Some(p);
        let f = frames::pos_frame(j.player, &p);
        for (q, s) in g.connected() {
            if q == j.player {
                continue;
            }
            if zone_change || s.pos.is_some_and(|o| o.zone == p.zone) {
                send(&s.out, cap, f.clone());
            }
        }
    }

    /// A player's connection ended (or `B`): the others get `X`.
    pub fn leave(&self, j: &Joined, conn: u64, cause: &str) {
        let cap = self.cfg.queue_cap;
        let mut g = j.entry.inner.lock().unwrap();
        let idx = usize::from(j.player - 1);
        if !g.seats[idx].as_ref().is_some_and(|s| s.conn == conn) {
            return;
        }
        g.seats[idx] = None;
        let x = encode(b'X', j.player.to_string().as_bytes());
        for (_, s) in g.connected() {
            send(&s.out, cap, x.clone());
        }
        if g.nobody() {
            g.empty_since = Instant::now();
        }
        info!(conn, world = %j.entry.code, player = j.player, cause, "world leave");
    }

    /// Drop made worlds nobody played for a while; delete expired worlds
    /// (blocking: removes folders).
    pub fn sweep(&self) {
        let unload = Duration::from_millis(self.cfg.world_unload_ms);
        let entries: Vec<Arc<Entry>> = self.map.lock().unwrap().values().cloned().collect();
        let now = now_s();
        for e in entries {
            let mut g = e.inner.lock().unwrap();
            if !g.nobody() || g.deleted {
                continue;
            }
            if g.built.is_some() && g.empty_since.elapsed() >= unload {
                g.built = None;
                debug!(code = %e.code, "world left memory");
            }
            let idle = now.saturating_sub(g.state.last_join);
            if idle >= self.cfg.world_idle_s {
                self.drop_world(&e, &mut g, &format!("expired ({idle} s without a join)"));
            } else if let Some(d) = g.state.done_at {
                if now.saturating_sub(d) >= self.cfg.world_done_s {
                    self.drop_world(
                        &e,
                        &mut g,
                        &format!("expired ({} s after the end)", now.saturating_sub(d)),
                    );
                }
            }
        }
    }

    /// Stop the generation threads.
    pub fn stop(&self) {
        self.pool.stop();
    }
}

/// Open the store a setting names: a folder, `temp`, or none (`off` or
/// empty). A folder that cannot be written falls back to the temporary
/// one, with an error in the log. Never fails the start.
pub fn open_store(setting: &str) -> Option<(Store, bool)> {
    let temp = || {
        let dir = std::env::temp_dir().join("metaxy-worlds");
        match Store::open(&dir) {
            Ok(s) => {
                warn!(
                    path = %dir.display(),
                    "the world store is a temporary folder and is not persistent: worlds are lost when the service restarts or redeploys"
                );
                Some((s, false))
            }
            Err(e) => {
                error!(error = e.as_str(), "no world store: world sessions are off");
                None
            }
        }
    };
    match setting {
        "" | "off" => None,
        "temp" => temp(),
        dir => match Store::open(std::path::Path::new(dir)) {
            Ok(s) => Some((s, true)),
            Err(e) => {
                error!(
                    error = e.as_str(),
                    "the world store folder cannot be used; falling back to a temporary folder"
                );
                temp()
            }
        },
    }
}
