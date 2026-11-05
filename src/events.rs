//! Mystery Gift events: the list in `data/mystery_events.json` and the
//! Wonder Card files beside it, and which events are up on a given day.
//!
//! Each event ran one or more times (runs). Every month, a run is up for
//! seven days from the day of the month it first started: a run that
//! started on 6 March is up from the 6th to the 12th of every month, and
//! one that started on the 28th carries into the first days of the next
//! month. A run that started on the 31st starts on the last day of a
//! shorter month.
//!
//! The relay answers a `G` frame (docs/SPEC.md, section 4b) with the events
//! up on the client's date for its language and game, each with its card.

use serde::Deserialize;
use std::path::Path;

/// One card file of an event, as the list names it.
#[derive(Debug, Clone, Deserialize)]
pub struct CardRef {
    pub file: String,
    pub sha256: String,
    pub games: Vec<String>,
    #[serde(default)]
    pub local_wireless_copy: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventRef {
    pub language: String,
    pub method: String,
    pub title: String,
    pub gifts: Vec<String>,
    pub games: Vec<String>,
    pub runs: Vec<Run>,
    pub cards: Vec<CardRef>,
}

#[derive(Debug, Deserialize)]
struct Doc {
    format: u32,
    events: Vec<EventRef>,
}

/// An event with its runs' start days and its cards loaded.
#[derive(Debug, Clone)]
pub struct Event {
    pub info: EventRef,
    /// (month, day) of each run's first day.
    pub starts: Vec<(u32, u32)>,
    /// The card bytes, in the list's order.
    pub cards: Vec<Vec<u8>>,
}

/// An event that is up, with the card for the asked game.
#[derive(Debug, Clone)]
pub struct Live<'a> {
    pub event: &'a Event,
    pub card: &'a [u8],
}

#[derive(Debug, Default)]
pub struct Events {
    pub list: Vec<Event>,
}

/// A date: year, month (1-12), day (1-31).
pub type Date = (i32, u32, u32);

pub fn parse_date(s: &str) -> Option<Date> {
    let mut it = s.split('-');
    let y: i32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || d < 1 || d > month_len(y, m) {
        return None;
    }
    Some((y, m, d))
}

fn leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn month_len(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if leap(y) {
                29
            } else {
                28
            }
        }
    }
}

/// The day before `d`.
fn prev(d: Date) -> Date {
    let (y, m, day) = d;
    if day > 1 {
        (y, m, day - 1)
    } else if m > 1 {
        (y, m - 1, month_len(y, m - 1))
    } else {
        (y - 1, 12, 31)
    }
}

/// Whether a run that first started on day `start` of a month is up on
/// `today`: one of the last seven days (today included) is that day of
/// its month, or the last day of a month shorter than `start`.
pub fn run_is_up(start_day: u32, today: Date) -> bool {
    let mut d = today;
    for _ in 0..7 {
        let (y, m, day) = d;
        if day == start_day.min(month_len(y, m)) {
            return true;
        }
        d = prev(d);
    }
    false
}

impl Events {
    /// Load the list and its cards. A card whose bytes do not match its
    /// SHA-256 is an error.
    pub fn load(path: &Path) -> Result<Events, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let doc: Doc = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if doc.format != 1 {
            return Err(format!("{}: format {} (this relay reads 1)", path.display(), doc.format));
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut list = Vec::with_capacity(doc.events.len());
        for info in doc.events {
            let mut starts = Vec::new();
            for r in &info.runs {
                let (_, m, d) = parse_date(&r.start)
                    .ok_or_else(|| format!("{}: bad date {:?} in {:?}", path.display(), r.start, info.title))?;
                starts.push((m, d));
            }
            let mut cards = Vec::new();
            for c in &info.cards {
                let p = dir.join(&c.file);
                let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                if sha256_hex(&bytes) != c.sha256 {
                    return Err(format!("{}: SHA-256 does not match the list", p.display()));
                }
                cards.push(bytes);
            }
            list.push(Event { info, starts, cards });
        }
        Ok(Events { list })
    }

    /// The events up on `today` for a language ("en", "fr", ...) and a game
    /// ("black" or "white"), each with the card for that game. An event
    /// with no card for the game is left out.
    pub fn live(&self, language: &str, game: &str, today: Date) -> Vec<Live<'_>> {
        let mut out = Vec::new();
        for e in &self.list {
            if e.info.language != language || !e.info.games.iter().any(|g| g == game) {
                continue;
            }
            if !e.starts.iter().any(|&(_, d)| run_is_up(d, today)) {
                continue;
            }
            // the card for the game; for a wireless event its local
            // wireless copy when the archive has one
            let wireless = e.info.method == "wireless";
            let pick = e
                .info
                .cards
                .iter()
                .enumerate()
                .filter(|(_, c)| c.games.iter().any(|g| g == game))
                .min_by_key(|(_, c)| c.local_wireless_copy != wireless);
            if let Some((i, _)) = pick {
                out.push(Live { event: e, card: &e.cards[i] });
            }
        }
        out
    }
}

/// SHA-256 as lowercase hex (FIPS 180-4), for checking the card files.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// The `M` frames' payloads for a reply: the method, the title and the
/// gifts on three lines, then the card's bytes.
pub fn reply_payloads(live: &[Live<'_>]) -> Vec<Vec<u8>> {
    live.iter()
        .map(|l| {
            let mut p = Vec::with_capacity(l.card.len() + 128);
            p.extend_from_slice(l.event.info.method.as_bytes());
            p.push(b'\n');
            p.extend_from_slice(l.event.info.title.as_bytes());
            p.push(b'\n');
            p.extend_from_slice(l.event.info.gifts.join(", ").as_bytes());
            p.push(b'\n');
            p.extend_from_slice(l.card);
            p
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_values() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn a_run_is_up_for_a_week_from_its_day() {
        // started on the 6th: the 6th to the 12th of every month
        assert!(!run_is_up(6, (2026, 11, 5)));
        assert!(run_is_up(6, (2026, 11, 6)));
        assert!(run_is_up(6, (2026, 11, 12)));
        assert!(!run_is_up(6, (2026, 11, 13)));
        // started on the 28th: carries into the next month
        assert!(run_is_up(28, (2026, 12, 3)));
        assert!(!run_is_up(28, (2026, 12, 5)));
        // started on the 31st: the last day of a shorter month
        assert!(run_is_up(31, (2026, 2, 28)));
        assert!(run_is_up(31, (2026, 3, 6)));
        assert!(!run_is_up(31, (2026, 3, 7)));
    }

    #[test]
    fn dates_parse_and_reject() {
        assert_eq!(parse_date("2011-03-06"), Some((2011, 3, 6)));
        assert_eq!(parse_date("2011-02-29"), None);
        assert_eq!(parse_date("2012-02-29"), Some((2012, 2, 29)));
        assert_eq!(parse_date("x"), None);
    }

    #[test]
    fn the_shipped_list_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/mystery_events.json");
        let ev = Events::load(&path).expect("the list and its cards load");
        assert!(ev.list.len() > 100);
        // the Liberty Pass first started on 6 March: up from the 6th to the 12th
        let up = ev.live("en", "black", (2026, 11, 8));
        assert!(up.iter().any(|l| l.event.info.title == "Liberty Pass"));
        assert!(up.iter().all(|l| l.card.len() == 204));
    }
}
