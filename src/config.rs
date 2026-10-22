//! Settings: defaults, an optional TOML file, command-line flags.
//!
//! One list below defines the config struct, the flags and the defaults.
//! Order of precedence: flags, then the file, then the defaults.

use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

macro_rules! settings {
    ($( $([$($extra:tt)*])? $name:ident : $ty:ty = $def:expr, $help:literal; )*) => {
        /// Every setting of the relay.
        #[derive(Debug, Clone, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct Config {
            $( #[doc = $help] pub $name: $ty, )*
        }

        impl Default for Config {
            fn default() -> Self {
                Config { $( $name: $def, )* }
            }
        }

        /// Command-line flags. Each one overrides the same setting.
        #[derive(Debug, Parser)]
        #[command(name = "metaxy", version, about = "Relay server for eidos multiplayer sessions")]
        pub struct Args {
            /// Read settings from this TOML file.
            #[arg(long)]
            pub config: Option<PathBuf>,
            $( #[arg(long, help = $help $(, $($extra)*)?)] pub $name: Option<$ty>, )*
        }

        impl Args {
            /// Copy the flags that were given onto `cfg`.
            pub fn apply(self, cfg: &mut Config) {
                $( if let Some(v) = self.$name { cfg.$name = v; } )*
            }
        }
    };
}

settings! {
    bind: String = "0.0.0.0".to_string(), "Address to listen on";
    port: u16 = 4141, "Plain TCP port (0 picks a free one)";
    [num_args = 0..=1, default_missing_value = "true"] plain: bool = true, "Serve plain TCP (true or false)";
    tls_cert: String = String::new(), "TLS certificate chain, PEM file (with tls_key, turns TLS on)";
    tls_key: String = String::new(), "TLS private key, PEM file";
    tls_port: u16 = 443, "TLS port";
    max_payload: usize = 4096 + 128, "Largest frame payload in bytes";
    max_players: usize = 16, "Players in a session (1 to 16)";
    msg_rate: f64 = 60.0, "D frames per second per client";
    msg_burst: f64 = 256.0, "D frame burst per client";
    byte_rate: f64 = 524_288.0, "D payload bytes per second per client";
    byte_burst: f64 = 1_048_576.0, "D payload byte burst per client";
    rate_factor: f64 = 4.0, "Sustained excess: a window is over when it offers this many times the rate";
    rate_window_ms: u64 = 1000, "Length of one rate window in milliseconds";
    rate_close_windows: u32 = 5, "Consecutive over windows before E rate and close";
    queue_cap: usize = 2_097_152, "Queued bytes to one client before E too-slow";
    conn_per_window: usize = 10, "New connections per IP address per window";
    conn_window_ms: u64 = 60_000, "Window for new connections per IP address, milliseconds";
    sessions_per_ip: usize = 4, "Sessions with players connected that one IP address may have created";
    max_sessions: usize = 100_000, "Sessions kept in total (E busy beyond this)";
    handshake_ms: u64 = 10_000, "Time from connect to H or J, milliseconds";
    idle_ms: u64 = 60_000, "Silence before a connection is dropped, milliseconds";
    session_expiry_ms: u64 = 600_000, "A session is forgotten this long after its last player disconnected, milliseconds";
    log_level: String = "info".to_string(), "Log level: error, warn, info, debug, trace or off (or filter directives such as metaxy=debug)";
    [num_args = 0..=1, default_missing_value = "true"] log_json: bool = false, "Write logs as JSON lines";
}

impl Config {
    /// Load the TOML file, if there is one.
    pub fn from_file(path: &std::path::Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// True when both TLS files are set.
    pub fn tls_enabled(&self) -> bool {
        !self.tls_cert.is_empty() && !self.tls_key.is_empty()
    }

    /// Check the settings fit together.
    pub fn validate(&self) -> Result<(), String> {
        if self.tls_cert.is_empty() != self.tls_key.is_empty() {
            return Err("tls_cert and tls_key must be set together".into());
        }
        if !self.plain && !self.tls_enabled() {
            return Err("no listener: plain is off and TLS is not set".into());
        }
        if !(1..=16).contains(&self.max_players) {
            return Err("max_players must be 1 to 16".into());
        }
        if self.max_payload < 64 {
            return Err("max_payload is too small".into());
        }
        if self.rate_window_ms == 0 || self.rate_close_windows == 0 {
            return Err("rate_window_ms and rate_close_windows must be above 0".into());
        }
        crate::logging::parse_level(&self.log_level)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::default();
        assert_eq!(c.port, 4141);
        assert_eq!(c.tls_port, 443);
        assert_eq!(c.max_payload, 4224);
        assert_eq!(c.max_players, 16);
        assert_eq!(c.queue_cap, 2 << 20);
        assert_eq!((c.byte_rate, c.byte_burst), (524_288.0, 1_048_576.0));
        assert_eq!(c.session_expiry_ms, 600_000);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn file_overrides_defaults() {
        let c: Config = toml::from_str("port = 9000\nmsg_rate = 5.0\n").unwrap();
        assert_eq!(c.port, 9000);
        assert_eq!(c.msg_rate, 5.0);
        assert_eq!(c.idle_ms, 60_000);
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(toml::from_str::<Config>("prot = 1\n").is_err());
    }

    #[test]
    fn flags_override_the_file() {
        let mut c: Config = toml::from_str("port = 9000\n").unwrap();
        let a = Args::parse_from(["metaxy", "--port", "9100", "--max-players", "4"]);
        a.apply(&mut c);
        assert_eq!((c.port, c.max_players), (9100, 4));
    }

    #[test]
    fn log_flags() {
        let mut c = Config::default();
        assert_eq!((c.log_level.as_str(), c.log_json), ("info", false));
        Args::parse_from(["metaxy", "--log-level", "debug", "--log-json"]).apply(&mut c);
        assert_eq!((c.log_level.as_str(), c.log_json), ("debug", true));
        assert!(c.validate().is_ok());
        c.log_level = "loud".into();
        assert!(c.validate().is_err());
        let c: Config = toml::from_str(
            "log_level = \"warn\"
log_json = true
",
        )
        .unwrap();
        assert_eq!((c.log_level.as_str(), c.log_json), ("warn", true));
    }

    #[test]
    fn half_tls_is_refused() {
        let c = Config {
            tls_cert: "a.pem".into(),
            ..Config::default()
        };
        assert!(c.validate().is_err());
    }
}
