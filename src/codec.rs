//! Frame codec: `<type> <length>\n<payload>`.

/// Longest header (type, space, length digits) before the newline.
pub const MAX_HEADER: usize = 16;
/// Largest message in a `D` frame.
pub const MAX_DATA: usize = 4096;
/// Longest key in a `D` frame.
pub const MAX_KEY: usize = 96;
/// The protocol name and version the relay speaks.
pub const PROTO: &str = "eidos-net/1";
/// Longest display name in bytes.
pub const MAX_NAME: usize = 64;

/// Frame types a client may send.
const CLIENT_TYPES: &[u8] = b"HJDBKQ";

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

/// An `E` frame to send: the reason word, then plain text for people.
///
/// The reason word is what clients match on and never changes. The text
/// never holds message contents, keys or tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail {
    pub reason: &'static str,
    pub detail: String,
}

impl Fail {
    pub fn new(reason: &'static str, detail: impl Into<String>) -> Fail {
        Fail {
            reason,
            detail: detail.into(),
        }
    }

    /// The payload of the `E` frame: `<reason> <detail>`.
    pub fn text(&self) -> String {
        if self.detail.is_empty() {
            self.reason.to_string()
        } else {
            format!("{} {}", self.reason, self.detail)
        }
    }

    /// The whole `E` frame.
    pub fn frame(&self) -> Vec<u8> {
        encode(b'E', self.text().as_bytes())
    }
}

/// Show client bytes in a log line or an error text: printable ASCII as
/// is, anything else escaped, cut to `max` input bytes.
pub fn show(bytes: &[u8], max: usize) -> String {
    let cut = &bytes[..bytes.len().min(max)];
    let mut s = cut.escape_ascii().to_string();
    if bytes.len() > max {
        s.push_str("...");
    }
    s
}

/// Why a frame was refused. The text says what was wrong.
#[derive(Debug, Clone)]
pub enum CodecError {
    /// Header too long, unknown type, or bad length.
    BadFrame(String),
    /// Length over the limit.
    TooBig(String),
}

/// Two errors are equal when they are the same kind (the text is for
/// people).
impl PartialEq for CodecError {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}
impl Eq for CodecError {}

impl From<CodecError> for Fail {
    fn from(e: CodecError) -> Fail {
        match e {
            CodecError::BadFrame(d) => Fail::new("bad-frame", d),
            CodecError::TooBig(d) => Fail::new("too-big", d),
        }
    }
}

/// Encode a frame the relay sends.
pub fn encode(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.push(kind);
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(payload);
    out
}

/// Try to read one client frame from the start of `buf`.
///
/// Returns `Ok(None)` when more bytes are needed, and the frame with the
/// number of bytes it used otherwise. Errors are reported as soon as the
/// bytes seen so far cannot be a valid header.
pub fn parse(buf: &[u8], max_payload: usize) -> Result<Option<(Frame, usize)>, CodecError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if !CLIENT_TYPES.contains(&buf[0]) {
        return Err(CodecError::BadFrame(format!(
            "unknown frame type \"{}\"",
            show(&buf[..1], 1)
        )));
    }
    if buf.len() >= 2 && buf[1] != b' ' {
        return Err(CodecError::BadFrame(format!(
            "no space after the frame type \"{}\"",
            buf[0] as char
        )));
    }
    let mut nl = None;
    for (i, &b) in buf.iter().enumerate().skip(2) {
        if b == b'\n' {
            nl = Some(i);
            break;
        }
        if !b.is_ascii_digit() {
            return Err(CodecError::BadFrame(format!(
                "length \"{}\" is not a number",
                show(&buf[2..=i], MAX_HEADER)
            )));
        }
        if i >= MAX_HEADER {
            return Err(CodecError::BadFrame(format!(
                "header is longer than {MAX_HEADER} bytes"
            )));
        }
    }
    let Some(nl) = nl else {
        return Ok(None);
    };
    let digits = &buf[2..nl];
    if digits.is_empty() {
        return Err(CodecError::BadFrame("length is empty".into()));
    }
    if digits.len() > 1 && digits[0] == b'0' {
        return Err(CodecError::BadFrame(format!(
            "length \"{}\" has a leading zero",
            show(digits, MAX_HEADER)
        )));
    }
    let mut len: u64 = 0;
    for &d in digits {
        len = len * 10 + u64::from(d - b'0');
    }
    if len > max_payload as u64 {
        return Err(CodecError::TooBig(format!(
            "frame of {len} bytes (limit {max_payload})"
        )));
    }
    let len = len as usize;
    let end = nl + 1 + len;
    if buf.len() < end {
        return Ok(None);
    }
    Ok(Some((
        Frame {
            kind: buf[0],
            payload: buf[nl + 1..end].to_vec(),
        },
        end,
    )))
}

/// Why an `H` or `J` payload was refused.
#[derive(Debug, Clone)]
pub enum Reject {
    Version(String),
    BadFrame(String),
}

/// Equal when the same kind (the text is for people).
impl PartialEq for Reject {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}
impl Eq for Reject {}

impl From<Reject> for Fail {
    fn from(r: Reject) -> Fail {
        match r {
            Reject::Version(d) => Fail::new("version", d),
            Reject::BadFrame(d) => Fail::new("bad-frame", d),
        }
    }
}

/// The fields of an `H` or `J` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub token: String,
    /// Upper-cased code; empty for `H`.
    pub code: String,
    pub name: String,
}

/// Cut a name to 64 bytes on a character boundary.
fn trim_name(name: &str) -> &str {
    if name.len() <= MAX_NAME {
        return name;
    }
    let mut end = MAX_NAME;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

fn valid_token(t: &[u8]) -> bool {
    (16..=64).contains(&t.len()) && t.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Parse the payload of `H` (`join` false) or `J` (`join` true).
pub fn parse_hello(payload: &[u8], join: bool) -> Result<Hello, Reject> {
    let k = if join { 'J' } else { 'H' };
    let bad = |d: String| Reject::BadFrame(d);
    let fields = if join { 4 } else { 3 };
    let mut parts = payload.splitn(fields, |&b| b == b' ');
    let version = parts.next().unwrap_or(&[]);
    if version != PROTO.as_bytes() {
        return Err(Reject::Version(format!(
            "got \"{}\", this relay speaks {PROTO}",
            show(version, 32)
        )));
    }
    let token = parts
        .next()
        .ok_or_else(|| bad(format!("{k} has no token")))?;
    if !valid_token(token) {
        return Err(bad(format!(
            "{k} token must be 16 to 64 characters of 0-9 and a-f (got {} bytes)",
            token.len()
        )));
    }
    let code = if join {
        let c = parts.next().ok_or_else(|| bad("J has no code".into()))?;
        std::str::from_utf8(c)
            .map_err(|_| bad("J code is not valid UTF-8".into()))?
            .to_ascii_uppercase()
    } else {
        String::new()
    };
    let name = parts
        .next()
        .ok_or_else(|| bad(format!("{k} has no name")))?;
    let name =
        std::str::from_utf8(name).map_err(|_| bad(format!("{k} name is not valid UTF-8")))?;
    if name.is_empty() {
        return Err(bad(format!("{k} name is empty")));
    }
    if name.chars().any(char::is_control) {
        return Err(bad(format!("{k} name has a control character")));
    }
    Ok(Hello {
        token: String::from_utf8_lossy(token).into_owned(),
        code,
        name: trim_name(name).to_string(),
    })
}

/// Parse the payload of `Q`: a decimal peer number.
pub fn parse_peer(payload: &[u8]) -> Result<u32, CodecError> {
    if payload.is_empty() || payload.len() > 4 || !payload.iter().all(u8::is_ascii_digit) {
        return Err(CodecError::BadFrame(format!(
            "Q peer \"{}\" is not a number of up to 4 digits",
            show(payload, 16)
        )));
    }
    Ok(payload
        .iter()
        .fold(0u32, |a, &d| a * 10 + u32::from(d - b'0')))
}

/// A parsed `D` frame from a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Data {
    /// 0 to 16 as sent; larger numbers parse and name nobody.
    pub peer: u32,
    /// The payload after the first newline: `<key>\n<bytes>`.
    pub rest: Vec<u8>,
}

/// Parse the payload of a client `D`. The texts never repeat the key or
/// the message.
pub fn parse_data(payload: &[u8]) -> Result<Data, CodecError> {
    let bad = |d: &str| CodecError::BadFrame(d.to_string());
    let nl = payload
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| bad("D has no newline after the peer"))?;
    let peer_bytes = &payload[..nl];
    if peer_bytes.is_empty() || peer_bytes.len() > 4 || !peer_bytes.iter().all(u8::is_ascii_digit) {
        return Err(bad("D peer is not a number of up to 4 digits"));
    }
    let peer = peer_bytes
        .iter()
        .fold(0u32, |a, &d| a * 10 + u32::from(d - b'0'));
    let rest = &payload[nl + 1..];
    let knl = rest
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| bad("D has no newline after the key"))?;
    if knl == 0 {
        return Err(bad("D key is empty"));
    }
    if knl > MAX_KEY {
        return Err(CodecError::BadFrame(format!(
            "D key of {knl} bytes (limit {MAX_KEY})"
        )));
    }
    let msg = rest.len() - knl - 1;
    if msg > MAX_DATA {
        return Err(CodecError::TooBig(format!(
            "D message of {msg} bytes (limit {MAX_DATA})"
        )));
    }
    Ok(Data {
        peer,
        rest: rest.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 4224;

    fn tok() -> String {
        "a".repeat(32)
    }

    #[test]
    fn parses_empty_and_full_frames() {
        let (f, n) = parse(b"B 0\nrest", MAX).unwrap().unwrap();
        assert_eq!((f.kind, f.payload.len(), n), (b'B', 0, 4));
        let (f, n) = parse(b"D 2\ngo", MAX).unwrap().unwrap();
        assert_eq!((f.payload, n), (b"go".to_vec(), 6));
    }

    #[test]
    fn needs_more_bytes() {
        assert_eq!(parse(b"", MAX), Ok(None));
        assert_eq!(parse(b"D", MAX), Ok(None));
        assert_eq!(parse(b"D ", MAX), Ok(None));
        assert_eq!(parse(b"D 12", MAX), Ok(None));
        assert_eq!(parse(b"D 3\nab", MAX), Ok(None));
    }

    #[test]
    fn unknown_type() {
        assert_eq!(
            parse(b"Z 0\n", MAX),
            Err(CodecError::BadFrame(String::new()))
        );
        assert_eq!(
            parse(b"h 0\n", MAX),
            Err(CodecError::BadFrame(String::new()))
        );
        // relay-only types are unknown to the relay
        assert_eq!(
            parse(b"E 1\nx", MAX),
            Err(CodecError::BadFrame(String::new()))
        );
        assert_eq!(parse(b"\n", MAX), Err(CodecError::BadFrame(String::new())));
    }

    #[test]
    fn bad_separator() {
        assert_eq!(
            parse(b"D\t1\nx", MAX),
            Err(CodecError::BadFrame(String::new()))
        );
        assert_eq!(
            parse(b"D1\nx", MAX),
            Err(CodecError::BadFrame(String::new()))
        );
    }

    #[test]
    fn bad_length() {
        for h in [
            &b"D \nx"[..],
            b"D -1\nx",
            b"D +1\nx",
            b"D 01\nx",
            b"D 00\n",
            b"D 1a\nx",
            b"D 1 \nx",
            b"D 0x1\nx",
        ] {
            assert_eq!(
                parse(h, MAX),
                Err(CodecError::BadFrame(String::new())),
                "{h:?}"
            );
        }
    }

    #[test]
    fn header_length_limit() {
        // 16 bytes before the newline is fine, 17 is not
        let ok = format!("D 1{}\n", "0".repeat(13));
        assert_eq!(ok.len() - 1, 16);
        assert_eq!(
            parse(ok.as_bytes(), MAX),
            Err(CodecError::TooBig(String::new()))
        );
        let long = format!("D 1{}", "0".repeat(15));
        assert_eq!(
            parse(long.as_bytes(), MAX),
            Err(CodecError::BadFrame(String::new()))
        );
    }

    #[test]
    fn too_big_is_seen_before_the_payload() {
        assert_eq!(
            parse(b"D 4225\n", MAX),
            Err(CodecError::TooBig(String::new()))
        );
        assert!(parse(b"D 4224\n", MAX).unwrap().is_none());
    }

    #[test]
    fn hello_host() {
        let p = format!("eidos-net/1 {} Ann Lee", tok());
        let h = parse_hello(p.as_bytes(), false).unwrap();
        assert_eq!((h.name.as_str(), h.code.as_str()), ("Ann Lee", ""));
    }

    #[test]
    fn hello_join_upper_cases_code() {
        let p = format!("eidos-net/1 {} ab3k9z Bob", tok());
        let h = parse_hello(p.as_bytes(), true).unwrap();
        assert_eq!((h.code.as_str(), h.name.as_str()), ("AB3K9Z", "Bob"));
    }

    #[test]
    fn hello_version() {
        let p = format!("eidos-net/2 {} Ann", tok());
        assert_eq!(
            parse_hello(p.as_bytes(), false),
            Err(Reject::Version(String::new()))
        );
        assert_eq!(parse_hello(b"", false), Err(Reject::Version(String::new())));
    }

    #[test]
    fn hello_token() {
        for t in ["", "abc", "ABCDEF0123456789ABCD", &"g".repeat(32)] {
            let p = format!("eidos-net/1 {t} Ann");
            assert_eq!(
                parse_hello(p.as_bytes(), false),
                Err(Reject::BadFrame(String::new()))
            );
        }
        let p = format!("eidos-net/1 {} Ann", "a".repeat(65));
        assert_eq!(
            parse_hello(p.as_bytes(), false),
            Err(Reject::BadFrame(String::new()))
        );
        let p = format!("eidos-net/1 {} Ann", "a".repeat(16));
        assert!(parse_hello(p.as_bytes(), false).is_ok());
        let p = format!("eidos-net/1 {} Ann", "a".repeat(64));
        assert!(parse_hello(p.as_bytes(), false).is_ok());
    }

    #[test]
    fn hello_name() {
        let t = tok();
        for bad in [
            format!("eidos-net/1 {t} "),
            format!("eidos-net/1 {t}"),
            format!("eidos-net/1 {t} A\nB"),
            format!("eidos-net/1 {t} A\u{7f}"),
        ] {
            assert_eq!(
                parse_hello(bad.as_bytes(), false),
                Err(Reject::BadFrame(String::new()))
            );
        }
        let mut bytes = format!("eidos-net/1 {t} ").into_bytes();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(
            parse_hello(&bytes, false),
            Err(Reject::BadFrame(String::new()))
        );
        // join without a name
        let p = format!("eidos-net/1 {t} ABCDEF");
        assert_eq!(
            parse_hello(p.as_bytes(), true),
            Err(Reject::BadFrame(String::new()))
        );
    }

    #[test]
    fn hello_name_trimmed_on_a_character_boundary() {
        let name = "é".repeat(40); // 80 bytes
        let p = format!("eidos-net/1 {} {name}", tok());
        let h = parse_hello(p.as_bytes(), false).unwrap();
        assert_eq!(h.name, "é".repeat(32));
        let name = format!("{}é", "a".repeat(63)); // the é straddles byte 64
        let p = format!("eidos-net/1 {} {name}", tok());
        assert_eq!(parse_hello(p.as_bytes(), false).unwrap().name.len(), 63);
    }

    #[test]
    fn data_ok() {
        let d = parse_data(b"0\nmod.a\n\x00\x01").unwrap();
        assert_eq!(d.peer, 0);
        assert_eq!(d.rest, b"mod.a\n\x00\x01");
        // a message with newlines in it
        assert!(parse_data(b"3\nk\na\nb\n").is_ok());
        // empty message
        assert!(parse_data(b"3\nk\n").is_ok());
    }

    #[test]
    fn data_malformed() {
        for p in [
            &b""[..],
            b"1",
            b"\nk\nx",
            b"a\nk\nx",
            b"-1\nk\nx",
            b"1\nk",
            b"1\n\nx",
            b"1\n",
            b"12345\nk\nx",
        ] {
            assert_eq!(
                parse_data(p),
                Err(CodecError::BadFrame(String::new())),
                "{p:?}"
            );
        }
        let mut long = b"1\n".to_vec();
        long.extend(std::iter::repeat_n(b'k', 97));
        long.extend_from_slice(b"\nx");
        assert_eq!(parse_data(&long), Err(CodecError::BadFrame(String::new())));
    }

    #[test]
    fn data_limits() {
        let mut p = b"1\nkey\n".to_vec();
        p.extend(std::iter::repeat_n(0u8, 4096));
        assert!(parse_data(&p).is_ok());
        p.push(0);
        assert_eq!(parse_data(&p), Err(CodecError::TooBig(String::new())));
        let mut p = b"1\n".to_vec();
        p.extend(std::iter::repeat_n(b'k', 96));
        p.extend_from_slice(b"\n");
        assert!(parse_data(&p).is_ok());
    }

    #[test]
    fn peer_payload() {
        assert_eq!(parse_peer(b"7"), Ok(7));
        assert_eq!(parse_peer(b"16"), Ok(16));
        for p in [
            &b""[..],
            b"x",
            b"-1",
            b"1 ",
            b"12345",
            b"1
",
        ] {
            assert_eq!(
                parse_peer(p),
                Err(CodecError::BadFrame(String::new())),
                "{p:?}"
            );
        }
    }

    #[test]
    fn query_type_is_a_client_type() {
        assert!(parse(
            b"Q 1
2", MAX
        )
        .unwrap()
        .is_some());
        for t in ["C", "W", "P", "X", "E"] {
            assert_eq!(
                parse(
                    format!(
                        "{t} 0
"
                    )
                    .as_bytes(),
                    MAX
                ),
                Err(CodecError::BadFrame(String::new()))
            );
        }
    }

    #[test]
    fn encode_round_trip() {
        assert_eq!(encode(b'E', b"full"), b"E 4\nfull");
    }
}
