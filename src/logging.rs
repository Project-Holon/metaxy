//! Log output: one line per event on standard error, with a UTC
//! timestamp, a level and named fields. Text by default, JSON lines on
//! request.
//!
//! The relay never logs message contents, keys or tokens. See section 8
//! of `docs/SPEC.md`.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `d` since the Unix epoch as an RFC 3339 UTC time with milliseconds.
pub fn rfc3339_ms(d: Duration) -> String {
    let secs = d.as_secs() as i64;
    let (y, mo, da) = civil(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{da:02}T{:02}:{:02}:{:02}.{:03}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60,
        d.subsec_millis()
    )
}

struct UtcMs;

impl FormatTime for UtcMs {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        w.write_str(&rfc3339_ms(d))
    }
}

/// Check a `log_level` value: a level (`error`, `warn`, `info`, `debug`,
/// `trace`, `off`) or filter directives such as `metaxy=debug`.
pub fn parse_level(level: &str) -> Result<EnvFilter, String> {
    let l = level.trim();
    let word = matches!(
        l.to_ascii_lowercase().as_str(),
        "error" | "warn" | "info" | "debug" | "trace" | "off"
    );
    if !word && !l.contains('=') {
        return Err(format!(
            "log_level \"{l}\" is not error, warn, info, debug, trace or off"
        ));
    }
    EnvFilter::try_new(l).map_err(|e| format!("log_level \"{l}\": {e}"))
}

/// Build a subscriber that writes to `w`.
pub fn subscriber<W>(
    level: &str,
    json: bool,
    w: W,
) -> Result<Box<dyn tracing::Subscriber + Send + Sync>, String>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = parse_level(level)?;
    let b = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(UtcMs)
        .with_target(false)
        .with_ansi(false)
        .with_writer(w);
    Ok(if json {
        Box::new(b.json().finish())
    } else {
        Box::new(b.finish())
    })
}

/// Install the logger for the whole process, writing to standard error.
pub fn init(level: &str, json: bool) -> Result<(), String> {
    let s = subscriber(level, json, std::io::stderr)?;
    tracing::subscriber::set_global_default(s).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_rfc3339_utc_with_milliseconds() {
        assert_eq!(rfc3339_ms(Duration::ZERO), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_ms(Duration::from_millis(1_700_000_000_123)),
            "2023-11-14T22:13:20.123Z"
        );
        // a leap day
        assert_eq!(
            rfc3339_ms(Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn levels_are_checked() {
        assert!(parse_level("debug").is_ok());
        assert!(parse_level("metaxy=debug,warn").is_ok());
        assert!(parse_level("loud").is_err());
    }
}
