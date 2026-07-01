//! YAML configuration.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Errors that can occur while loading the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse YAML config: {0}")]
    Parse(#[from] serde_norway::Error),
}

/// Runtime configuration. All fields have defaults, so a missing config file is
/// not an error — the exporter runs with sane defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address:port the HTTP server binds to (device ingest + `/metrics`).
    pub listen: SocketAddr,
    /// Path the Prometheus metrics are served on.
    pub metrics_path: String,
    /// Default log filter if `RUST_LOG` is unset.
    pub log_level: String,
    /// If set, only these serials are accepted (guards metric cardinality). An
    /// empty/`None` value accepts any plausible serial.
    pub allowed_serials: Option<Vec<String>>,
    /// Maximum accepted request body size in bytes.
    pub max_body_bytes: usize,
    /// Per-request timeout in seconds.
    pub request_timeout_secs: u64,
    /// Coulomb counter: cap the integration interval between frames to this many
    /// seconds, so a data gap (device offline) doesn't integrate a stale current.
    pub coulomb_max_gap_secs: u64,
    /// Hard cap on the number of distinct devices (serials) tracked, bounding
    /// metric cardinality / memory against untrusted `Sn` values. `0` = unlimited.
    pub max_devices: usize,
    /// If set, the coulomb counters are persisted to this JSON file and restored
    /// on startup, so charge/discharge totals survive restarts. `None` disables
    /// persistence (counters reset on restart).
    pub coulomb_state_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
            metrics_path: "/metrics".to_string(),
            log_level: "info".to_string(),
            allowed_serials: None,
            max_body_bytes: 64 * 1024,
            request_timeout_secs: 10,
            coulomb_max_gap_secs: 900,
            max_devices: 64,
            coulomb_state_path: None,
        }
    }
}

impl Config {
    /// Load from a YAML file. A non-existent path yields the defaults; any other
    /// I/O or parse error is returned.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] if the file exists but cannot be read, and
    /// [`ConfigError::Parse`] if its contents are not valid YAML for [`Config`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_norway::from_str(&text).map_err(ConfigError::from)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether a device-supplied serial should be accepted as a metric label.
    /// Rejects implausible serials (cardinality-bomb protection) and, when an
    /// allowlist is configured, anything outside it.
    pub fn accept_serial(&self, sn: &str) -> bool {
        if !is_plausible_serial(sn) {
            return false;
        }
        match &self.allowed_serials {
            Some(list) if !list.is_empty() => list.iter().any(|s| s == sn),
            _ => true,
        }
    }
}

/// A serial must be a short ASCII-alphanumeric token. This is the first line of
/// defence against an untrusted `Sn` exploding label cardinality.
fn is_plausible_serial(sn: &str) -> bool {
    !sn.is_empty() && sn.len() <= 64 && sn.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_file_missing() {
        let cfg = Config::load(Path::new("/nonexistent/daly-config.yaml")).unwrap();
        assert_eq!(cfg.listen.port(), 8080);
        assert_eq!(cfg.metrics_path, "/metrics");
    }

    #[test]
    fn parses_yaml_overrides() {
        let cfg: Config =
            serde_norway::from_str("listen: \"127.0.0.1:9000\"\nlog_level: debug\n").unwrap();
        assert_eq!(cfg.listen.port(), 9000);
        assert_eq!(cfg.log_level, "debug");
        // Untouched fields keep their defaults.
        assert_eq!(cfg.max_body_bytes, 64 * 1024);
    }

    #[test]
    fn serial_validation() {
        let cfg = Config::default();
        assert!(cfg.accept_serial("224KE220900366"));
        assert!(!cfg.accept_serial("")); // empty
        assert!(!cfg.accept_serial("bad serial!")); // non-alnum
        assert!(!cfg.accept_serial(&"x".repeat(65))); // too long

        let cfg = Config {
            allowed_serials: Some(vec!["AAA".to_string()]),
            ..Config::default()
        };
        assert!(cfg.accept_serial("AAA"));
        assert!(!cfg.accept_serial("BBB"));
    }
}
