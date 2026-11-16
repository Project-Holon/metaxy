//! The world-session frames (`eidos-net/2`): payload parsing, `F` pieces
//! and the big-payload encoding.

use crate::codec::{encode, show, Fail};
use std::collections::HashMap;

/// The protocol version of the world-session frames.
pub const PROTO2: &str = "eidos-net/2";
/// The most bytes of a big payload in one `F` frame.
pub const PIECE: usize = 4096;

fn bad(d: impl Into<String>) -> Fail {
    Fail::new("bad-frame", d)
}

fn utf8<'a>(b: &'a [u8], what: &str) -> Result<&'a str, Fail> {
    std::str::from_utf8(b).map_err(|_| bad(format!("{what} is not UTF-8")))
}

fn check_version(v: &str) -> Result<(), Fail> {
    if v != PROTO2 {
        return Err(Fail::new(
            "version",
            format!(
                "got \"{}\", world sessions speak {PROTO2}",
                show(v.as_bytes(), 32)
            ),
        ));
    }
    Ok(())
}

fn check_token(t: &str, k: char) -> Result<(), Fail> {
    let ok =
        (16..=64).contains(&t.len()) && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !ok {
        return Err(bad(format!(
            "{k} token must be 16 to 64 characters of 0-9 and a-f (got {} bytes)",
            t.len()
        )));
    }
    Ok(())
}

/// A body carried in the frame or in `F` pieces sent before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Inline(Vec<u8>),
    /// `@<message id>`
    Pieces(u32),
}

fn body(b: &[u8]) -> Result<Body, Fail> {
    if let Some(id) = b.strip_prefix(b"@") {
        return Ok(Body::Pieces(number(id, "message id")?));
    }
    Ok(Body::Inline(b.to_vec()))
}

/// A decimal number of up to 9 digits.
fn number(b: &[u8], what: &str) -> Result<u32, Fail> {
    if b.is_empty() || b.len() > 9 || !b.iter().all(u8::is_ascii_digit) {
        return Err(bad(format!("{what} \"{}\" is not a number", show(b, 16))));
    }
    Ok(b.iter().fold(0u32, |a, &d| a * 10 + u32::from(d - b'0')))
}

/// `N <version> <token>\n<configuration JSON or @id>`
pub fn parse_new(p: &[u8]) -> Result<(String, Body), Fail> {
    let nl = p
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| bad("N has no newline before the configuration"))?;
    let head = utf8(&p[..nl], "N")?;
    let mut f = head.split(' ');
    check_version(f.next().unwrap_or(""))?;
    let token = f.next().ok_or_else(|| bad("N has no token"))?;
    check_token(token, 'N')?;
    if f.next().is_some() {
        return Err(bad("N wants <version> <token> on its first line"));
    }
    Ok((token.to_string(), body(&p[nl + 1..])?))
}

/// An owner action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Close,
    Delete,
}

impl Action {
    /// The word the relay answers with.
    pub fn done(self) -> &'static str {
        match self {
            Action::Start => "started",
            Action::Close => "closed",
            Action::Delete => "deleted",
        }
    }
}

/// `O <version> <code> <owner key> <action>`
pub fn parse_owner(p: &[u8]) -> Result<(String, String, Action), Fail> {
    let t = utf8(p, "O")?;
    let f: Vec<&str> = t.split(' ').collect();
    if f.len() != 4 {
        return Err(bad("O wants <version> <code> <owner key> <action>"));
    }
    check_version(f[0])?;
    let a = match f[3] {
        "start" => Action::Start,
        "close" => Action::Close,
        "delete" => Action::Delete,
        x => {
            return Err(bad(format!(
                "O action \"{}\" is not start, close or delete",
                show(x.as_bytes(), 16)
            )))
        }
    };
    Ok((f[1].to_ascii_uppercase(), f[2].to_string(), a))
}

/// `I <version> <token> <code> <name or player key>`
pub fn parse_join(p: &[u8]) -> Result<(String, String, String), Fail> {
    let t = utf8(p, "I")?;
    let mut f = t.splitn(4, ' ');
    check_version(f.next().unwrap_or(""))?;
    let token = f.next().ok_or_else(|| bad("I has no token"))?;
    check_token(token, 'I')?;
    let code = f.next().ok_or_else(|| bad("I has no code"))?;
    let who = f.next().ok_or_else(|| bad("I has no name or player key"))?;
    if who.is_empty() {
        return Err(bad("I name is empty"));
    }
    Ok((
        token.to_string(),
        code.to_ascii_uppercase(),
        who.to_string(),
    ))
}

/// What a `V` frame says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checks {
    Locations(Vec<u32>),
    Goal,
}

/// `V <location id> ...` or `V goal`
pub fn parse_checks(p: &[u8]) -> Result<Checks, Fail> {
    if p == b"goal" {
        return Ok(Checks::Goal);
    }
    let mut v = Vec::new();
    for w in p.split(|&b| b == b' ') {
        v.push(number(w, "V location")?);
    }
    Ok(Checks::Locations(v))
}

/// A position: zone, then x, y, z and the facing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub zone: u32,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub dir: u32,
}

fn signed(w: &str) -> Option<i32> {
    let d = w.strip_prefix('-').unwrap_or(w);
    if d.is_empty() || d.len() > 9 || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    w.parse().ok()
}

/// `U <zone> <x> <y> <z> <dir>`
pub fn parse_pos(p: &[u8]) -> Result<Pos, Fail> {
    let t = utf8(p, "U")?;
    let f: Vec<&str> = t.split(' ').collect();
    let err = || bad("U wants <zone> <x> <y> <z> <dir> as whole numbers");
    if f.len() != 5 {
        return Err(err());
    }
    let zone = number(f[0].as_bytes(), "U zone")?;
    let dir = number(f[4].as_bytes(), "U dir")?;
    let x = signed(f[1]).ok_or_else(err)?;
    let y = signed(f[2]).ok_or_else(err)?;
    let z = signed(f[3]).ok_or_else(err)?;
    Ok(Pos { zone, x, y, z, dir })
}

/// The relay's `U <player> <zone> <x> <y> <z> <dir>`.
pub fn pos_frame(player: u8, p: &Pos) -> Vec<u8> {
    encode(
        b'U',
        format!("{player} {} {} {} {} {}", p.zone, p.x, p.y, p.z, p.dir).as_bytes(),
    )
}

/// `F <message id> <last 0/1>\n<bytes>`
pub fn parse_piece(p: &[u8]) -> Result<(u32, bool, &[u8]), Fail> {
    let nl = p
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| bad("F has no newline after its header"))?;
    let mut f = p[..nl].split(|&b| b == b' ');
    let id = number(f.next().unwrap_or(&[]), "F message id")?;
    let last = match f.next() {
        Some(b"0") => false,
        Some(b"1") => true,
        _ => return Err(bad("F wants <message id> <last 0/1>")),
    };
    if f.next().is_some() {
        return Err(bad("F wants <message id> <last 0/1>"));
    }
    let bytes = &p[nl + 1..];
    if bytes.len() > PIECE {
        return Err(Fail::new(
            "too-big",
            format!("F piece of {} bytes (limit {PIECE})", bytes.len()),
        ));
    }
    Ok((id, last, bytes))
}

/// The most big payloads a client may have open or unused at once.
pub const MAX_MESSAGES: usize = 8;

/// The big payloads a client is sending: pieces by message id.
#[derive(Debug, Default)]
pub struct Assembly {
    open: HashMap<u32, Vec<u8>>,
    done: HashMap<u32, Vec<u8>>,
    total: usize,
}

impl Assembly {
    /// Add one `F` piece. `cap` is the most bytes held at once.
    pub fn add(&mut self, payload: &[u8], cap: usize) -> Result<(), Fail> {
        let (id, last, bytes) = parse_piece(payload)?;
        if self.done.contains_key(&id) {
            return Err(bad(format!("F message {id} is already whole")));
        }
        if !self.open.contains_key(&id) && self.open.len() + self.done.len() >= MAX_MESSAGES {
            return Err(Fail::new(
                "too-big",
                format!("more than {MAX_MESSAGES} big payloads at once"),
            ));
        }
        if self.total + bytes.len() > cap {
            return Err(Fail::new(
                "too-big",
                format!(
                    "big payloads of {} bytes (limit {cap})",
                    self.total + bytes.len()
                ),
            ));
        }
        self.total += bytes.len();
        let buf = self.open.entry(id).or_default();
        buf.extend_from_slice(bytes);
        if last {
            let whole = self.open.remove(&id).unwrap_or_default();
            self.done.insert(id, whole);
        }
        Ok(())
    }

    /// Take a whole message for the frame that uses it.
    pub fn take(&mut self, id: u32) -> Result<Vec<u8>, Fail> {
        match self.done.remove(&id) {
            Some(b) => {
                self.total -= b.len();
                Ok(b)
            }
            None => Err(bad(format!("no whole F message {id}"))),
        }
    }

    /// The bytes of a body.
    pub fn resolve(&mut self, b: Body) -> Result<Vec<u8>, Fail> {
        match b {
            Body::Inline(v) => Ok(v),
            Body::Pieces(id) => self.take(id),
        }
    }
}

/// A relay frame `<kind> <head>\n<body>`; when it does not fit in
/// `max_payload`, the body goes first as `F` pieces and the frame carries
/// `@<id>`. `next_id` numbers the relay's messages on this connection.
pub fn encode_big(
    kind: u8,
    head: &str,
    body: &[u8],
    max_payload: usize,
    next_id: &mut u32,
) -> Vec<u8> {
    if head.len() + 1 + body.len() <= max_payload {
        let mut p = Vec::with_capacity(head.len() + 1 + body.len());
        p.extend_from_slice(head.as_bytes());
        p.push(b'\n');
        p.extend_from_slice(body);
        return encode(kind, &p);
    }
    *next_id += 1;
    let id = *next_id;
    let size = PIECE.min(max_payload.saturating_sub(24)).max(1);
    let mut out = Vec::with_capacity(body.len() + body.len() / size * 16 + 64);
    let n = body.len().div_ceil(size).max(1);
    for (i, chunk) in body.chunks(size).enumerate() {
        let last = u8::from(i + 1 == n);
        let mut p = format!("{id} {last}\n").into_bytes();
        p.extend_from_slice(chunk);
        out.extend_from_slice(&encode(b'F', &p));
    }
    out.extend_from_slice(&encode(kind, format!("{head}\n@{id}").as_bytes()));
    out
}

/// `Y <index> <item id> <from player> ...` frames, each at most `max`
/// bytes of payload.
pub fn item_frames(items: &[(u32, u32, u8)], max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for (idx, item, from) in items {
        let t = format!("{idx} {item} {from}");
        if !cur.is_empty() && cur.len() + 1 + t.len() > max {
            out.extend_from_slice(&encode(b'Y', cur.as_bytes()));
            cur.clear();
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&t);
    }
    if !cur.is_empty() {
        out.extend_from_slice(&encode(b'Y', cur.as_bytes()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOK: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn new_frames() {
        let (t, b) = parse_new(format!("eidos-net/2 {TOK}\n{{}}").as_bytes()).unwrap();
        assert_eq!((t.as_str(), b), (TOK, Body::Inline(b"{}".to_vec())));
        let (_, b) = parse_new(format!("eidos-net/2 {TOK}\n@7").as_bytes()).unwrap();
        assert_eq!(b, Body::Pieces(7));
        let e = parse_new(format!("eidos-net/1 {TOK}\n{{}}").as_bytes()).unwrap_err();
        assert_eq!(e.reason, "version");
        assert_eq!(
            parse_new(b"eidos-net/2 abc\n{}").unwrap_err().reason,
            "bad-frame"
        );
        assert_eq!(
            parse_new(format!("eidos-net/2 {TOK}").as_bytes())
                .unwrap_err()
                .reason,
            "bad-frame"
        );
        assert_eq!(
            parse_new(format!("eidos-net/2 {TOK}\n@x").as_bytes())
                .unwrap_err()
                .reason,
            "bad-frame"
        );
    }

    #[test]
    fn owner_join_check_pos() {
        assert_eq!(
            parse_owner(b"eidos-net/2 abc234 k start").unwrap(),
            ("ABC234".into(), "k".into(), Action::Start)
        );
        assert_eq!(
            parse_owner(b"eidos-net/2 A k stop").unwrap_err().reason,
            "bad-frame"
        );
        assert_eq!(
            parse_owner(b"eidos-net/2 A k").unwrap_err().reason,
            "bad-frame"
        );
        assert_eq!(
            parse_owner(b"eidos-net/9 A k start").unwrap_err().reason,
            "version"
        );
        let (t, c, w) =
            parse_join(format!("eidos-net/2 {TOK} abc234 Ash Ketchum").as_bytes()).unwrap();
        assert_eq!(
            (t.as_str(), c.as_str(), w.as_str()),
            (TOK, "ABC234", "Ash Ketchum")
        );
        assert_eq!(
            parse_join(format!("eidos-net/2 {TOK} abc234").as_bytes())
                .unwrap_err()
                .reason,
            "bad-frame"
        );
        assert_eq!(parse_checks(b"goal").unwrap(), Checks::Goal);
        assert_eq!(
            parse_checks(b"1 22 333").unwrap(),
            Checks::Locations(vec![1, 22, 333])
        );
        assert!(parse_checks(b"1  2").is_err());
        assert!(parse_checks(b"").is_err());
        assert!(parse_checks(b"1 -2").is_err());
        let p = parse_pos(b"12 -3 4 0 2").unwrap();
        assert_eq!(
            p,
            Pos {
                zone: 12,
                x: -3,
                y: 4,
                z: 0,
                dir: 2
            }
        );
        assert_eq!(pos_frame(3, &p), b"U 13\n3 12 -3 4 0 2");
        assert!(parse_pos(b"12 -3 4 0").is_err());
        assert!(parse_pos(b"12 - 4 0 1").is_err());
        assert!(parse_pos(b"-1 3 4 0 1").is_err());
    }

    #[test]
    fn pieces() {
        let mut a = Assembly::default();
        a.add(b"5 0\nhello ", 100).unwrap();
        a.add(b"5 1\nworld", 100).unwrap();
        assert!(a.take(6).is_err());
        assert_eq!(a.resolve(Body::Pieces(5)).unwrap(), b"hello world");
        assert!(a.take(5).is_err());
        assert_eq!(
            a.add(&[b"1 0\n".as_slice(), &[b'x'; 101]].concat(), 100)
                .unwrap_err()
                .reason,
            "too-big"
        );
        assert_eq!(a.add(b"1 2\nx", 100).unwrap_err().reason, "bad-frame");
        assert_eq!(a.add(b"1 0 x", 100).unwrap_err().reason, "bad-frame");
        a.add(b"2 1\n", 100).unwrap();
        assert_eq!(a.take(2).unwrap(), b"");
    }

    #[test]
    fn pieces_are_limited_in_count() {
        let mut a = Assembly::default();
        for id in 0..MAX_MESSAGES {
            a.add(format!("{id} {}\n", id % 2).as_bytes(), 100).unwrap();
        }
        a.add(b"0 0\nx", 100).unwrap();
        let e = a.add(b"99 0\n", 100).unwrap_err();
        assert_eq!(e.reason, "too-big");
        a.take(1).unwrap();
        a.add(b"99 0\n", 100).unwrap();
    }

    #[test]
    fn big_round_trip() {
        let mut id = 0;
        assert_eq!(
            encode_big(b'S', "1 k", b"{}", 4224, &mut id),
            b"S 6\n1 k\n{}"
        );
        assert_eq!(id, 0);
        let body: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let out = encode_big(b'S', "1 k", &body, 4224, &mut id);
        assert_eq!(id, 1);
        // read it back as a client would
        let mut a = Assembly::default();
        let mut rest = &out[..];
        let mut last = None;
        while !rest.is_empty() {
            let nl = rest.iter().position(|&b| b == b'\n').unwrap();
            let h = std::str::from_utf8(&rest[..nl]).unwrap();
            let len: usize = h[2..].parse().unwrap();
            let p = &rest[nl + 1..nl + 1 + len];
            if rest[0] == b'F' {
                a.add(p, 1 << 20).unwrap();
            } else {
                last = Some((rest[0], p.to_vec()));
            }
            rest = &rest[nl + 1 + len..];
        }
        let (k, p) = last.unwrap();
        assert_eq!((k, p.as_slice()), (b'S', b"1 k\n@1".as_slice()));
        assert_eq!(a.take(1).unwrap(), body);
    }

    #[test]
    fn items_split_in_frames() {
        let items: Vec<(u32, u32, u8)> = (1..=1000).map(|i| (i, 500 + i, 2)).collect();
        let out = item_frames(&items, 100);
        let text = String::from_utf8(out).unwrap();
        let mut seen = 0;
        for f in text.split("Y ").skip(1) {
            let (len, body) = f.split_once('\n').unwrap();
            let body = &body[..len.parse::<usize>().unwrap()];
            assert!(body.len() <= 100);
            seen += body.split(' ').count() / 3;
        }
        assert_eq!(seen, 1000);
    }
}
