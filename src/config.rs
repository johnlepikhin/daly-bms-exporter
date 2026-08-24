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
    /// Reject a realtime frame whose |pack current| exceeds this many amperes
    /// before it can reach the coulomb/energy counters. The wire encoding spans
    /// -3000..+3553 A, so one corrupt frame would otherwise dump kilowatt-hours
    /// into a monotonic counter, irreversibly. Observed production peak: ~33 A.
    pub max_plausible_current_amperes: f64,
    /// Plausible pack-voltage window (volts) for the same gate. The encoding
    /// spans 0..6553 V; the production bank (LTO 24S) runs at 22..29 V.
    pub min_plausible_pack_volts: f64,
    pub max_plausible_pack_volts: f64,
    /// Second line of defence: cap what a *single* frame may add to the
    /// counters. The thresholds above only bound the reading, while the
    /// integration interval is bounded separately by `coulomb_max_gap_secs`, so
    /// their product still allows an implausibly large delta. Observed
    /// production peak per frame is far below these.
    pub max_frame_amp_hours: f64,
    pub max_frame_watt_hours: f64,
    /// Minimum interval between durable writes of the state file. Increments
    /// arriving inside the window are held in memory and applied to the exported
    /// counters only after the next successful write, so nothing is lost — but a
    /// `SIGKILL` drops up to one window's worth, so keep this below the device's
    /// frame interval. Clamped to `1..=3600`.
    pub coulomb_state_min_interval_secs: u64,
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
            max_plausible_current_amperes: 100.0,
            min_plausible_pack_volts: 18.0,
            max_plausible_pack_volts: 34.0,
            max_frame_amp_hours: 5.0,
            max_frame_watt_hours: 150.0,
            coulomb_state_min_interval_secs: 5,
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
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => serde_norway::from_str::<Self>(&text).map_err(ConfigError::from)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        cfg.clamp_to_sane_ranges();
        Ok(cfg)
    }

    /// Replace out-of-range values with their defaults, warning about each.
    ///
    /// These knobs are not merely cosmetic: `0` for the write interval removes
    /// the only rate limit on fsyncs triggered by unauthenticated input, and a
    /// value of, say, 86400 would silently turn `/metrics` into a day-stale
    /// feed. Rejecting the file outright would be worse — the exporter would
    /// refuse to start over a typo — so clamp and say so.
    fn clamp_to_sane_ranges(&mut self) {
        let d = Self::default();
        if !(1..=3600).contains(&self.coulomb_state_min_interval_secs) {
            tracing::warn!(
                value = self.coulomb_state_min_interval_secs,
                default = d.coulomb_state_min_interval_secs,
                "coulomb_state_min_interval_secs out of range 1..=3600; using the default"
            );
            self.coulomb_state_min_interval_secs = d.coulomb_state_min_interval_secs;
        }
        if self.max_plausible_current_amperes <= 0.0 {
            tracing::warn!(
                value = self.max_plausible_current_amperes,
                "max_plausible_current_amperes must be positive; using the default"
            );
            self.max_plausible_current_amperes = d.max_plausible_current_amperes;
        }
        // An inverted window would reject every frame and silently switch off
        // energy accounting altogether, so fall back rather than honour it.
        if self.min_plausible_pack_volts >= self.max_plausible_pack_volts {
            tracing::warn!(
                min = self.min_plausible_pack_volts,
                max = self.max_plausible_pack_volts,
                "plausible pack-voltage window is inverted; using the defaults"
            );
            self.min_plausible_pack_volts = d.min_plausible_pack_volts;
            self.max_plausible_pack_volts = d.max_plausible_pack_volts;
        }
        if self.max_frame_amp_hours <= 0.0 {
            tracing::warn!(
                value = self.max_frame_amp_hours,
                "max_frame_amp_hours must be positive; using the default"
            );
            self.max_frame_amp_hours = d.max_frame_amp_hours;
        }
        if self.max_frame_watt_hours <= 0.0 {
            tracing::warn!(
                value = self.max_frame_watt_hours,
                "max_frame_watt_hours must be positive; using the default"
            );
            self.max_frame_watt_hours = d.max_frame_watt_hours;
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

/// Mapping config -> metrics knobs. It lives here rather than in `metrics.rs` so
/// that module stays independent of the YAML layer (`config` imports nothing
/// from the crate, so this direction cannot cycle). It cannot live in `main.rs`
/// either — both types would be foreign there (orphan rule).
impl From<&Config> for crate::metrics::MetricsOptions {
    fn from(c: &Config) -> Self {
        Self {
            coulomb_max_gap_secs: c.coulomb_max_gap_secs as f64,
            max_devices: c.max_devices,
            coulomb_state_path: c.coulomb_state_path.clone(),
            state_min_interval: std::time::Duration::from_secs(c.coulomb_state_min_interval_secs),
            max_plausible_current_amperes: c.max_plausible_current_amperes,
            plausible_pack_volts: (c.min_plausible_pack_volts, c.max_plausible_pack_volts),
            max_frame_amp_hours: c.max_frame_amp_hours,
            max_frame_watt_hours: c.max_frame_watt_hours,
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
    fn plausibility_defaults_bracket_production_values() {
        let c = Config::default();
        // Observed production: |I| up to ~33 A, pack 22..29 V (LTO 24S).
        assert!(33.0 < c.max_plausible_current_amperes);
        assert!(c.min_plausible_pack_volts < 22.0 && 29.0 < c.max_plausible_pack_volts);
        // ...while the extremes of the wire encoding are rejected.
        assert!(3553.5 > c.max_plausible_current_amperes);
        assert!(6553.5 > c.max_plausible_pack_volts);
    }

    #[test]
    fn out_of_range_knobs_fall_back_to_defaults() {
        let d = Config::default();
        let mut cfg: Config = serde_norway::from_str(
            "coulomb_state_min_interval_secs: 0\n\
             min_plausible_pack_volts: 40.0\n\
             max_plausible_pack_volts: 10.0\n\
             max_plausible_current_amperes: -5.0\n\
             max_frame_amp_hours: 0.0\n",
        )
        .unwrap();
        cfg.clamp_to_sane_ranges();
        // 0 would remove the only rate limit on fsyncs from unauthenticated input.
        assert_eq!(
            cfg.coulomb_state_min_interval_secs,
            d.coulomb_state_min_interval_secs
        );
        // An inverted window would reject every frame and stop all accounting.
        assert_eq!(cfg.min_plausible_pack_volts, d.min_plausible_pack_volts);
        assert_eq!(cfg.max_plausible_pack_volts, d.max_plausible_pack_volts);
        assert_eq!(
            cfg.max_plausible_current_amperes,
            d.max_plausible_current_amperes
        );
        assert_eq!(cfg.max_frame_amp_hours, d.max_frame_amp_hours);
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
