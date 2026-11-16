//! Where a link points: a room link (look up the room's port) or a host
//! and port. A small HTTP GET for the room status.

use super::ws::{self, Stream};
use std::net::IpAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A WebSocket address to try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Connect to these, in order, until one upgrades.
    Direct(Vec<Target>),
    /// A room page: `tls`, host, port of the site, and the room id.
    Room {
        tls: bool,
        host: String,
        port: u16,
        room: String,
    },
}

fn host_port(s: &str, default: Option<u16>) -> Result<(String, u16), String> {
    let (h, p) = if let Some(r) = s.strip_prefix('[') {
        let (h, rest) = r.split_once(']').ok_or("a bad IPv6 address")?;
        (h, rest.strip_prefix(':'))
    } else {
        match s.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (s, None),
        }
    };
    let port = match p {
        Some(p) => p
            .parse()
            .map_err(|_| format!("port \"{p}\" is not a number"))?,
        None => default.ok_or("the link has no port")?,
    };
    let h = h.to_string();
    if h.is_empty()
        || !h
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
    {
        return Err("the link's host is not a host name".into());
    }
    Ok((h, port))
}

/// Read a link: `ws://host:port/path`, `wss://...`, a room link
/// (`https://archipelago.gg/room/<id>`, with or without the scheme) or
/// `host:port` (tried as wss, then ws).
pub fn plan(link: &str) -> Result<Plan, String> {
    let link = link.trim();
    if link.is_empty() || link.len() > 512 || link.chars().any(char::is_whitespace) {
        return Err("the link must be one word of at most 512 bytes".into());
    }
    let (scheme, rest) = match link.split_once("://") {
        Some((s, r)) => (Some(s.to_ascii_lowercase()), r),
        None => (None, link),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if let Some(room) = path.strip_prefix("/room/") {
        let room = room.trim_end_matches('/');
        if room.is_empty()
            || !room
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err("the room id is not valid".into());
        }
        let tls = match scheme.as_deref() {
            None | Some("https") => true,
            Some("http") => false,
            Some(s) => return Err(format!("a room link cannot use {s}://")),
        };
        let (host, port) = host_port(authority, Some(if tls { 443 } else { 80 }))?;
        return Ok(Plan::Room {
            tls,
            host,
            port,
            room: room.to_string(),
        });
    }
    let path = if path.is_empty() { "/" } else { path };
    match scheme.as_deref() {
        Some("ws") | Some("wss") => {
            let tls = scheme.as_deref() == Some("wss");
            let (host, port) = host_port(authority, Some(if tls { 443 } else { 80 }))?;
            Ok(Plan::Direct(vec![Target {
                tls,
                host,
                port,
                path: path.into(),
            }]))
        }
        None => {
            let (host, port) = host_port(authority, None)?;
            Ok(Plan::Direct(vec![
                Target {
                    tls: true,
                    host: host.clone(),
                    port,
                    path: path.into(),
                },
                Target {
                    tls: false,
                    host,
                    port,
                    path: path.into(),
                },
            ]))
        }
        Some(s) => Err(format!("links cannot use {s}://")),
    }
}

/// GET a page; returns the status and the body (at most `max` bytes).
pub async fn get(
    tls: bool,
    host: &str,
    port: u16,
    path: &str,
    max: usize,
    check: &(dyn Fn(IpAddr) -> bool + Sync),
) -> Result<(u16, Vec<u8>), String> {
    let mut s: Stream = ws::open(tls, host, port, check).await?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: metaxy\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let (head, mut body) = ws::read_head(&mut s, 32 * 1024).await?;
    let mut chunk = [0u8; 16384];
    loop {
        if body.len() > max {
            return Err("the page is too big".into());
        }
        match s.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            // a TLS close without close_notify still ends the page
            Err(_) => break,
        }
    }
    let chunked = head.lines().skip(1).any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        })
    });
    if chunked {
        body = unchunk(&body)?;
    }
    Ok((ws::status(&head), body))
}

fn unchunk(b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let nl = b[i..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("a cut chunk")?;
        let size = std::str::from_utf8(&b[i..i + nl]).map_err(|_| "a bad chunk size")?;
        let size = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "a bad chunk size")?;
        i += nl + 2;
        if size == 0 {
            return Ok(out);
        }
        if i + size > b.len() {
            return Err("a cut chunk".into());
        }
        out.extend_from_slice(&b[i..i + size]);
        i += size + 2;
    }
}

/// The room's current port: open the room page first (it wakes a sleeping
/// room), then read `/api/room_status/<room>` (`last_port`).
pub async fn room_port(
    tls: bool,
    host: &str,
    port: u16,
    room: &str,
    check: &(dyn Fn(IpAddr) -> bool + Sync),
) -> Result<u16, String> {
    let _ = get(tls, host, port, &format!("/room/{room}"), 2 << 20, check).await;
    let (st, body) = get(
        tls,
        host,
        port,
        &format!("/api/room_status/{room}"),
        2 << 20,
        check,
    )
    .await?;
    if st != 200 {
        return Err(format!("room status: HTTP {st}"));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("room status: {e}"))?;
    v["last_port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| "the room has no port yet".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(tls: bool, host: &str, port: u16, path: &str) -> Target {
        Target {
            tls,
            host: host.into(),
            port,
            path: path.into(),
        }
    }

    #[test]
    fn links() {
        assert_eq!(
            plan("https://archipelago.gg/room/AbC-9_x").unwrap(),
            Plan::Room {
                tls: true,
                host: "archipelago.gg".into(),
                port: 443,
                room: "AbC-9_x".into()
            }
        );
        assert_eq!(
            plan("archipelago.gg/room/r1/").unwrap(),
            Plan::Room {
                tls: true,
                host: "archipelago.gg".into(),
                port: 443,
                room: "r1".into()
            }
        );
        assert_eq!(
            plan("http://127.0.0.1:8080/room/r1").unwrap(),
            Plan::Room {
                tls: false,
                host: "127.0.0.1".into(),
                port: 8080,
                room: "r1".into()
            }
        );
        assert_eq!(
            plan("archipelago.gg:38281").unwrap(),
            Plan::Direct(vec![
                t(true, "archipelago.gg", 38281, "/"),
                t(false, "archipelago.gg", 38281, "/")
            ])
        );
        assert_eq!(
            plan("ws://localhost:38281").unwrap(),
            Plan::Direct(vec![t(false, "localhost", 38281, "/")])
        );
        assert_eq!(
            plan("wss://ap.example.org/x").unwrap(),
            Plan::Direct(vec![t(true, "ap.example.org", 443, "/x")])
        );
        for bad in [
            "",
            "archipelago.gg",
            "ftp://x:1",
            "a b:1",
            "x:port",
            "https://x/room/",
            "https://x/room/a?b",
            "user@host:1",
        ] {
            assert!(plan(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn chunked_bodies() {
        assert_eq!(
            unchunk(b"5\r\nhello\r\n6;x=1\r\n world\r\n0\r\n\r\n").unwrap(),
            b"hello world"
        );
        assert!(unchunk(b"5\r\nhel").is_err());
    }
}
