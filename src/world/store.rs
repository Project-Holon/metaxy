//! The world store: a folder per world with `world.json` (written once)
//! and `log` (append-only, one JSON line per change, each written and
//! synced to disk before metaxy answers).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing::warn;

/// The format name in `world.json`.
pub const FORMAT: &str = "metaxy-world/1";

/// Seconds since 1970.
pub fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SHA-256 of a key, in hex. Only these hashes are stored.
pub fn key_hash(key: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The generator that made a world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenId {
    #[serde(rename = "crate")]
    pub krate: String,
    pub version: String,
    pub data_hash: String,
}

/// `world.json`: what makes the world, not the world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub format: String,
    pub code: String,
    /// Seconds since 1970.
    pub created: u64,
    pub generator: GenId,
    pub seed: u64,
    /// The hash the generator gave at creation.
    pub world_hash: String,
    /// SHA-256 of the owner key.
    pub owner_key: String,
    /// The configuration as checked on `N` (with its seed).
    pub config: Value,
    /// The players' names, in player order (player 1 first).
    pub players: Vec<String>,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "lowercase")]
pub enum Line {
    /// The owner made the world joinable.
    Start { t: u64 },
    /// The owner closed it: no more joins.
    Close { t: u64 },
    /// A player's first join: the hash of their new key.
    Claim { p: u8, key: String, t: u64 },
    /// A later join.
    Join { p: u8, t: u64 },
    /// Player `p` checked location `loc`; the item goes to player `to`
    /// with index `i` in their list (0 for the player's own item).
    Check {
        p: u8,
        loc: u32,
        item: u32,
        to: u8,
        i: u32,
        t: u64,
    },
    /// Player `p` reached the goal.
    Goal { p: u8, t: u64 },
}

/// One player's state.
#[derive(Debug, Clone, Default)]
pub struct PlayerState {
    pub name: String,
    /// SHA-256 of the player key, once claimed.
    pub key: Option<String>,
    pub checks: HashSet<u32>,
    /// Items for this player, index 1 first: (item, from player).
    pub items: Vec<(u32, u8)>,
    pub goal: bool,
    pub last_seen: u64,
}

/// A world's state, rebuilt from its log.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub started: bool,
    pub closed: bool,
    pub players: Vec<PlayerState>,
    /// The last join, or the creation time.
    pub last_join: u64,
    /// When the last player reached the goal.
    pub done_at: Option<u64>,
}

impl State {
    pub fn new(meta: &Meta) -> State {
        State {
            players: meta
                .players
                .iter()
                .map(|n| PlayerState {
                    name: n.clone(),
                    ..PlayerState::default()
                })
                .collect(),
            last_join: meta.created,
            ..State::default()
        }
    }

    fn player(&mut self, p: u8) -> Option<&mut PlayerState> {
        self.players.get_mut(usize::from(p).checked_sub(1)?)
    }

    /// Apply one line. Lines naming a player the world does not have are
    /// skipped.
    pub fn apply(&mut self, l: &Line) {
        match l {
            Line::Start { .. } => {
                self.started = true;
                self.closed = false;
            }
            Line::Close { .. } => self.closed = true,
            Line::Claim { p, key, t } => {
                self.last_join = self.last_join.max(*t);
                if let Some(s) = self.player(*p) {
                    s.key = Some(key.clone());
                    s.last_seen = *t;
                }
            }
            Line::Join { p, t } => {
                self.last_join = self.last_join.max(*t);
                if let Some(s) = self.player(*p) {
                    s.last_seen = s.last_seen.max(*t);
                }
            }
            Line::Check {
                p,
                loc,
                item,
                to,
                i,
                ..
            } => {
                if let Some(s) = self.player(*p) {
                    s.checks.insert(*loc);
                }
                if *i > 0 && to != p {
                    if let Some(r) = self.player(*to) {
                        r.items.push((*item, *p));
                    }
                }
            }
            Line::Goal { p, t } => {
                if let Some(s) = self.player(*p) {
                    s.goal = true;
                }
                if self.done_at.is_none() && self.players.iter().all(|s| s.goal) {
                    self.done_at = Some(*t);
                }
            }
        }
    }
}

/// The folder that holds every world.
#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
}

/// A world read back from the store.
pub struct Loaded {
    pub meta: Meta,
    pub state: State,
    /// Lines skipped: a cut last line, or lines that do not read.
    pub skipped: usize,
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

impl Store {
    /// Use `root`, made if missing. Fails when it cannot be written.
    pub fn open(root: &Path) -> Result<Store, String> {
        fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
        let probe = root.join(".metaxy-probe");
        write_new(&probe, b"ok")
            .map_err(|e| format!("{} cannot be written: {e}", root.display()))?;
        let _ = fs::remove_file(&probe);
        Ok(Store {
            root: root.to_path_buf(),
        })
    }

    fn dir(&self, code: &str) -> PathBuf {
        self.root.join(code)
    }

    /// True when a folder for this code exists.
    pub fn exists(&self, code: &str) -> bool {
        self.dir(code).exists()
    }

    /// Write a new world's folder: `world.json` and an empty log.
    pub fn create(&self, meta: &Meta) -> io::Result<()> {
        let d = self.dir(&meta.code);
        fs::create_dir_all(&d)?;
        File::create(d.join("log"))?.sync_all()?;
        let json = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
        write_new(&d.join("world.json"), &json)
    }

    /// Append lines to a world's log and sync them to disk.
    pub fn append(&self, code: &str, lines: &[Line]) -> io::Result<()> {
        let mut buf = Vec::new();
        for l in lines {
            serde_json::to_writer(&mut buf, l).map_err(io::Error::other)?;
            buf.push(b'\n');
        }
        let mut f = OpenOptions::new()
            .append(true)
            .open(self.dir(code).join("log"))?;
        f.write_all(&buf)?;
        f.sync_data()
    }

    /// Remove a world's folder.
    pub fn delete(&self, code: &str) -> io::Result<()> {
        fs::remove_dir_all(self.dir(code))
    }

    /// Read every world. Folders that do not read are logged and left
    /// alone. A cut last line (a crash in the middle of a write) is cut
    /// off the file. Logs with many repeated joins are compacted.
    pub fn load(&self) -> Vec<Loaded> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(&self.root) else {
            return out;
        };
        for e in rd.flatten() {
            let d = e.path();
            if !d.is_dir() {
                continue;
            }
            match self.load_one(&d) {
                Ok(l) => out.push(l),
                Err(err) => {
                    warn!(dir = %d.display(), error = err.as_str(), "world folder not loaded")
                }
            }
        }
        out
    }

    fn load_one(&self, d: &Path) -> Result<Loaded, String> {
        let text = fs::read(d.join("world.json")).map_err(|e| format!("world.json: {e}"))?;
        let meta: Meta = serde_json::from_slice(&text).map_err(|e| format!("world.json: {e}"))?;
        if meta.format != FORMAT {
            return Err(format!("world.json format {:?}", meta.format));
        }
        let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if meta.code != name {
            return Err(format!(
                "world.json names code {} in folder {name}",
                meta.code
            ));
        }
        let log_path = d.join("log");
        let raw = match fs::read(&log_path) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                File::create(&log_path).map_err(|e| format!("log: {e}"))?;
                Vec::new()
            }
            Err(e) => return Err(format!("log: {e}")),
        };
        let mut state = State::new(&meta);
        let mut lines = Vec::new();
        let mut skipped = 0;
        // the part up to the last newline; anything after it was cut
        let whole = raw.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if whole < raw.len() {
            skipped += 1;
            let f = OpenOptions::new()
                .write(true)
                .open(&log_path)
                .map_err(|e| format!("log: {e}"))?;
            f.set_len(whole as u64).map_err(|e| format!("log: {e}"))?;
            f.sync_all().map_err(|e| format!("log: {e}"))?;
        }
        for l in raw[..whole].split(|&b| b == b'\n') {
            if l.is_empty() {
                continue;
            }
            match serde_json::from_slice::<Line>(l) {
                Ok(line) => {
                    state.apply(&line);
                    lines.push(line);
                }
                Err(_) => skipped += 1,
            }
        }
        let joins = lines
            .iter()
            .filter(|l| matches!(l, Line::Join { .. }))
            .count();
        if joins > 4 * meta.players.len() + 64 {
            compact(&log_path, &lines).map_err(|e| format!("log compaction: {e}"))?;
        }
        Ok(Loaded {
            meta,
            state,
            skipped,
        })
    }
}

/// Rewrite a log with one join line per player (the last), every other
/// line as it was.
fn compact(path: &Path, lines: &[Line]) -> io::Result<()> {
    let mut last = std::collections::BTreeMap::new();
    let mut buf = Vec::new();
    for l in lines {
        if let Line::Join { p, t } = l {
            let e = last.entry(*p).or_insert(*t);
            *e = (*e).max(*t);
            continue;
        }
        serde_json::to_writer(&mut buf, l).map_err(io::Error::other)?;
        buf.push(b'\n');
    }
    for (p, t) in last {
        serde_json::to_writer(&mut buf, &Line::Join { p, t }).map_err(io::Error::other)?;
        buf.push(b'\n');
    }
    write_new(path, &buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(code: &str) -> Meta {
        Meta {
            format: FORMAT.into(),
            code: code.into(),
            created: 100,
            generator: GenId {
                krate: "arkh-gen".into(),
                version: "0.1.0".into(),
                data_hash: "00".into(),
            },
            seed: 1,
            world_hash: "ab".into(),
            owner_key: key_hash("owner"),
            config: serde_json::json!({}),
            players: vec!["Ash".into(), "Misty".into()],
        }
    }

    #[test]
    fn write_crash_rebuild_compact() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.create(&meta("AAAAAA")).unwrap();
        s.append(
            "AAAAAA",
            &[
                Line::Start { t: 101 },
                Line::Claim {
                    p: 1,
                    key: key_hash("k1"),
                    t: 102,
                },
                Line::Check {
                    p: 1,
                    loc: 7,
                    item: 50,
                    to: 2,
                    i: 1,
                    t: 103,
                },
                Line::Check {
                    p: 1,
                    loc: 8,
                    item: 51,
                    to: 1,
                    i: 0,
                    t: 103,
                },
            ],
        )
        .unwrap();
        // a crash in the middle of a line
        let log = dir.path().join("AAAAAA").join("log");
        let mut f = OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"{\"e\":\"goal\",\"p\":").unwrap();
        drop(f);
        let l = s.load();
        assert_eq!(l.len(), 1);
        let st = &l[0].state;
        assert_eq!(l[0].skipped, 1);
        assert!(st.started && !st.closed);
        assert_eq!(st.players[0].key.as_deref(), Some(key_hash("k1").as_str()));
        assert_eq!(st.players[0].checks.len(), 2);
        assert_eq!(st.players[1].items, vec![(50, 1)]);
        assert!(st.players[0].items.is_empty());
        assert_eq!(st.last_join, 102);
        // the cut line is gone: a new line reads
        s.append(
            "AAAAAA",
            &[Line::Goal { p: 1, t: 104 }, Line::Goal { p: 2, t: 105 }],
        )
        .unwrap();
        let st = &s.load()[0].state;
        assert_eq!(st.done_at, Some(105));
        // many joins are compacted to the last one per player
        let joins: Vec<Line> = (0..200)
            .map(|i| Line::Join {
                p: 1 + (i % 2) as u8,
                t: 200 + i,
            })
            .collect();
        s.append("AAAAAA", &joins).unwrap();
        let before = s.load();
        assert_eq!(before[0].state.last_join, 399);
        let text = fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().filter(|l| l.contains("\"join\"")).count(), 2);
        assert_eq!(text.lines().count(), 8);
        let after = &s.load()[0].state;
        assert_eq!(after.players[0].last_seen, 398);
        assert_eq!(after.players[1].items, vec![(50, 1)]);
    }

    #[test]
    fn bad_folders_are_skipped_and_unwritable_root_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        fs::create_dir_all(dir.path().join("BBBBBB")).unwrap();
        fs::write(dir.path().join("BBBBBB").join("world.json"), b"{").unwrap();
        let mut m = meta("CCCCCC");
        m.code = "DDDDDD".into();
        fs::create_dir_all(dir.path().join("CCCCCC")).unwrap();
        fs::write(
            dir.path().join("CCCCCC").join("world.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        s.create(&meta("EEEEEE")).unwrap();
        let l = s.load();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].meta.code, "EEEEEE");
        // a file where the folder should be
        let f = dir.path().join("file");
        fs::write(&f, b"x").unwrap();
        assert!(Store::open(&f.join("sub")).is_err());
    }
}
