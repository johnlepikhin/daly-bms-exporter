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
    /// Drop a realtime frame whose |pack current| exceeds this many amperes
    /// before it can reach any metric — gauges as well as the coulomb/energy
    /// counters. The wire encoding spans -3000..+3553 A; the controller has been
    /// seen emitting a single 436.6 A register inside an otherwise sane frame,
    /// and one such frame would dump kilowatt-hours into a monotonic counter,
    /// irreversibly. Observed production peak: ~33 A.
    pub max_plausible_current_amperes: f64,
    /// Plausible pack-voltage window (volts) for the same gate. The encoding
    /// spans 0..6553 V; the production bank (LTO 24S) runs at 22..29 V.
    pub min_plausible_pack_volts: f64,
    pub max_plausible_pack_volts: f64,
    /// Second line of defence: cap what a *single* frame may add to the
    /// counters, in case the integration itself misbehaves.
    ///
    /// The defaults sit just above what the gate and `coulomb_max_gap_secs`
    /// already permit together (`max_plausible_current_amperes` sustained for a
    /// whole gap), because that combination is a real reading after a comms
    /// drop, not an anomaly — clamping it would under-count energy after every
    /// outage. Lower them only if the pack physically cannot take that much in
    /// one interval; startup warns when they are set below that product.
    pub max_frame_amp_hours: f64,
    pub max_frame_watt_hours: f64,
    /// Run the pack current-sensor self-calibration. The estimator watches the
    /// charge balance `∫I dt = ΔQ + ∫I_balance dt + offset·T` and subtracts the
    /// offset it finds, feeding a parallel set of `daly_bms_calibrated_*`
    /// counters; the raw counters are never touched. Set to `false` to stop the
    /// estimator entirely (the calibrated series then stop updating).
    pub calibration_enabled: bool,
    /// Forgetting time constant of the estimator, in integrated hours. Long,
    /// because the offset drifts far more slowly than the noise around it and
    /// because the accuracy bound below scales with the window.
    pub calibration_tau_hours: f64,
    /// Integrated hours that must accumulate before any offset is applied. A
    /// floor under `calibration_max_anchor_error_amperes`, which normally binds
    /// first.
    pub calibration_min_span_hours: f64,
    /// Hard clamp on the applied offset (amperes), whatever the estimator says.
    pub calibration_max_offset_amperes: f64,
    /// Hold the correction until the estimate is provably good to within this
    /// many amperes.
    ///
    /// The BMS's own state of charge anchors the estimate and is bounded by the
    /// pack's capacity, so it can shift the result by at most `Cap / window` —
    /// and the exporter knows that number exactly. On a 40 Ah pack, 0.05 A is
    /// reached after ~800 integrated hours (~33 days). Lower it for a stricter
    /// correction that takes proportionally longer to engage.
    pub calibration_max_anchor_error_amperes: f64,
    /// Freeze the correction when the parallel-peer cross-check and the charge
    /// balance disagree by more than this (amperes).
    ///
    /// They are *not* expected to agree exactly: the peer regression measures
    /// the sensor alone, while the balance also absorbs real losses the balancer
    /// under-reports. In production the two sit ~50 mA apart. This threshold is
    /// for gross breakage — a failing sensor, a pack leaving the bank — not for
    /// policing that difference.
    pub calibration_peer_max_disagreement_amperes: f64,
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
            max_frame_amp_hours: 25.0,
            max_frame_watt_hours: 900.0,
            calibration_enabled: true,
            calibration_tau_hours: 1440.0,
            calibration_min_span_hours: 168.0,
            calibration_max_offset_amperes: 0.5,
            calibration_max_anchor_error_amperes: 0.05,
            calibration_peer_max_disagreement_amperes: 0.25,
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
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_norway::from_str::<Self>(&text).map_err(ConfigError::from)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Replace out-of-range values with their defaults, warning about each.
    ///
    /// Call this *after* the tracing subscriber is installed — otherwise every
    /// warning below is a no-op and the substitution is silent, which defeats
    /// the point of substituting rather than failing.
    ///
    /// These knobs are not merely cosmetic: `0` for the write interval removes
    /// the only rate limit on fsyncs triggered by unauthenticated input, and a
    /// value of, say, 86400 would silently turn `/metrics` into a day-stale
    /// feed. Rejecting the file outright would be worse — the exporter would
    /// refuse to start over a typo — so substitute and say so.
    ///
    /// `NaN` (a legal YAML `.nan`) is tested for explicitly: every comparison
    /// against it is false, so a `NaN` threshold would slip through validation
    /// and then switch the plausibility gate off without a word.
    pub fn reset_out_of_range_to_defaults(&mut self) {
        let d = Self::default();
        if !(1..=3600).contains(&self.coulomb_state_min_interval_secs) {
            tracing::warn!(
                value = self.coulomb_state_min_interval_secs,
                default = d.coulomb_state_min_interval_secs,
                "coulomb_state_min_interval_secs outside 1..=3600; using the default"
            );
            self.coulomb_state_min_interval_secs = d.coulomb_state_min_interval_secs;
        }
        if self.max_plausible_current_amperes.is_nan() || self.max_plausible_current_amperes <= 0.0
        {
            tracing::warn!(
                value = self.max_plausible_current_amperes,
                "max_plausible_current_amperes must be a positive number; using the default"
            );
            self.max_plausible_current_amperes = d.max_plausible_current_amperes;
        }
        // An inverted window would reject every frame and silently switch off
        // energy accounting altogether, so fall back rather than honour it.
        if self.min_plausible_pack_volts.is_nan()
            || self.max_plausible_pack_volts.is_nan()
            || self.min_plausible_pack_volts >= self.max_plausible_pack_volts
        {
            tracing::warn!(
                min = self.min_plausible_pack_volts,
                max = self.max_plausible_pack_volts,
                "plausible pack-voltage window is not an ordered pair; using the defaults"
            );
            self.min_plausible_pack_volts = d.min_plausible_pack_volts;
            self.max_plausible_pack_volts = d.max_plausible_pack_volts;
        }
        if self.max_frame_amp_hours.is_nan() || self.max_frame_amp_hours <= 0.0 {
            tracing::warn!(
                value = self.max_frame_amp_hours,
                "max_frame_amp_hours must be a positive number; using the default"
            );
            self.max_frame_amp_hours = d.max_frame_amp_hours;
        }
        // Below this the clamp starts eating legitimate readings rather than
        // anomalies: a full-length comms gap at the highest allowed current is
        // recoverable data, not a glitch.
        let gap_hours = self.coulomb_max_gap_secs as f64 / 3600.0;
        let reachable_ah = self.max_plausible_current_amperes * gap_hours;
        if self.max_frame_amp_hours < reachable_ah {
            tracing::warn!(
                ceiling = self.max_frame_amp_hours,
                reachable = reachable_ah,
                "max_frame_amp_hours is below what the plausibility gate and                  coulomb_max_gap_secs allow; long comms gaps will be under-counted"
            );
        }
        let reachable_wh = reachable_ah * self.max_plausible_pack_volts;
        if self.max_frame_watt_hours < reachable_wh {
            tracing::warn!(
                ceiling = self.max_frame_watt_hours,
                reachable = reachable_wh,
                "max_frame_watt_hours is below what the plausibility gate and                  coulomb_max_gap_secs allow; long comms gaps will be under-counted"
            );
        }
        if self.max_frame_watt_hours.is_nan() || self.max_frame_watt_hours <= 0.0 {
            tracing::warn!(
                value = self.max_frame_watt_hours,
                "max_frame_watt_hours must be a positive number; using the default"
            );
            self.max_frame_watt_hours = d.max_frame_watt_hours;
        }
        self.reset_calibration_out_of_range(&d);
    }

    /// Same substitute-and-warn treatment for the calibration knobs.
    ///
    /// Split out purely to keep `reset_out_of_range_to_defaults` readable. The
    /// stakes here are lower than for the plausibility gate — a nonsensical
    /// value can only skew the `daly_bms_calibrated_*` series, never the raw
    /// counters — but a silently disabled correction is still worth a warning.
    fn reset_calibration_out_of_range(&mut self, d: &Self) {
        // Non-positive or NaN would make the forgetting factor `exp(-h/tau)`
        // meaningless and the anchor bound infinite.
        if self.calibration_tau_hours.is_nan() || self.calibration_tau_hours <= 0.0 {
            tracing::warn!(
                value = self.calibration_tau_hours,
                "calibration_tau_hours must be a positive number; using the default"
            );
            self.calibration_tau_hours = d.calibration_tau_hours;
        }
        if self.calibration_min_span_hours.is_nan() || self.calibration_min_span_hours < 0.0 {
            tracing::warn!(
                value = self.calibration_min_span_hours,
                "calibration_min_span_hours must not be negative; using the default"
            );
            self.calibration_min_span_hours = d.calibration_min_span_hours;
        }
        // Zero is a legal, meaningful setting for both thresholds below: it
        // holds the correction forever while still publishing the estimate,
        // which is exactly how to run the estimator in observe-only mode.
        if self.calibration_max_offset_amperes.is_nan() || self.calibration_max_offset_amperes < 0.0
        {
            tracing::warn!(
                value = self.calibration_max_offset_amperes,
                "calibration_max_offset_amperes must not be negative; using the default"
            );
            self.calibration_max_offset_amperes = d.calibration_max_offset_amperes;
        }
        if self.calibration_max_anchor_error_amperes.is_nan()
            || self.calibration_max_anchor_error_amperes < 0.0
        {
            tracing::warn!(
                value = self.calibration_max_anchor_error_amperes,
                "calibration_max_anchor_error_amperes must not be negative; using the default"
            );
            self.calibration_max_anchor_error_amperes = d.calibration_max_anchor_error_amperes;
        }
        // A non-positive threshold would freeze the correction the moment any
        // peer is found, which reads as "calibration mysteriously stopped".
        if self.calibration_peer_max_disagreement_amperes.is_nan()
            || self.calibration_peer_max_disagreement_amperes <= 0.0
        {
            tracing::warn!(
                value = self.calibration_peer_max_disagreement_amperes,
                "calibration_peer_max_disagreement_amperes must be positive; using the default"
            );
            self.calibration_peer_max_disagreement_amperes =
                d.calibration_peer_max_disagreement_amperes;
        }
        // The anchor gate needs `capacity/window <= threshold`, so a window
        // shorter than the forgetting constant is what actually binds. Warn when
        // the span floor is so long that it, not the accuracy bound, decides.
        if self.calibration_min_span_hours > self.calibration_tau_hours {
            tracing::warn!(
                min_span = self.calibration_min_span_hours,
                tau = self.calibration_tau_hours,
                "calibration_min_span_hours exceeds calibration_tau_hours; the estimator never sees a window that long"
            );
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
/// that module stays independent of the YAML layer: `metrics` imports nothing
/// from `config`, so the dependency runs one way only. It cannot live in
/// `main.rs` either — both types would be foreign there (orphan rule).
impl From<&Config> for crate::metrics::MetricsOptions {
    fn from(c: &Config) -> Self {
        Self {
            coulomb_max_gap_secs: c.coulomb_max_gap_secs as f64,
            max_devices: c.max_devices,
            coulomb_state_path: c.coulomb_state_path.clone(),
            state_min_interval: std::time::Duration::from_secs(c.coulomb_state_min_interval_secs),
            max_frame_amp_hours: c.max_frame_amp_hours,
            max_frame_watt_hours: c.max_frame_watt_hours,
            calibration_enabled: c.calibration_enabled,
            calibration: crate::calibration::Options {
                tau_hours: c.calibration_tau_hours,
                min_span_hours: c.calibration_min_span_hours,
                max_offset_amperes: c.calibration_max_offset_amperes,
                max_anchor_error_amperes: c.calibration_max_anchor_error_amperes,
                peer_max_disagreement_amperes: c.calibration_peer_max_disagreement_amperes,
                // The interval cap must match the counter's, or the estimator
                // would measure time the counter did not integrate over.
                max_gap_secs: c.coulomb_max_gap_secs as f64,
                ..crate::calibration::Options::default()
            },
        }
    }
}

/// The plausibility gate thresholds, applied by the ingest handler to every
/// realtime frame before it reaches any metric. Same one-way dependency
/// argument as for `MetricsOptions` above.
impl From<&Config> for crate::decode::PlausibilityLimits {
    fn from(c: &Config) -> Self {
        Self {
            max_current_a: c.max_plausible_current_amperes,
            pack_volts: (c.min_plausible_pack_volts, c.max_plausible_pack_volts),
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
        cfg.reset_out_of_range_to_defaults();
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
    fn metrics_defaults_match_config_defaults() {
        // The two Default impls carry the same numbers, and only this test keeps
        // them in step: unit tests build Metrics from MetricsOptions::default(),
        // while production goes through Config -> MetricsOptions. A drift would
        // leave the production thresholds untested and the tested ones unshipped.
        let from_config: crate::metrics::MetricsOptions = (&Config::default()).into();
        let standalone = crate::metrics::MetricsOptions::default();
        assert_eq!(
            from_config.coulomb_max_gap_secs,
            standalone.coulomb_max_gap_secs
        );
        assert_eq!(from_config.max_devices, standalone.max_devices);
        assert_eq!(
            from_config.state_min_interval,
            standalone.state_min_interval
        );
        assert_eq!(
            from_config.max_frame_amp_hours,
            standalone.max_frame_amp_hours
        );
        assert_eq!(
            from_config.max_frame_watt_hours,
            standalone.max_frame_watt_hours
        );
    }

    #[test]
    fn plausibility_limits_come_from_config() {
        let cfg = Config {
            max_plausible_current_amperes: 42.0,
            min_plausible_pack_volts: 20.0,
            max_plausible_pack_volts: 30.0,
            ..Config::default()
        };
        let limits: crate::decode::PlausibilityLimits = (&cfg).into();
        assert_eq!(
            limits,
            crate::decode::PlausibilityLimits {
                max_current_a: 42.0,
                pack_volts: (20.0, 30.0),
            }
        );
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

    /// `config.example.yaml` ships inside the .deb and is what an operator
    /// copies to get started, so a typo in it is a production footgun that no
    /// other test would catch. Parsing it here also pins the documented
    /// calibration values to the real defaults, so the two cannot drift apart.
    #[test]
    fn the_example_config_parses_and_documents_the_real_defaults() {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.yaml"))
                .expect("config.example.yaml is part of the repo");
        let cfg: Config = serde_norway::from_str(&text).expect("example config must be valid YAML");
        let d = Config::default();
        assert_eq!(cfg.calibration_enabled, d.calibration_enabled);
        assert_eq!(cfg.calibration_tau_hours, d.calibration_tau_hours);
        assert_eq!(cfg.calibration_min_span_hours, d.calibration_min_span_hours);
        assert_eq!(
            cfg.calibration_max_offset_amperes,
            d.calibration_max_offset_amperes
        );
        assert_eq!(
            cfg.calibration_max_anchor_error_amperes,
            d.calibration_max_anchor_error_amperes
        );
        assert_eq!(
            cfg.calibration_peer_max_disagreement_amperes,
            d.calibration_peer_max_disagreement_amperes
        );
        assert_eq!(cfg.max_frame_amp_hours, d.max_frame_amp_hours);
        assert_eq!(
            cfg.max_plausible_current_amperes,
            d.max_plausible_current_amperes
        );
    }
}
