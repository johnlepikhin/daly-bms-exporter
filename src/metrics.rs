//! Prometheus metric families and the update logic that maps decoded telemetry
//! onto them. All families are labelled by `sn`; per-cell/sensor series are
//! pruned when a device reports fewer cells so stale readings don't linger.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use prometheus::{
    CounterVec, Encoder, Gauge, GaugeVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
use serde::{Deserialize, Serialize};

use crate::calibration::{self, Calibrator, Report, Sample};
use crate::decode::{
    ConfigData, DROP_IMPLAUSIBLE_CURRENT, DROP_IMPLAUSIBLE_VOLTAGE, Limits, RealtimeData,
};

/// Every `daly_bms_frames_dropped_total{reason}` label the exporter can emit.
/// Kept as one list so the series can be pre-created at startup (see
/// [`Metrics::new`]); `server.rs` must only ever record reasons from here.
pub const DROP_REASONS: [&str; 11] = [
    "bad_json",
    "serial_rejected",
    "device_limit",
    "bad_command",
    "unknown_block",
    "bad_hex",
    "too_short",
    "bad_frame",
    "crc",
    DROP_IMPLAUSIBLE_CURRENT,
    DROP_IMPLAUSIBLE_VOLTAGE,
];

/// The one `daly_bms_coulomb_samples_rejected_total{reason}` label left now
/// that the plausibility gate drops frames upstream.
pub const REJECT_CLAMPED_DELTA: &str = "clamped_delta";

/// On-disk snapshot of the charge/energy counters, keyed by serial. The type and
/// the state file (`coulombs.json`) keep the "coulomb" name for backward
/// compatibility, though they now also carry energy (watt-hours).
#[derive(Debug, Default, Serialize, Deserialize)]
struct CoulombState {
    devices: BTreeMap<String, CoulombEntry>,
    /// Current-sensor calibration state. Rides in the same file as the counters
    /// it feeds, so the two can never be restored out of step with each other.
    /// Defaulted, so a state file written before calibration existed still loads.
    #[serde(default)]
    calibration: calibration::State,
}

/// Persisted charge (amp-hours) and energy (watt-hours) totals for one device.
/// The `*_wh` fields default to `0.0` so a pre-existing state file that only has
/// the `*_ah` fields still loads (energy simply starts fresh).
#[derive(Debug, Serialize, Deserialize)]
struct CoulombEntry {
    charge_ah: f64,
    discharge_ah: f64,
    #[serde(default)]
    charge_wh: f64,
    #[serde(default)]
    discharge_wh: f64,
    /// The same four totals recomputed from the calibrated current. Defaulted
    /// for the same reason as `*_wh`: an older state file simply starts them
    /// fresh, which is safe because they are separate series.
    #[serde(default)]
    cal_charge_ah: f64,
    #[serde(default)]
    cal_discharge_ah: f64,
    #[serde(default)]
    cal_charge_wh: f64,
    #[serde(default)]
    cal_discharge_wh: f64,
    /// Cumulative passive-balancer bleed (Ah).
    #[serde(default)]
    balance_ah: f64,
}

/// Directional split of an integrated quantity: exactly one side is non-zero.
/// Feeding current (A) yields amp-hours, feeding power (W) yields watt-hours.
///
/// A named type rather than a `(f64, f64)` tuple on purpose: the value is passed
/// through three hops (integrate -> accumulate -> counter), and swapping the two
/// sides anywhere would silently cross charge with discharge in *monotonic*
/// counters — an error that cannot be undone once exported.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Split {
    charge: f64,
    discharge: f64,
}

/// A [`Split`] after per-frame clamping, with a flag so the caller can report
/// that a clamp actually fired.
struct Clamped {
    value: Split,
    was_clamped: bool,
}

impl Split {
    /// Cap each direction at `max`. The plausibility thresholds bound the
    /// *reading* and `coulomb_max_gap_secs` bounds the *interval*, but their
    /// product is still far above anything physical, so the delta itself needs
    /// its own ceiling before it reaches a monotonic counter.
    fn clamped(self, max: f64) -> Clamped {
        let value = Split {
            charge: self.charge.min(max),
            discharge: self.discharge.min(max),
        };
        Clamped {
            was_clamped: value != self,
            value,
        }
    }
}

impl std::ops::AddAssign for Split {
    fn add_assign(&mut self, rhs: Self) {
        self.charge += rhs.charge;
        self.discharge += rhs.discharge;
    }
}

/// One device's worth of integrated-but-not-yet-exported deltas.
///
/// Every field here obeys the same rule as `pending_ah` did on its own: it is
/// added to a monotonic counter only after a state file containing the result
/// has been fsynced.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Pending {
    ah: Split,
    wh: Split,
    cal_ah: Split,
    cal_wh: Split,
    /// Balancer bleed is a magnitude, not a direction, so it needs no `Split`.
    balance_ah: f64,
}

impl std::ops::AddAssign for Pending {
    fn add_assign(&mut self, rhs: Self) {
        self.ah += rhs.ah;
        self.wh += rhs.wh;
        self.cal_ah += rhs.cal_ah;
        self.cal_wh += rhs.cal_wh;
        self.balance_ah += rhs.balance_ah;
    }
}

/// Per-device record of which cell/sensor series currently exist, so we can
/// remove series that disappear (e.g. cell count shrinks or device goes away).
#[derive(Default)]
struct LastSeries {
    cells: Vec<u32>,
    sensors: Vec<u32>,
    /// Coulomb/energy-counter state: time, pack current and pack voltage of the
    /// previous frame (voltage is used to integrate power = V*I into watt-hours).
    last_coulomb_ts: Option<f64>,
    last_current: Option<f64>,
    last_pack_v: Option<f64>,
    /// Last label-tuple written to `device_info`, so the previous (possibly
    /// stale) series can be removed before a differing tuple is set. Guards
    /// against unbounded series growth from a device varying its identity
    /// strings (`daly_bms_device_info` cardinality bomb).
    last_device_info: Option<[String; 5]>,
    /// Real pack serial decoded from the realtime block (register data), used to
    /// populate the `serial` label of `daly_bms_device_info`. `None` until a
    /// realtime frame carrying a serial has been seen.
    realtime_serial: Option<String>,
    /// Balancer bleed of the previous frame, for the same trapezoid.
    last_balance_a: Option<f64>,
    /// Calibration offset in force when the previous frame was integrated. The
    /// calibrated trapezoid corrects each endpoint with the offset that applied
    /// *at that endpoint*, so a moving offset never retroactively rewrites the
    /// interval before it changed.
    last_offset: Option<f64>,
    /// Peers whose `{sn, peer}` gauges currently exist, so a peer that stops
    /// qualifying has its series removed instead of going stale. Same
    /// cardinality guard as `last_device_info`.
    last_peers: Vec<String>,
    /// Deltas already integrated but NOT yet applied to the exported counters.
    /// They are applied only once the state file holding them has been durably
    /// written, so the file can never be behind what /metrics has served.
    pending: Pending,
}

impl LastSeries {
    /// Whether this device has deltas waiting for a durable write.
    fn has_pending(&self) -> bool {
        self.pending != Pending::default()
    }
}

/// Everything behind the single coulomb mutex. Keeping the throttle
/// timestamp inside the same guard (rather than in a second mutex) makes the
/// "always locked together" rule structural instead of a comment.
struct CoulombShared {
    devices: HashMap<String, LastSeries>,
    /// Current-sensor self-calibration. Behind the same mutex as the counters
    /// so a frame's offset and the delta it produces can never interleave with
    /// another frame's.
    calibrator: Calibrator,
    /// Monotonic time of the last successful state write. `Instant`, not wall
    /// clock, so a system-time jump cannot stall or spam the writes.
    last_persist_at: Option<Instant>,
    /// Set while a write is in flight (the lock is released across the fsync).
    /// Guards against two writers applying overlapping pending snapshots and
    /// double-counting them into the monotonic counters.
    writing: bool,
}

impl CoulombShared {
    fn new(opts: calibration::Options) -> Self {
        Self {
            devices: HashMap::new(),
            calibrator: Calibrator::new(opts),
            last_persist_at: None,
            writing: false,
        }
    }
}

/// Seconds since the Unix epoch as a float.
pub fn now_unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// All exporter metrics plus the registry that renders them.
pub struct Metrics {
    registry: Registry,

    // Realtime scalars {sn}.
    pack_voltage: GaugeVec,
    current: GaugeVec,
    soc: GaugeVec,
    remaining_capacity: GaugeVec,
    cell_voltage: GaugeVec,     // {sn, cell}
    cell_voltage_max: GaugeVec, // {sn}
    cell_voltage_min: GaugeVec,
    cell_voltage_avg: GaugeVec,
    cell_voltage_delta: GaugeVec,
    temperature: GaugeVec, // {sn, sensor}
    temperature_max: GaugeVec,
    temperature_min: GaugeVec,
    mos_temperature: GaugeVec,
    charge_cycles: IntGaugeVec,
    charge_mos: IntGaugeVec,
    discharge_mos: IntGaugeVec,
    balancer_active: IntGaugeVec,
    balance_current: GaugeVec,
    balancing_cell_count: IntGaugeVec,
    alarm_bits: IntGaugeVec,
    alarm: IntGaugeVec, // {sn, type}

    // Config {sn} and thresholds {sn, level}.
    rated_capacity: GaugeVec,
    cell_reference_voltage: GaugeVec,
    cell_voltage_high_limit: GaugeVec,
    cell_voltage_low_limit: GaugeVec,
    pack_voltage_high_limit: GaugeVec,
    pack_voltage_low_limit: GaugeVec,
    charge_overcurrent_limit: GaugeVec,
    discharge_overcurrent_limit: GaugeVec,
    charge_temp_high_limit: GaugeVec,
    charge_temp_low_limit: GaugeVec,
    discharge_temp_high_limit: GaugeVec,
    diff_temp_limit: GaugeVec,
    fan_on_temperature: GaugeVec,
    balance_enable: IntGaugeVec,
    config_balance_current: GaugeVec,
    device_info: IntGaugeVec, // {sn, serial, machine_code, sw_version, hw_version}

    // Coulomb counter: cumulative charge/discharge in amp-hours {sn}.
    charge_amp_hours: CounterVec,
    discharge_amp_hours: CounterVec,
    // Energy counter: cumulative charge/discharge in watt-hours (integral of
    // measured power V*I) {sn}.
    charge_watt_hours: CounterVec,
    discharge_watt_hours: CounterVec,
    // The same four integrals recomputed from the self-calibrated current, plus
    // the balancer bleed that explains most of what is left over {sn}.
    cal_charge_amp_hours: CounterVec,
    cal_discharge_amp_hours: CounterVec,
    cal_charge_watt_hours: CounterVec,
    cal_discharge_watt_hours: CounterVec,
    balance_amp_hours: CounterVec,
    // Calibration observability {sn}.
    current_offset: GaugeVec,
    current_offset_estimate: GaugeVec,
    calibration_anchor_error: GaugeVec,
    calibration_span_hours: GaugeVec,
    calibration_fit_r2: GaugeVec,
    calibration_hold: GaugeVec,
    // Peer cross-check {sn, peer}.
    calibration_peer_offset: GaugeVec,
    calibration_peer_slope: GaugeVec,
    calibration_peer_r2: GaugeVec,
    calibration_peer_disagreement: GaugeVec,

    // Exporter self-observability.
    http_requests: IntCounterVec,   // {endpoint, status}
    frames_decoded: IntCounterVec,  // {block}
    frames_dropped: IntCounterVec,  // {reason}
    last_frame_timestamp: GaugeVec, // {sn}
    /// Failed durable writes of the coulomb state file, by stage.
    state_write_errors: IntCounterVec, // {stage}
    /// Unix time at which the persisted state was last known to match the
    /// counters — either a successful write, or a check that found nothing to
    /// write. Exported so a stalled accounting loop is detectable even when
    /// nothing errors; it must not age while the exporter is merely idle.
    state_last_write_timestamp: Gauge,

    /// Integration-interval cap for the coulomb counter (seconds).
    coulomb_max_gap_secs: f64,
    /// Hard cap on distinct tracked serials (`0` = unlimited).
    max_devices: usize,
    /// If set, coulomb totals are persisted here and restored on startup.
    coulomb_state_path: Option<PathBuf>,
    /// Minimum interval between durable state writes.
    state_min_interval: Duration,
    /// Per-frame caps on the integrated delta; see [`MetricsOptions`].
    max_frame_amp_hours: f64,
    max_frame_watt_hours: f64,
    /// When false the calibrator never runs: no offset, no calibrated counters,
    /// no calibration gauges. The escape hatch if the estimator misbehaves.
    calibration_enabled: bool,
    /// Realtime samples whose delta was clamped at integration, by reason.
    coulomb_samples_rejected: IntCounterVec, // {reason}
    coulombs: Mutex<CoulombShared>,
}

/// Construction-time knobs for [`Metrics`], mirroring the corresponding
/// [`crate::config::Config`] fields (see `impl From<&Config> for MetricsOptions`).
#[derive(Debug, Clone)]
pub struct MetricsOptions {
    /// Integration-interval cap for the coulomb counter (seconds).
    pub coulomb_max_gap_secs: f64,
    /// Hard cap on distinct tracked serials (`0` = unlimited).
    pub max_devices: usize,
    /// If set, coulomb/energy totals are persisted here and restored on startup.
    pub coulomb_state_path: Option<PathBuf>,
    /// Minimum interval between durable writes of the state file. Deltas
    /// arriving inside the window are held in memory, not lost.
    pub state_min_interval: Duration,
    /// Per-frame cap on what one frame may add to the counters. The
    /// plausibility gate itself is not a metrics concern: it lives in the
    /// ingest handler (`server.rs`) and drops the frame before it gets here.
    pub max_frame_amp_hours: f64,
    pub max_frame_watt_hours: f64,
    /// Run the current-sensor self-calibration.
    pub calibration_enabled: bool,
    /// Estimator tunables; see [`calibration::Options`].
    pub calibration: calibration::Options,
}

impl Default for MetricsOptions {
    fn default() -> Self {
        Self {
            coulomb_max_gap_secs: 900.0,
            max_devices: 64,
            coulomb_state_path: None,
            state_min_interval: Duration::from_secs(5),
            max_frame_amp_hours: 25.0,
            max_frame_watt_hours: 900.0,
            calibration_enabled: true,
            calibration: calibration::Options::default(),
        }
    }
}

/// Register a `GaugeVec` on the registry (name collisions are a startup bug).
fn register_gauge_vec(reg: &Registry, name: &str, help: &str, labels: &[&str]) -> GaugeVec {
    let m = GaugeVec::new(Opts::new(name, help), labels).expect("valid metric");
    reg.register(Box::new(m.clone())).expect("unique metric");
    m
}

fn register_gauge(reg: &Registry, name: &str, help: &str) -> Gauge {
    let m = Gauge::with_opts(Opts::new(name, help)).expect("valid metric");
    reg.register(Box::new(m.clone())).expect("unique metric");
    m
}

fn register_int_gauge_vec(reg: &Registry, name: &str, help: &str, labels: &[&str]) -> IntGaugeVec {
    let m = IntGaugeVec::new(Opts::new(name, help), labels).expect("valid metric");
    reg.register(Box::new(m.clone())).expect("unique metric");
    m
}

fn register_int_counter_vec(
    reg: &Registry,
    name: &str,
    help: &str,
    labels: &[&str],
) -> IntCounterVec {
    let m = IntCounterVec::new(Opts::new(name, help), labels).expect("valid metric");
    reg.register(Box::new(m.clone())).expect("unique metric");
    m
}

fn register_counter_vec(reg: &Registry, name: &str, help: &str, labels: &[&str]) -> CounterVec {
    let m = CounterVec::new(Opts::new(name, help), labels).expect("valid metric");
    reg.register(Box::new(m.clone())).expect("unique metric");
    m
}

impl Metrics {
    /// Construct the metric registry and all metric families. See
    /// [`MetricsOptions`] for the knobs; when `coulomb_state_path` is set, call
    /// [`Metrics::restore_coulombs`] right after construction.
    ///
    /// # Panics
    ///
    /// Panics only at startup if a metric name is duplicate or invalid (a
    /// programming bug).
    #[expect(clippy::too_many_lines)]
    pub fn new(opts: MetricsOptions) -> Self {
        let MetricsOptions {
            coulomb_max_gap_secs,
            max_devices,
            coulomb_state_path,
            state_min_interval,
            max_frame_amp_hours,
            max_frame_watt_hours,
            calibration_enabled,
            calibration,
        } = opts;
        let r = Registry::new();
        let m = Self {
            pack_voltage: register_gauge_vec(
                &r,
                "daly_bms_pack_voltage_volts",
                "Pack voltage",
                &["sn"],
            ),
            current: register_gauge_vec(
                &r,
                "daly_bms_current_amperes",
                "Pack current (positive = charge)",
                &["sn"],
            ),
            soc: register_gauge_vec(&r, "daly_bms_soc_percent", "State of charge", &["sn"]),
            remaining_capacity: register_gauge_vec(
                &r,
                "daly_bms_remaining_capacity_amp_hours",
                "Remaining capacity",
                &["sn"],
            ),
            cell_voltage: register_gauge_vec(
                &r,
                "daly_bms_cell_voltage_volts",
                "Per-cell voltage",
                &["sn", "cell"],
            ),
            cell_voltage_max: register_gauge_vec(
                &r,
                "daly_bms_cell_voltage_max_volts",
                "Max cell voltage",
                &["sn"],
            ),
            cell_voltage_min: register_gauge_vec(
                &r,
                "daly_bms_cell_voltage_min_volts",
                "Min cell voltage",
                &["sn"],
            ),
            cell_voltage_avg: register_gauge_vec(
                &r,
                "daly_bms_cell_voltage_avg_volts",
                "Average cell voltage",
                &["sn"],
            ),
            cell_voltage_delta: register_gauge_vec(
                &r,
                "daly_bms_cell_voltage_delta_volts",
                "Cell voltage spread (max-min)",
                &["sn"],
            ),
            temperature: register_gauge_vec(
                &r,
                "daly_bms_temperature_celsius",
                "External temperature sensor",
                &["sn", "sensor"],
            ),
            temperature_max: register_gauge_vec(
                &r,
                "daly_bms_temperature_max_celsius",
                "Max temperature",
                &["sn"],
            ),
            temperature_min: register_gauge_vec(
                &r,
                "daly_bms_temperature_min_celsius",
                "Min temperature",
                &["sn"],
            ),
            mos_temperature: register_gauge_vec(
                &r,
                "daly_bms_mos_temperature_celsius",
                "MOSFET temperature",
                &["sn"],
            ),
            charge_cycles: register_int_gauge_vec(
                &r,
                "daly_bms_charge_cycles",
                "Charge cycle count (absolute reading from the device)",
                &["sn"],
            ),
            charge_mos: register_int_gauge_vec(
                &r,
                "daly_bms_charge_mos",
                "Charge MOSFET state (1 = on)",
                &["sn"],
            ),
            discharge_mos: register_int_gauge_vec(
                &r,
                "daly_bms_discharge_mos",
                "Discharge MOSFET state (1 = on)",
                &["sn"],
            ),
            balancer_active: register_int_gauge_vec(
                &r,
                "daly_bms_balancer_active",
                "Balancer running (1 = yes)",
                &["sn"],
            ),
            balance_current: register_gauge_vec(
                &r,
                "daly_bms_balance_current_amperes",
                "Active-balancer current",
                &["sn"],
            ),
            balancing_cell_count: register_int_gauge_vec(
                &r,
                "daly_bms_balancing_cell_count",
                "Number of cells being balanced",
                &["sn"],
            ),
            alarm_bits: register_int_gauge_vec(
                &r,
                "daly_bms_alarm_bits",
                "Raw alarm bitmask (register 0x3B)",
                &["sn"],
            ),
            alarm: register_int_gauge_vec(
                &r,
                "daly_bms_alarm",
                "Decoded alarm flag (1 = active)",
                &["sn", "type"],
            ),
            rated_capacity: register_gauge_vec(
                &r,
                "daly_bms_rated_capacity_amp_hours",
                "Rated capacity",
                &["sn"],
            ),
            cell_reference_voltage: register_gauge_vec(
                &r,
                "daly_bms_cell_reference_voltage_volts",
                "Cell reference voltage",
                &["sn"],
            ),
            cell_voltage_high_limit: register_gauge_vec(
                &r,
                "daly_bms_cell_voltage_high_limit_volts",
                "Cell over-voltage threshold",
                &["sn", "level"],
            ),
            cell_voltage_low_limit: register_gauge_vec(
                &r,
                "daly_bms_cell_voltage_low_limit_volts",
                "Cell under-voltage threshold",
                &["sn", "level"],
            ),
            pack_voltage_high_limit: register_gauge_vec(
                &r,
                "daly_bms_pack_voltage_high_limit_volts",
                "Pack over-voltage threshold",
                &["sn", "level"],
            ),
            pack_voltage_low_limit: register_gauge_vec(
                &r,
                "daly_bms_pack_voltage_low_limit_volts",
                "Pack under-voltage threshold",
                &["sn", "level"],
            ),
            charge_overcurrent_limit: register_gauge_vec(
                &r,
                "daly_bms_charge_overcurrent_limit_amperes",
                "Charge over-current threshold",
                &["sn", "level"],
            ),
            discharge_overcurrent_limit: register_gauge_vec(
                &r,
                "daly_bms_discharge_overcurrent_limit_amperes",
                "Discharge over-current threshold",
                &["sn", "level"],
            ),
            charge_temp_high_limit: register_gauge_vec(
                &r,
                "daly_bms_charge_temp_high_limit_celsius",
                "Charge high-temperature threshold",
                &["sn", "level"],
            ),
            charge_temp_low_limit: register_gauge_vec(
                &r,
                "daly_bms_charge_temp_low_limit_celsius",
                "Charge low-temperature threshold",
                &["sn", "level"],
            ),
            discharge_temp_high_limit: register_gauge_vec(
                &r,
                "daly_bms_discharge_temp_high_limit_celsius",
                "Discharge high-temperature threshold",
                &["sn", "level"],
            ),
            diff_temp_limit: register_gauge_vec(
                &r,
                "daly_bms_diff_temp_limit_celsius",
                "Differential-temperature protection",
                &["sn"],
            ),
            fan_on_temperature: register_gauge_vec(
                &r,
                "daly_bms_fan_on_temperature_celsius",
                "Fan-on temperature",
                &["sn"],
            ),
            balance_enable: register_int_gauge_vec(
                &r,
                "daly_bms_balance_enable",
                "Balancing enabled (1 = yes)",
                &["sn"],
            ),
            config_balance_current: register_gauge_vec(
                &r,
                "daly_bms_config_balance_current_amperes",
                "Configured active-balancer current",
                &["sn"],
            ),
            device_info: register_int_gauge_vec(
                &r,
                "daly_bms_device_info",
                "Device identity (always 1)",
                &["sn", "serial", "machine_code", "sw_version", "hw_version"],
            ),
            http_requests: register_int_counter_vec(
                &r,
                "daly_bms_http_requests_total",
                "HTTP requests handled",
                &["endpoint", "status"],
            ),
            frames_decoded: register_int_counter_vec(
                &r,
                "daly_bms_frames_decoded_total",
                "Modbus frames decoded",
                &["block"],
            ),
            frames_dropped: register_int_counter_vec(
                &r,
                "daly_bms_frames_dropped_total",
                "Frames dropped",
                &["reason"],
            ),
            last_frame_timestamp: register_gauge_vec(
                &r,
                "daly_bms_last_frame_timestamp_seconds",
                "Unix time of the last accepted frame",
                &["sn"],
            ),
            coulomb_samples_rejected: register_int_counter_vec(
                &r,
                "daly_bms_coulomb_samples_rejected_total",
                "Realtime samples rejected, or their delta clamped, at coulomb/energy integration",
                &["reason"],
            ),
            state_write_errors: register_int_counter_vec(
                &r,
                "daly_bms_state_write_errors_total",
                "Failed durable writes of the coulomb/energy state file",
                &["stage"],
            ),
            state_last_write_timestamp: register_gauge(
                &r,
                "daly_bms_state_last_write_timestamp_seconds",
                "Unix time at which the persisted coulomb/energy state was last known current",
            ),
            charge_amp_hours: register_counter_vec(
                &r,
                "daly_bms_charge_amp_hours_total",
                "Cumulative charge throughput (coulomb-counted)",
                &["sn"],
            ),
            discharge_amp_hours: register_counter_vec(
                &r,
                "daly_bms_discharge_amp_hours_total",
                "Cumulative discharge throughput (coulomb-counted)",
                &["sn"],
            ),
            charge_watt_hours: register_counter_vec(
                &r,
                "daly_bms_charge_watt_hours_total",
                "Cumulative charge energy in watt-hours (integral of measured V*I)",
                &["sn"],
            ),
            cal_charge_amp_hours: register_counter_vec(
                &r,
                "daly_bms_calibrated_charge_amp_hours_total",
                "Cumulative charge throughput, self-calibrated current",
                &["sn"],
            ),
            cal_discharge_amp_hours: register_counter_vec(
                &r,
                "daly_bms_calibrated_discharge_amp_hours_total",
                "Cumulative discharge throughput, self-calibrated current",
                &["sn"],
            ),
            cal_charge_watt_hours: register_counter_vec(
                &r,
                "daly_bms_calibrated_charge_watt_hours_total",
                "Cumulative charge energy, self-calibrated current",
                &["sn"],
            ),
            cal_discharge_watt_hours: register_counter_vec(
                &r,
                "daly_bms_calibrated_discharge_watt_hours_total",
                "Cumulative discharge energy, self-calibrated current",
                &["sn"],
            ),
            balance_amp_hours: register_counter_vec(
                &r,
                "daly_bms_balance_amp_hours_total",
                "Cumulative charge bled off by the passive balancer",
                &["sn"],
            ),
            current_offset: register_gauge_vec(
                &r,
                "daly_bms_current_offset_amperes",
                "Offset currently subtracted from the reported pack current",
                &["sn"],
            ),
            current_offset_estimate: register_gauge_vec(
                &r,
                "daly_bms_current_offset_estimate_amperes",
                "Latest offset estimate, whether or not it is being applied",
                &["sn"],
            ),
            calibration_anchor_error: register_gauge_vec(
                &r,
                "daly_bms_calibration_anchor_error_amperes",
                "Worst-case error the bounded state of charge can induce in the estimate",
                &["sn"],
            ),
            calibration_span_hours: register_gauge_vec(
                &r,
                "daly_bms_calibration_span_hours",
                "Integrated hours behind the estimate",
                &["sn"],
            ),
            calibration_fit_r2: register_gauge_vec(
                &r,
                "daly_bms_calibration_fit_r2",
                "Goodness of fit of the closure regression",
                &["sn"],
            ),
            calibration_hold: register_gauge_vec(
                &r,
                "daly_bms_calibration_hold",
                "Why the estimate is not applied (0 applied, 1 warmup, 2 no_fit, 3 noisy, 4 peer_disagreement)",
                &["sn"],
            ),
            calibration_peer_offset: register_gauge_vec(
                &r,
                "daly_bms_calibration_peer_offset_amperes",
                "Offset relative to a parallel peer, measured by regressing the two currents",
                &["sn", "peer"],
            ),
            calibration_peer_slope: register_gauge_vec(
                &r,
                "daly_bms_calibration_peer_slope",
                "Slope of that peer regression (1.0 = the two packs share current equally)",
                &["sn", "peer"],
            ),
            calibration_peer_r2: register_gauge_vec(
                &r,
                "daly_bms_calibration_peer_r2",
                "Goodness of fit of that peer regression",
                &["sn", "peer"],
            ),
            calibration_peer_disagreement: register_gauge_vec(
                &r,
                "daly_bms_calibration_peer_disagreement_amperes",
                "Gap between the peer-measured and closure-derived offset difference",
                &["sn", "peer"],
            ),
            discharge_watt_hours: register_counter_vec(
                &r,
                "daly_bms_discharge_watt_hours_total",
                "Cumulative discharge energy in watt-hours (integral of measured V*I)",
                &["sn"],
            ),
            coulomb_max_gap_secs,
            max_devices,
            coulomb_state_path,
            state_min_interval,
            max_frame_amp_hours,
            max_frame_watt_hours,
            calibration_enabled,
            registry: r,
            coulombs: Mutex::new(CoulombShared::new(calibration)),
        };
        // Pre-create every known reason series at zero. A counter series that
        // only appears on its first increment gives `increase()` nothing to
        // compare against, so the very first drop — the one that matters — is
        // invisible to the alert rule. This is what happened with the
        // production 436.6 A frame: the sample was rejected, the series was
        // born at 1, and RatzekBMSImplausibleSamples never fired.
        for reason in DROP_REASONS {
            m.frames_dropped.with_label_values(&[reason]);
        }
        m.coulomb_samples_rejected
            .with_label_values(&[REJECT_CLAMPED_DELTA]);
        m
    }

    /// Whether a telemetry frame for `sn` should be processed. Returns `true`
    /// for an already-tracked device or while under the `max_devices` cap;
    /// `false` once the cap is reached for a new serial (bounds cardinality).
    pub fn admit(&self, sn: &str) -> bool {
        if self.max_devices == 0 {
            return true;
        }
        let mut coulombs = self.coulombs.lock().unwrap_or_else(PoisonError::into_inner);
        if coulombs.devices.contains_key(sn) {
            return true;
        }
        if coulombs.devices.len() < self.max_devices {
            // Reserve the slot now, so that even frames which never decode (and
            // thus never reach `update_*`) still count against the cap — e.g.
            // `mark_seen` mints an `sn`-labelled series unconditionally.
            coulombs
                .devices
                .insert(sn.to_string(), LastSeries::default());
            true
        } else {
            false
        }
    }

    /// Apply a decoded realtime frame, pruning stale per-cell/sensor series.
    pub fn update_realtime(&self, sn: &str, d: &RealtimeData) {
        {
            let mut coulombs = self.coulombs.lock().unwrap_or_else(PoisonError::into_inner);
            // Avoid allocating a String key on every frame: only insert when the
            // device is new, then take a mutable borrow of the existing entry.
            if !coulombs.devices.contains_key(sn) {
                if self.max_devices != 0 && coulombs.devices.len() >= self.max_devices {
                    return; // cap reached; do not create a new device entry
                }
                coulombs
                    .devices
                    .insert(sn.to_string(), LastSeries::default());
            }
            let entry = coulombs
                .devices
                .get_mut(sn)
                .expect("just inserted or present");

            entry.realtime_serial = d.serial.clone();
            sync_indexed(&self.cell_voltage, sn, &mut entry.cells, &d.cells_v);
            sync_indexed(&self.temperature, sn, &mut entry.sensors, &d.temps_c);
        }

        set_gauge(&self.pack_voltage, sn, d.pack_v);
        set_gauge(&self.current, sn, d.current_a);
        set_gauge(&self.soc, sn, d.soc_pct);
        set_gauge(&self.remaining_capacity, sn, d.remaining_ah);
        set_gauge(&self.cell_voltage_max, sn, d.cell_max_v);
        set_gauge(&self.cell_voltage_min, sn, d.cell_min_v);
        set_gauge(&self.cell_voltage_avg, sn, d.cell_avg_v);
        set_gauge(&self.cell_voltage_delta, sn, d.cell_delta_v);
        set_gauge(&self.temperature_max, sn, d.temp_max_c);
        set_gauge(&self.temperature_min, sn, d.temp_min_c);
        set_gauge(&self.mos_temperature, sn, d.mos_temp_c);
        set_gauge(&self.balance_current, sn, d.balance_current_a);

        set_int_gauge(&self.charge_cycles, sn, d.cycles.map(i64::from));
        set_int_gauge(
            &self.balancing_cell_count,
            sn,
            d.balancing_cells.map(i64::from),
        );
        set_int_gauge(&self.charge_mos, sn, d.charge_mos.map(i64::from));
        set_int_gauge(&self.discharge_mos, sn, d.discharge_mos.map(i64::from));
        set_int_gauge(&self.balancer_active, sn, d.balancer_active.map(i64::from));

        if let Some(bits) = d.alarm_bits {
            self.alarm_bits
                .with_label_values(&[sn])
                .set(i64::from(bits));
            for &(mask, name) in ALARM_FLAGS {
                self.alarm
                    .with_label_values(&[sn, name])
                    .set(i64::from(bits & mask != 0));
            }
        }
    }

    /// Apply a decoded config frame.
    pub fn update_config(&self, sn: &str, c: &ConfigData) {
        set_gauge(&self.rated_capacity, sn, c.rated_capacity_ah);
        set_gauge(&self.cell_reference_voltage, sn, c.cell_reference_v);
        set_limit(&self.cell_voltage_high_limit, sn, c.cell_high_v);
        set_limit(&self.cell_voltage_low_limit, sn, c.cell_low_v);
        set_limit(&self.pack_voltage_high_limit, sn, c.pack_high_v);
        set_limit(&self.pack_voltage_low_limit, sn, c.pack_low_v);
        set_limit(&self.charge_overcurrent_limit, sn, c.charge_overcurrent_a);
        set_limit(
            &self.discharge_overcurrent_limit,
            sn,
            c.discharge_overcurrent_a,
        );
        set_limit(&self.charge_temp_high_limit, sn, c.charge_temp_high_c);
        set_limit(&self.charge_temp_low_limit, sn, c.charge_temp_low_c);
        set_limit(&self.discharge_temp_high_limit, sn, c.discharge_temp_high_c);
        set_gauge(&self.diff_temp_limit, sn, c.diff_temp_c);
        set_gauge(&self.fan_on_temperature, sn, c.fan_on_temp_c);
        set_int_gauge(&self.balance_enable, sn, c.balance_enable.map(i64::from));
        set_gauge(&self.config_balance_current, sn, c.balance_current_a);

        // Info metric: fill every label (empty string when a field is absent).
        // The label-tuple is built from untrusted decoded strings; a device that
        // varies its identity would otherwise mint unbounded series, so we drop
        // the previous tuple whenever it differs from the new one. The whole
        // build+prune+set runs under the coulomb lock so the metric mutation is
        // serialized with the tracked-identity state.
        let mut coulombs = self.coulombs.lock().unwrap_or_else(PoisonError::into_inner);
        if !coulombs.devices.contains_key(sn) {
            if self.max_devices != 0 && coulombs.devices.len() >= self.max_devices {
                return; // cap reached; do not create a new device entry
            }
            coulombs
                .devices
                .insert(sn.to_string(), LastSeries::default());
        }
        let entry = coulombs
            .devices
            .get_mut(sn)
            .expect("just inserted or present");

        // Surface the real decoded pack serial when a realtime frame has
        // supplied one; otherwise fall back to the transport-level `sn`.
        let serial = entry
            .realtime_serial
            .clone()
            .unwrap_or_else(|| sn.to_string());
        let labels = [
            sn.to_string(),
            serial,
            c.machine_code.clone().unwrap_or_default(),
            c.sw_version.clone().unwrap_or_default(),
            c.hw_version.clone().unwrap_or_default(),
        ];
        self.set_device_info(entry, labels);
    }

    /// Prune the previous `device_info` label-tuple (if it changed) and set the
    /// current one to `1`, recording it on `entry`. Must be called while holding
    /// the coulomb lock so the metric mutation stays serialized with `entry`.
    fn set_device_info(&self, entry: &mut LastSeries, labels: [String; 5]) {
        let refs = labels.each_ref().map(String::as_str);
        if let Some(old) = &entry.last_device_info
            && *old != labels
        {
            let old_refs = old.each_ref().map(String::as_str);
            let _ = self.device_info.remove_label_values(&old_refs);
        }
        self.device_info.with_label_values(&refs).set(1);
        entry.last_device_info = Some(labels);
    }

    /// Stamp the last-frame timestamp for a device.
    pub fn mark_seen(&self, sn: &str) {
        self.last_frame_timestamp
            .with_label_values(&[sn])
            .set(now_unix_secs());
    }

    /// Integrate pack current into the cumulative charge/discharge counters
    /// (coulomb counting, amp-hours) and, when a pack voltage is present, integrate
    /// measured power `V*I` into the energy counters (watt-hours). Call once per
    /// accepted realtime frame with the wall-clock time; trapezoidal over the
    /// interval since the previous frame.
    ///
    /// Energy = current × voltage, so an anomalous frame inflates the monotonic
    /// Wh counter more than the Ah one. Hence three bounds: the plausibility
    /// gate on the reading (applied upstream by the ingest handler, which drops
    /// the frame before it reaches here), `coulomb_max_gap_secs` on the
    /// interval, and `max_frame_amp_hours`/`max_frame_watt_hours` on the
    /// resulting delta. Callers must not pass a frame that failed the gate:
    /// this function trusts its input.
    ///
    /// The integrated deltas are not applied to the exported counters here; they
    /// are staged as `pending` and applied by [`Metrics::flush_coulomb_state`]
    /// once the state file containing them has been durably written.
    pub fn accumulate_coulombs(&self, sn: &str, d: &RealtimeData, now_secs: f64) {
        let Some(cur) = d.current_a else { return };
        let pack_v = d.pack_v;
        // `pack_v == None` is legal: a frame without pack voltage still counts
        // amp-hours, it just cannot contribute energy.
        let mut clamped = false;
        {
            let mut coulombs = self.coulombs.lock().unwrap_or_else(PoisonError::into_inner);
            if !coulombs.devices.contains_key(sn) {
                if self.max_devices != 0 && coulombs.devices.len() >= self.max_devices {
                    return; // cap reached; do not create a new device entry
                }
                coulombs
                    .devices
                    .insert(sn.to_string(), LastSeries::default());
            }
            let gap = self.coulomb_max_gap_secs;

            // Calibration runs first and on the *raw* reading: its estimator's
            // whole input is the uncorrected current, and the offset it returns
            // applies to this very frame. Both borrows of `coulombs` below touch
            // disjoint fields, so they are taken one after the other.
            let (offset, report) = if self.calibration_enabled {
                let offset = coulombs.calibrator.observe(
                    sn,
                    Sample {
                        current_a: cur,
                        balance_a: d.balance_current_a,
                        remaining_ah: d.remaining_ah,
                        capacity_ah: self.rated_capacity_hint(sn),
                        now_secs,
                    },
                );
                (offset, coulombs.calibrator.report(sn))
            } else {
                (0.0, None)
            };

            let entry = coulombs
                .devices
                .get_mut(sn)
                .expect("just inserted or present");
            // Single block (avoids a nested `if let` → clippy::collapsible_if): integrate
            // Ah from current, and Wh from power when both endpoint voltages are known.
            if let (Some(last_ts), Some(last_cur)) = (entry.last_coulomb_ts, entry.last_current) {
                let dt = now_secs - last_ts;
                let ah = trapezoid_hours(last_cur, cur, dt, gap).clamped(self.max_frame_amp_hours);
                entry.pending.ah += ah.value;
                clamped |= ah.was_clamped;
                // The calibrated pair corrects each endpoint with the offset that
                // was in force at that endpoint, so an offset that moves between
                // frames never rewrites the interval before it moved.
                let cal_prev = last_cur - entry.last_offset.unwrap_or(0.0);
                let cal_cur = cur - offset;
                let cal_ah =
                    trapezoid_hours(cal_prev, cal_cur, dt, gap).clamped(self.max_frame_amp_hours);
                entry.pending.cal_ah += cal_ah.value;
                clamped |= cal_ah.was_clamped;
                if let (Some(last_pv), Some(pv)) = (entry.last_pack_v, pack_v) {
                    // Power = V*I; trapezoid of power over the interval → watt-hours.
                    let wh = trapezoid_hours(last_cur * last_pv, cur * pv, dt, gap)
                        .clamped(self.max_frame_watt_hours);
                    entry.pending.wh += wh.value;
                    clamped |= wh.was_clamped;
                    let cal_wh = trapezoid_hours(cal_prev * last_pv, cal_cur * pv, dt, gap)
                        .clamped(self.max_frame_watt_hours);
                    entry.pending.cal_wh += cal_wh.value;
                    clamped |= cal_wh.was_clamped;
                }
                if let (Some(last_bal), Some(bal)) = (entry.last_balance_a, d.balance_current_a) {
                    // The bleed is a magnitude: a negative reading is a decode
                    // artifact, not a balancer running backwards. Only the
                    // `charge` side can be non-zero once both ends are clamped
                    // at zero, so the `discharge` side is discarded.
                    let bleed = trapezoid_hours(last_bal.max(0.0), bal.max(0.0), dt, gap)
                        .clamped(self.max_frame_amp_hours);
                    entry.pending.balance_ah += bleed.value.charge;
                    clamped |= bleed.was_clamped;
                }
            }
            entry.last_coulomb_ts = Some(now_secs);
            entry.last_current = Some(cur);
            // Only updated on frames that carry current (early return on None above).
            entry.last_pack_v = pack_v;
            entry.last_balance_a = d.balance_current_a;
            entry.last_offset = Some(offset);

            if let Some(report) = report {
                // Under the lock, like every other metric mutation here, so the
                // published numbers cannot interleave with another frame's.
                let stale_peers = std::mem::replace(
                    &mut entry.last_peers,
                    report.peers.iter().map(|p| p.peer_sn.clone()).collect(),
                );
                self.publish_calibration(sn, &report, &stale_peers);
            }
        }
        if clamped {
            self.record_sample_rejected(REJECT_CLAMPED_DELTA);
        }
        self.flush_coulomb_state(false);
    }

    /// Rated capacity previously decoded from a config frame, if any.
    ///
    /// Read back out of the gauge rather than duplicated into a field: the gauge
    /// *is* where `update_config` stores it, and a second copy could drift from
    /// it. Zero (never set) reads as "unknown", which the calibrator handles by
    /// falling back to the high-water mark of remaining capacity.
    fn rated_capacity_hint(&self, sn: &str) -> Option<f64> {
        let v = self.rated_capacity.with_label_values(&[sn]).get();
        (v > 0.0).then_some(v)
    }

    /// Publish one device's calibration diagnostics, removing the peer-labelled
    /// series of any peer that no longer qualifies.
    fn publish_calibration(&self, sn: &str, r: &Report, stale_peers: &[String]) {
        self.current_offset.with_label_values(&[sn]).set(r.applied);
        self.calibration_hold
            .with_label_values(&[sn])
            .set(r.hold.code());
        self.calibration_span_hours
            .with_label_values(&[sn])
            .set(r.span_hours);
        self.calibration_anchor_error
            .with_label_values(&[sn])
            .set(r.anchor_error);
        if let Some(estimate) = r.estimate {
            self.current_offset_estimate
                .with_label_values(&[sn])
                .set(estimate);
        }
        if let Some(r2) = r.r2 {
            self.calibration_fit_r2.with_label_values(&[sn]).set(r2);
        }

        for stale in stale_peers {
            if r.peers.iter().any(|p| &p.peer_sn == stale) {
                continue;
            }
            for m in [
                &self.calibration_peer_offset,
                &self.calibration_peer_slope,
                &self.calibration_peer_r2,
                &self.calibration_peer_disagreement,
            ] {
                let _ = m.remove_label_values(&[sn, stale]);
            }
        }
        for p in &r.peers {
            let labels = [sn, p.peer_sn.as_str()];
            self.calibration_peer_offset
                .with_label_values(&labels)
                .set(p.relative_offset);
            self.calibration_peer_slope
                .with_label_values(&labels)
                .set(p.slope);
            self.calibration_peer_r2
                .with_label_values(&labels)
                .set(p.r2);
            self.calibration_peer_disagreement
                .with_label_values(&labels)
                .set(p.disagreement);
        }
    }

    /// Restore the persisted counters from `coulomb_state_path` (if configured),
    /// seeding the amp-hour and watt-hour charge/discharge counters so the totals
    /// survive a restart. A missing file is a normal first run; a corrupt file is
    /// logged and ignored. Restored serials still honour the `max_devices` cap, so
    /// a carried-over or hand-edited state file cannot blow past the cardinality
    /// bound. Call once at startup after [`Metrics::new`].
    pub fn restore_coulombs(&self) {
        let Some(path) = &self.coulomb_state_path else {
            return;
        };
        // Seed the freshness gauge with the start time: at this instant the file
        // *is* current. Leaving it at zero would make the staleness alert read
        // "never written since the epoch" for as long as the counters have no
        // delta to persist — which is the normal state of an idle battery.
        self.state_last_write_timestamp.set(now_unix_secs());
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "cannot read coulomb state");
                return;
            }
        };
        let state: CoulombState = match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "corrupt coulomb state; starting fresh");
                return;
            }
        };
        let mut coulombs = self.coulombs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut restored = 0usize;
        for (sn, e) in &state.devices {
            // Honour the same cardinality bound as `admit`/`accumulate_coulombs`:
            // never restore more distinct serials than the cap allows.
            if self.max_devices != 0 && coulombs.devices.len() >= self.max_devices {
                tracing::warn!(sn = ?sn, "restored state exceeds max_devices cap; skipping serial");
                continue;
            }
            inc_if_positive(&self.charge_amp_hours, sn, e.charge_ah);
            inc_if_positive(&self.discharge_amp_hours, sn, e.discharge_ah);
            inc_if_positive(&self.charge_watt_hours, sn, e.charge_wh);
            inc_if_positive(&self.discharge_watt_hours, sn, e.discharge_wh);
            inc_if_positive(&self.cal_charge_amp_hours, sn, e.cal_charge_ah);
            inc_if_positive(&self.cal_discharge_amp_hours, sn, e.cal_discharge_ah);
            inc_if_positive(&self.cal_charge_watt_hours, sn, e.cal_charge_wh);
            inc_if_positive(&self.cal_discharge_watt_hours, sn, e.cal_discharge_wh);
            inc_if_positive(&self.balance_amp_hours, sn, e.balance_ah);
            // Track the serial so future writes keep persisting it.
            coulombs.devices.entry(sn.clone()).or_default();
            restored += 1;
        }
        // The calibrator gets the same cardinality treatment: only serials that
        // actually made it past the cap above are worth restoring an estimate
        // for, and a pair naming a dropped serial is dead weight.
        let admitted: std::collections::HashSet<String> =
            coulombs.devices.keys().cloned().collect();
        coulombs
            .calibrator
            .restore(state.calibration, |sn| admitted.contains(sn));
        tracing::info!(devices = restored, "restored coulomb/energy counters");
    }

    /// Persist the coulomb counters now (e.g. on graceful shutdown), bypassing
    /// the write throttle. No-op if persistence is not configured.
    pub fn persist_coulombs(&self) {
        self.flush_coulomb_state(true);
    }

    /// Durably write the pending coulomb/energy totals and, on success, apply
    /// them to the exported counters.
    ///
    /// The invariant this exists for: a counter is advanced only after a file
    /// already containing that value has been fsynced, so the on-disk value can
    /// never be lower than what `/metrics` has served. A counter that comes back
    /// lower after a restart is read by Prometheus as a reset, which adds the
    /// entire accumulated total to `increase()` — a 20 kWh phantom in practice.
    ///
    /// The file is written with the lock *released*: `main` runs a
    /// `current_thread` runtime and this is called from inside an HTTP handler,
    /// so holding the lock across two fsyncs would stall every other connection,
    /// `/metrics` included. Correctness does not depend on the lock being held —
    /// the pending snapshot is applied only after the write succeeds, and a
    /// concurrent writer is excluded by the `writing` flag.
    fn flush_coulomb_state(&self, force: bool) {
        let Some(path) = self.coulomb_state_path.clone() else {
            // No persistence configured: nothing can be durably written, so the
            // deltas would never be exported. Apply them immediately instead.
            //
            // This must return before any `with_label_values` below, or devices
            // reporting no pack voltage would get an empty watt-hour series.
            let mut coulombs = self.coulombs.lock().unwrap_or_else(PoisonError::into_inner);
            let snapshot = take_pending(&mut coulombs);
            drop(coulombs);
            self.apply_pending(&snapshot);
            return;
        };

        // Phase 1: decide whether to write, and build the payload under the lock.
        let (bytes, snapshot) = {
            let mut coulombs = self.coulombs.lock().unwrap_or_else(PoisonError::into_inner);
            if coulombs.writing {
                return; // another writer is mid-flush; its snapshot covers ours
            }
            if !force {
                if !coulombs.devices.values().any(LastSeries::has_pending) {
                    // Nothing to write: no fsync on idle frames. The file still
                    // matches the counters, so the freshness gauge is stamped —
                    // otherwise a resting battery (current at exactly 0 A
                    // produces no delta) ages the gauge until the staleness
                    // alert fires on a perfectly healthy exporter.
                    drop(coulombs);
                    self.state_last_write_timestamp.set(now_unix_secs());
                    return;
                }
                if let Some(last) = coulombs.last_persist_at
                    && last.elapsed() < self.state_min_interval
                {
                    return; // throttled: the delta stays pending, it is not lost
                }
            }
            let snapshot = take_pending(&mut coulombs);
            let mut state = CoulombState::default();
            state.calibration = coulombs.calibrator.state();
            for (sn, p) in &snapshot {
                state.devices.insert(
                    sn.clone(),
                    CoulombEntry {
                        charge_ah: bump_ulp(
                            self.charge_amp_hours.with_label_values(&[sn]).get() + p.ah.charge,
                        ),
                        discharge_ah: bump_ulp(
                            self.discharge_amp_hours.with_label_values(&[sn]).get()
                                + p.ah.discharge,
                        ),
                        charge_wh: bump_ulp(
                            self.charge_watt_hours.with_label_values(&[sn]).get() + p.wh.charge,
                        ),
                        discharge_wh: bump_ulp(
                            self.discharge_watt_hours.with_label_values(&[sn]).get()
                                + p.wh.discharge,
                        ),
                        cal_charge_ah: bump_ulp(
                            self.cal_charge_amp_hours.with_label_values(&[sn]).get()
                                + p.cal_ah.charge,
                        ),
                        cal_discharge_ah: bump_ulp(
                            self.cal_discharge_amp_hours.with_label_values(&[sn]).get()
                                + p.cal_ah.discharge,
                        ),
                        cal_charge_wh: bump_ulp(
                            self.cal_charge_watt_hours.with_label_values(&[sn]).get()
                                + p.cal_wh.charge,
                        ),
                        cal_discharge_wh: bump_ulp(
                            self.cal_discharge_watt_hours.with_label_values(&[sn]).get()
                                + p.cal_wh.discharge,
                        ),
                        balance_ah: bump_ulp(
                            self.balance_amp_hours.with_label_values(&[sn]).get() + p.balance_ah,
                        ),
                    },
                );
            }
            match serde_json::to_vec(&state) {
                Ok(b) => {
                    coulombs.writing = true;
                    (b, snapshot)
                }
                Err(e) => {
                    self.record_write_error(
                        WriteStage::Serialize,
                        &std::io::Error::other(e),
                        &path,
                    );
                    self.requeue_pending(&mut coulombs, &snapshot);
                    return;
                }
            }
        };

        // Phase 2: the actual fsync, outside the lock.
        //
        // Every stage counts as a failure, the directory fsync included: the
        // filesystems that cannot fsync a directory at all are already filtered
        // out inside `write_file_durable`, so anything surfacing here is a real
        // I/O error — and it means the rename may not survive a reboot, which is
        // precisely the rollback this whole mechanism exists to prevent.
        let written = match write_file_durable(&path, &bytes) {
            Ok(()) => true,
            Err((stage, e)) => {
                self.record_write_error(stage, &e, &path);
                false
            }
        };

        // Phase 3: advance the counters only now that the file holds the value.
        let mut coulombs = self.coulombs.lock().unwrap_or_else(PoisonError::into_inner);
        // Stamp the attempt, not just the success. Throttling on successes alone
        // would leave a broken disk retrying — with two fsyncs — on every single
        // frame, and one unauthenticated POST can carry ~100 of them.
        coulombs.last_persist_at = Some(Instant::now());
        if written {
            // Apply before clearing `writing`: until the counters are raised,
            // a concurrent writer entering phase 1 would read stale counter
            // values and persist a total lower than what /metrics is about to
            // serve — the very inversion this function guards against.
            self.apply_pending(&snapshot);
            self.state_last_write_timestamp.set(now_unix_secs());
        } else {
            // Keep the deltas queued for the next attempt. Applying them anyway
            // would put the counters ahead of the file, which is exactly the
            // phantom-spike condition after the next restart.
            self.requeue_pending(&mut coulombs, &snapshot);
        }
        coulombs.writing = false;
    }

    /// Add a taken pending snapshot onto the exported counters.
    fn apply_pending(&self, snapshot: &[(String, Pending)]) {
        for (sn, p) in snapshot {
            add_split(&self.charge_amp_hours, &self.discharge_amp_hours, sn, p.ah);
            add_split(
                &self.charge_watt_hours,
                &self.discharge_watt_hours,
                sn,
                p.wh,
            );
            add_split(
                &self.cal_charge_amp_hours,
                &self.cal_discharge_amp_hours,
                sn,
                p.cal_ah,
            );
            add_split(
                &self.cal_charge_watt_hours,
                &self.cal_discharge_watt_hours,
                sn,
                p.cal_wh,
            );
            inc_if_positive(&self.balance_amp_hours, sn, p.balance_ah);
        }
    }

    /// Put a taken pending snapshot back after a failed write, so the deltas are
    /// retried rather than lost.
    fn requeue_pending(&self, coulombs: &mut CoulombShared, snapshot: &[(String, Pending)]) {
        for (sn, p) in snapshot {
            if let Some(e) = coulombs.devices.get_mut(sn) {
                e.pending += *p;
            }
        }
    }

    /// Count a failed state write and log it. Kept separate so every failure
    /// path reports the same way.
    fn record_write_error(&self, stage: WriteStage, e: &std::io::Error, path: &Path) {
        self.state_write_errors
            .with_label_values(&[stage.as_str()])
            .inc();
        tracing::warn!(
            error = %e,
            stage = stage.as_str(),
            path = %path.display(),
            "cannot persist coulomb state"
        );
    }

    /// Record the outcome of an HTTP request (endpoint, status).
    pub fn record_request(&self, endpoint: &str, status: u16) {
        self.http_requests
            .with_label_values(&[endpoint, &status.to_string()])
            .inc();
    }

    /// Count a successfully decoded frame by block.
    pub fn record_decoded(&self, block: &str) {
        self.frames_decoded.with_label_values(&[block]).inc();
    }

    /// Count a realtime sample whose integrated delta was clamped.
    ///
    /// Deliberately *not* `frames_dropped`: that family means "the frame never
    /// reached any metric", while a clamped frame is decoded, exported as
    /// gauges and counted in `frames_decoded`. Reusing it would make the drop
    /// ratio panels double-count the same frame. (Implausible frames *are*
    /// dropped whole, by the ingest handler, and do land in `frames_dropped`.)
    fn record_sample_rejected(&self, reason: &str) {
        self.coulomb_samples_rejected
            .with_label_values(&[reason])
            .inc();
    }

    /// Count a dropped frame by reason.
    pub fn record_dropped(&self, reason: &str) {
        // A reason outside the list would be born at 1 and invisible to
        // `increase()`; the tests exercise every drop path, so this catches it.
        debug_assert!(
            DROP_REASONS.contains(&reason),
            "drop reason {reason:?} is not pre-created in DROP_REASONS"
        );
        self.frames_dropped.with_label_values(&[reason]).inc();
    }

    /// Render the metrics in the Prometheus text exposition format.
    pub fn render(&self) -> (String, String) {
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        // Encoding into a Vec cannot fail; ignore the Result.
        let _ = encoder.encode(&self.registry.gather(), &mut buf);
        let body = String::from_utf8(buf).unwrap_or_default();
        (encoder.format_type().to_string(), body)
    }
}

/// Decoded alarm flags: `(bitmask, label)` pairs applied to `daly_bms_alarm`.
const ALARM_FLAGS: &[(u16, &str)] = &[(0x0100, "diff_volt_l1"), (0x0200, "diff_volt_l2")];

/// Prune-then-set an indexed (per-cell/per-sensor) gauge family for one device:
/// remove series whose index was present in `prev` but is absent from `cur`, set
/// the current values, and update `prev` to the new index set.
fn sync_indexed(vec: &GaugeVec, sn: &str, prev: &mut Vec<u32>, cur: &[(u32, f64)]) {
    let cur_idx: Vec<u32> = cur.iter().map(|(n, _)| *n).collect();
    for old in prev.iter() {
        if !cur_idx.contains(old) {
            let _ = vec.remove_label_values(&[sn, &idx_label(*old)]);
        }
    }
    for (n, v) in cur {
        vec.with_label_values(&[sn, &idx_label(*n)]).set(*v);
    }
    *prev = cur_idx;
}

/// Trapezoidal integral of a signed quantity over an interval, in per-hour units,
/// split by direction (one side is always zero). Feed current (A) to get
/// amp-hours, or power (W = V*I) to get watt-hours. `dt` is clamped to `max_gap`
/// so a data gap doesn't integrate a stale reading; non-positive `dt` yields zero.
fn trapezoid_hours(prev: f64, cur: f64, dt_secs: f64, max_gap: f64) -> Split {
    if dt_secs <= 0.0 {
        return Split::default();
    }
    let dt = dt_secs.min(max_gap);
    let value = (prev + cur) / 2.0 * dt / 3600.0;
    if value >= 0.0 {
        Split {
            charge: value,
            discharge: 0.0,
        }
    } else {
        Split {
            charge: 0.0,
            discharge: -value,
        }
    }
}

/// Stage of a durable write, used as the `stage` label of
/// `daly_bms_state_write_errors_total`. `io::Result` alone would lose which step
/// failed, and "the rename failed" needs a very different response from
/// "the directory fsync is unsupported".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteStage {
    Serialize,
    CreateDir,
    Create,
    Write,
    Sync,
    Rename,
    DirSync,
}

impl WriteStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Serialize => "serialize",
            Self::CreateDir => "create_dir",
            Self::Create => "create",
            Self::Write => "write",
            Self::Sync => "sync",
            Self::Rename => "rename",
            Self::DirSync => "dirsync",
        }
    }
}

/// Durably replace `path` with `bytes`: create the parent directory, write a
/// temp file, fsync it, rename it into place, then fsync the parent directory.
///
/// Both fsyncs matter. Without the file fsync the rename can be committed while
/// the payload is still in page cache; without the directory fsync the rename
/// itself is only guaranteed by ext4's `data=ordered` until the next journal
/// commit (and on other filesystems, not at all). That is exactly the window a
/// hard reboot of the router hits — and a state file that rolls back even one
/// frame makes the restored counter lower than what /metrics already served,
/// which Prometheus reads as a counter reset.
///
/// A failed directory fsync is reported by the caller but does not fail the
/// write: some filesystems reject fsync on a directory outright.
fn write_file_durable(path: &Path, bytes: &[u8]) -> Result<(), (WriteStage, std::io::Error)> {
    use std::io::Write as _;

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = parent {
        std::fs::create_dir_all(dir).map_err(|e| (WriteStage::CreateDir, e))?;
    }

    let tmp = path.with_extension("tmp");
    let write = (|| {
        let mut f = std::fs::File::create(&tmp).map_err(|e| (WriteStage::Create, e))?;
        f.write_all(bytes).map_err(|e| (WriteStage::Write, e))?;
        f.sync_all().map_err(|e| (WriteStage::Sync, e))?;
        drop(f);
        std::fs::rename(&tmp, path).map_err(|e| (WriteStage::Rename, e))
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write;
    }

    // The rename is committed; a directory-fsync failure is worth reporting but
    // must not make the caller withhold the increment, or a filesystem that
    // cannot fsync directories would freeze energy accounting forever.
    if let Some(dir) = parent
        && let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all())
        && !matches!(
            e.kind(),
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
        )
    {
        return Err((WriteStage::DirSync, e));
    }
    Ok(())
}

/// Nudge a persisted total up by one ULP.
///
/// Belt and braces behind the `float_roundtrip` feature and the write-ahead
/// order: it keeps the file strictly above the exported value even if a future
/// serializer, a dropped cargo feature or a hand-edited file loses the last bit.
/// Prometheus reads *any* counter decrease as a reset and adds the whole total
/// to `increase()`, so a single lost bit is worth a whole phantom spike — that
/// is precisely how the +20 kWh bar of 2026-08-22 came about.
///
/// The cost is ~1e-12 Wh per restart, and it does not accumulate: every write
/// recomputes the file from the live counter rather than from its own output.
fn bump_ulp(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 {
        let up = v.next_up();
        if up.is_finite() { up } else { v }
    } else {
        v
    }
}

/// Take the pending deltas of every tracked device, zeroing them in the guard.
///
/// Every device is listed, not just the ones with pending deltas: the caller
/// writes the whole state file, so it needs a row per tracked serial. Taking the
/// deltas out (rather than reading them in place) is what lets the write happen
/// with the lock released — a frame arriving meanwhile accumulates on top of a
/// zeroed field and is picked up by the next flush.
fn take_pending(coulombs: &mut CoulombShared) -> Vec<(String, Pending)> {
    coulombs
        .devices
        .iter_mut()
        .map(|(sn, e)| {
            let taken = (sn.clone(), e.pending);
            e.pending = Pending::default();
            taken
        })
        .collect()
}

/// Add a [`Split`] (from [`trapezoid_hours`]) onto a pair of direction counters
/// for one device, skipping zero increments.
fn add_split(charge: &CounterVec, discharge: &CounterVec, sn: &str, split: Split) {
    inc_if_positive(charge, sn, split.charge);
    inc_if_positive(discharge, sn, split.discharge);
}

/// Increment a counter for `sn` by `v` when `v > 0` (a `CounterVec` panics on a
/// negative increment; a zero increment is a pointless series-creating no-op).
fn inc_if_positive(counter: &CounterVec, sn: &str, v: f64) {
    if v > 0.0 {
        counter.with_label_values(&[sn]).inc_by(v);
    }
}

/// Zero-pad a cell/sensor index so string-sorted consumers (Grafana legends,
/// bar gauges, tables) order them numerically: "01".."32" instead of "1","10","2".
fn idx_label(n: u32) -> String {
    format!("{n:02}")
}

fn set_gauge(m: &GaugeVec, sn: &str, v: Option<f64>) {
    if let Some(v) = v {
        m.with_label_values(&[sn]).set(v);
    }
}

fn set_int_gauge(m: &IntGaugeVec, sn: &str, v: Option<i64>) {
    if let Some(v) = v {
        m.with_label_values(&[sn]).set(v);
    }
}

fn set_limit(m: &GaugeVec, sn: &str, l: Limits) {
    if let Some(w) = l.warning {
        m.with_label_values(&[sn, "warning"]).set(w);
    }
    if let Some(p) = l.protection {
        m.with_label_values(&[sn, "protection"]).set(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realtime frame carrying just the two fields the coulomb counter reads.
    /// Calibration needs `remaining_ah` on top of these, so a frame built here
    /// exercises the raw counters only — which is what these tests are about.
    fn frame(current_a: Option<f64>, pack_v: Option<f64>) -> RealtimeData {
        RealtimeData {
            current_a,
            pack_v,
            ..RealtimeData::default()
        }
    }

    #[test]
    fn render_contains_updated_series() {
        let m = Metrics::new(MetricsOptions::default());
        let data = RealtimeData {
            pack_v: Some(26.9),
            ..Default::default()
        };
        m.update_realtime("SN1", &data);
        m.mark_seen("SN1");
        let (ctype, body) = m.render();
        assert!(ctype.starts_with("text/plain"));
        assert!(body.contains("daly_bms_pack_voltage_volts{sn=\"SN1\"} 26.9"));
        assert!(body.contains("daly_bms_last_frame_timestamp_seconds{sn=\"SN1\"}"));
    }

    #[test]
    fn stale_cell_series_are_pruned() {
        let m = Metrics::new(MetricsOptions::default());
        let d3 = RealtimeData {
            cells_v: vec![(1, 2.0), (2, 2.0), (3, 2.0)],
            ..Default::default()
        };
        m.update_realtime("SN1", &d3);
        assert!(m.render().1.contains("cell=\"03\""));

        let d2 = RealtimeData {
            cells_v: vec![(1, 2.0), (2, 2.0)],
            ..Default::default()
        };
        m.update_realtime("SN1", &d2);
        assert!(!m.render().1.contains("cell=\"03\""));
    }

    #[test]
    fn alarm_bits_decode_to_labelled_flags() {
        let m = Metrics::new(MetricsOptions::default());
        let data = RealtimeData {
            alarm_bits: Some(0x0300),
            ..Default::default()
        };
        m.update_realtime("SN1", &data);
        let body = m.render().1;
        assert!(body.contains("daly_bms_alarm{sn=\"SN1\",type=\"diff_volt_l1\"} 1"));
        assert!(body.contains("daly_bms_alarm{sn=\"SN1\",type=\"diff_volt_l2\"} 1"));
    }

    #[test]
    fn trapezoid_hours_directions_and_gap_cap() {
        let charge = |v: f64| Split {
            charge: v,
            discharge: 0.0,
        };
        // 10 A charge for 1 h, but dt capped to 900 s -> 10*900/3600 = 2.5 Ah.
        assert_eq!(trapezoid_hours(10.0, 10.0, 3600.0, 900.0), charge(2.5));
        // -20 A discharge for 900 s -> 5.0 Ah discharge.
        let s = trapezoid_hours(-20.0, -20.0, 900.0, 900.0);
        assert!(s.charge == 0.0 && (s.discharge - 5.0).abs() < 1e-9);
        // Non-positive dt -> nothing.
        assert_eq!(trapezoid_hours(10.0, 10.0, 0.0, 900.0), Split::default());
        // Trapezoidal average across a sign change (avg = 0) -> nothing.
        assert_eq!(trapezoid_hours(10.0, -10.0, 100.0, 900.0), Split::default());
        // Fed power (260 W = 10 A * 26 V) for 900 s -> 65 Wh.
        assert_eq!(trapezoid_hours(260.0, 260.0, 900.0, 900.0), charge(65.0));
    }

    #[test]
    fn accumulate_coulombs_integrates_charge_and_energy() {
        let m = Metrics::new(MetricsOptions::default());
        // First frame just sets the baseline (no increment).
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0);
        // Second frame 3600 s later at 10 A / 26 V -> dt capped to 900 s ->
        // 2.5 Ah charge and (10*26)*900/3600 = 65 Wh charge.
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 3600.0);
        let body = m.render().1;
        assert!(
            body.contains("daly_bms_charge_amp_hours_total{sn=\"SN1\"} 2.5"),
            "unexpected body: {body}"
        );
        assert!(
            body.contains("daly_bms_charge_watt_hours_total{sn=\"SN1\"} 65"),
            "unexpected body: {body}"
        );
    }

    #[test]
    fn accumulate_coulombs_discharge_and_skips_energy_without_voltage() {
        let m = Metrics::new(MetricsOptions::default());
        // Discharge with voltage: -10 A / 26 V over 900 s -> 2.5 Ah and 65 Wh discharge.
        m.accumulate_coulombs("SN1", &frame(Some(-10.0), Some(26.0)), 1_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(-10.0), Some(26.0)), 1_000.0 + 900.0);
        // Current present but no voltage: amp-hours accumulate, energy is skipped.
        m.accumulate_coulombs("SN2", &frame(Some(-10.0), None), 2_000.0);
        m.accumulate_coulombs("SN2", &frame(Some(-10.0), None), 2_000.0 + 900.0);
        let body = m.render().1;
        assert!(
            body.contains("daly_bms_discharge_amp_hours_total{sn=\"SN1\"} 2.5"),
            "unexpected body: {body}"
        );
        assert!(
            body.contains("daly_bms_discharge_watt_hours_total{sn=\"SN1\"} 65"),
            "unexpected body: {body}"
        );
        assert!(
            body.contains("daly_bms_discharge_amp_hours_total{sn=\"SN2\"} 2.5"),
            "unexpected body: {body}"
        );
        // No pack voltage for SN2 -> no watt-hour series must be minted for it.
        assert!(
            !body.contains("daly_bms_discharge_watt_hours_total{sn=\"SN2\"}"),
            "energy minted without voltage: {body}"
        );
    }

    #[test]
    fn coulomb_and_energy_state_persist_across_restart() {
        let path = temp_state_path("energy-persist");
        {
            let m = Metrics::new(MetricsOptions {
                coulomb_state_path: Some(path.clone()),
                ..Default::default()
            });
            m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0); // baseline
            m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 3600.0); // +2.5 Ah / 65 Wh
        }
        // A fresh instance (simulating a restart) restores both totals.
        let m2 = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            ..Default::default()
        });
        m2.restore_coulombs();
        let body = m2.render().1;
        // Compare parsed values, not string prefixes: the persisted totals are
        // nudged up by one ULP, so `contains("} 65")` would pass on 65.00000001
        // and hide a real drift.
        let ah = metric_value(&body, "daly_bms_charge_amp_hours_total{sn=\"SN1\"}")
            .unwrap_or_else(|| panic!("no amp-hour series: {body}"));
        let wh = metric_value(&body, "daly_bms_charge_watt_hours_total{sn=\"SN1\"}")
            .unwrap_or_else(|| panic!("no watt-hour series: {body}"));
        assert!((ah - 2.5).abs() < 1e-9, "restored {ah} Ah");
        assert!((wh - 65.0).abs() < 1e-9, "restored {wh} Wh");
        let _ = std::fs::remove_file(&path);
    }

    /// A frame that also carries what the calibrator needs: the balancer bleed
    /// and the BMS's remaining capacity.
    fn cal_frame(
        current_a: f64,
        pack_v: f64,
        balance_a: Option<f64>,
        remaining_ah: Option<f64>,
    ) -> RealtimeData {
        RealtimeData {
            current_a: Some(current_a),
            pack_v: Some(pack_v),
            balance_current_a: balance_a,
            remaining_ah,
            ..RealtimeData::default()
        }
    }

    /// Calibration options with both warmup gates opened, so a test can reach
    /// the applied-offset state in a handful of frames instead of a month.
    fn eager_calibration() -> calibration::Options {
        calibration::Options {
            min_span_hours: 0.0,
            max_anchor_error_amperes: 1_000.0,
            ..calibration::Options::default()
        }
    }

    #[test]
    fn balancer_bleed_is_counted() {
        let m = Metrics::new(MetricsOptions::default());
        // 2 A of bleed for an hour, capped to the 900 s gap -> 0.5 Ah.
        m.accumulate_coulombs("SN1", &cal_frame(0.0, 26.0, Some(2.0), None), 1_000.0);
        m.accumulate_coulombs(
            "SN1",
            &cal_frame(0.0, 26.0, Some(2.0), None),
            1_000.0 + 3600.0,
        );
        let body = m.render().1;
        let ah = metric_value(&body, "daly_bms_balance_amp_hours_total{sn=\"SN1\"}")
            .unwrap_or_else(|| panic!("no balance series: {body}"));
        assert!((ah - 0.5).abs() < 1e-9, "counted {ah} Ah of bleed");
    }

    #[test]
    fn a_negative_balance_reading_does_not_reduce_the_bleed_counter() {
        // The register is a magnitude; a negative decode is noise, and a counter
        // cannot go down anyway — it must simply contribute nothing.
        let m = Metrics::new(MetricsOptions::default());
        m.accumulate_coulombs("SN1", &cal_frame(0.0, 26.0, Some(-5.0), None), 1_000.0);
        m.accumulate_coulombs(
            "SN1",
            &cal_frame(0.0, 26.0, Some(-5.0), None),
            1_000.0 + 900.0,
        );
        let body = m.render().1;
        assert!(
            metric_value(&body, "daly_bms_balance_amp_hours_total{sn=\"SN1\"}")
                .is_none_or(|v| v.abs() < 1e-12),
            "negative bleed must not be counted: {body}"
        );
    }

    #[test]
    fn calibrated_counters_track_the_raw_ones_while_warming_up() {
        let m = Metrics::new(MetricsOptions::default());
        // Default options hold the correction for ~800 integrated hours, so the
        // two sets of counters must agree exactly for now.
        for i in 0..10 {
            m.accumulate_coulombs(
                "SN1",
                &cal_frame(10.0, 26.0, Some(0.0), Some(20.0)),
                1_000.0 + f64::from(i) * 300.0,
            );
        }
        let body = m.render().1;
        let raw = metric_value(&body, "daly_bms_charge_amp_hours_total{sn=\"SN1\"}")
            .unwrap_or_else(|| panic!("no raw series: {body}"));
        let cal = metric_value(
            &body,
            "daly_bms_calibrated_charge_amp_hours_total{sn=\"SN1\"}",
        )
        .unwrap_or_else(|| panic!("no calibrated series: {body}"));
        assert!((raw - cal).abs() < 1e-9, "raw {raw} vs calibrated {cal}");
        assert_eq!(
            metric_value(&body, "daly_bms_calibration_hold{sn=\"SN1\"}"),
            Some(1.0),
            "must report the warmup hold"
        );
    }

    #[test]
    fn an_applied_offset_separates_the_calibrated_counters() {
        let m = Metrics::new(MetricsOptions {
            calibration: eager_calibration(),
            ..Default::default()
        });
        // A steady 1 A "charge" that never moves the remaining capacity is,
        // by the balance equation, entirely sensor offset. The estimate lands
        // at 1 A and is clamped to the 0.5 A ceiling.
        for i in 0..20 {
            m.accumulate_coulombs(
                "SN1",
                &cal_frame(1.0, 26.0, Some(0.0), Some(20.0)),
                1_000.0 + f64::from(i) * 300.0,
            );
        }
        let body = m.render().1;
        let offset = metric_value(&body, "daly_bms_current_offset_amperes{sn=\"SN1\"}")
            .unwrap_or_else(|| panic!("no offset gauge: {body}"));
        assert!((offset - 0.5).abs() < 1e-9, "offset {offset}");
        let raw = metric_value(&body, "daly_bms_charge_amp_hours_total{sn=\"SN1\"}")
            .unwrap_or_else(|| panic!("no raw series: {body}"));
        let cal = metric_value(
            &body,
            "daly_bms_calibrated_charge_amp_hours_total{sn=\"SN1\"}",
        )
        .unwrap_or_else(|| panic!("no calibrated series: {body}"));
        assert!(
            cal < raw * 0.75,
            "a 0.5 A correction on a 1 A reading must roughly halve it: raw {raw}, calibrated {cal}"
        );
        assert_eq!(
            metric_value(&body, "daly_bms_calibration_hold{sn=\"SN1\"}"),
            Some(0.0),
            "the correction is applied, so nothing is holding it"
        );
    }

    #[test]
    fn calibration_can_be_switched_off() {
        let m = Metrics::new(MetricsOptions {
            calibration_enabled: false,
            calibration: eager_calibration(),
            ..Default::default()
        });
        for i in 0..20 {
            m.accumulate_coulombs(
                "SN1",
                &cal_frame(1.0, 26.0, Some(0.0), Some(20.0)),
                1_000.0 + f64::from(i) * 300.0,
            );
        }
        let body = m.render().1;
        assert!(
            !body.contains("daly_bms_current_offset_amperes{"),
            "a disabled estimator must publish no calibration series: {body}"
        );
        let raw =
            metric_value(&body, "daly_bms_charge_amp_hours_total{sn=\"SN1\"}").expect("raw series");
        let cal = metric_value(
            &body,
            "daly_bms_calibrated_charge_amp_hours_total{sn=\"SN1\"}",
        )
        .expect("calibrated series");
        assert!(
            (raw - cal).abs() < 1e-9,
            "with no offset the two must stay identical: raw {raw}, calibrated {cal}"
        );
    }

    #[test]
    fn calibration_state_and_counters_survive_a_restart() {
        let path = temp_state_path("calibration-persist");
        let opts = || MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            calibration: eager_calibration(),
            ..Default::default()
        };
        {
            let m = Metrics::new(opts());
            for i in 0..20 {
                m.accumulate_coulombs(
                    "SN1",
                    &cal_frame(1.0, 26.0, Some(0.2), Some(20.0)),
                    1_000.0 + f64::from(i) * 300.0,
                );
            }
            m.persist_coulombs();
        }
        let m2 = Metrics::new(opts());
        m2.restore_coulombs();
        // One more frame, so the restored estimator publishes its gauges again.
        m2.accumulate_coulombs(
            "SN1",
            &cal_frame(1.0, 26.0, Some(0.2), Some(20.0)),
            1_000.0 + 20.0 * 300.0,
        );
        let body = m2.render().1;
        let offset = metric_value(&body, "daly_bms_current_offset_amperes{sn=\"SN1\"}")
            .unwrap_or_else(|| panic!("no offset gauge after restart: {body}"));
        assert!(
            (offset - 0.5).abs() < 1e-9,
            "the estimate must survive a restart, got {offset}"
        );
        assert!(
            metric_value(&body, "daly_bms_balance_amp_hours_total{sn=\"SN1\"}")
                .expect("balance series")
                > 0.0,
            "the bleed counter must be restored too"
        );
        assert!(
            metric_value(
                &body,
                "daly_bms_calibrated_charge_amp_hours_total{sn=\"SN1\"}"
            )
            .expect("calibrated series")
                > 0.0,
            "the calibrated counter must be restored too"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persisted_calibrated_totals_are_never_below_the_exported_ones() {
        // The same invariant the raw counters have: a restart that restored a
        // lower total would read to Prometheus as a counter reset, adding the
        // whole accumulated total to `increase()`.
        let path = temp_state_path("calibrated-never-below");
        let m = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            state_min_interval: Duration::from_millis(1),
            calibration: eager_calibration(),
            ..Default::default()
        });
        for i in 0..40 {
            m.accumulate_coulombs(
                "SN1",
                &cal_frame(3.0, 26.5, Some(0.3), Some(20.0)),
                1_000.0 + f64::from(i) * 120.0,
            );
            m.persist_coulombs();
            let body = m.render().1;
            let state: CoulombState =
                serde_json::from_slice(&std::fs::read(&path).expect("state file"))
                    .expect("valid state");
            let e = state.devices.get("SN1").expect("device row");
            for (name, on_disk) in [
                (
                    "daly_bms_calibrated_charge_amp_hours_total",
                    e.cal_charge_ah,
                ),
                (
                    "daly_bms_calibrated_discharge_amp_hours_total",
                    e.cal_discharge_ah,
                ),
                (
                    "daly_bms_calibrated_charge_watt_hours_total",
                    e.cal_charge_wh,
                ),
                ("daly_bms_balance_amp_hours_total", e.balance_ah),
            ] {
                let exported = metric_value(&body, &format!("{name}{{sn=\"SN1\"}}")).unwrap_or(0.0);
                assert!(
                    on_disk >= exported,
                    "iteration {i}: {name} on disk {on_disk} < exported {exported}"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Unique temp path per test: the suite runs tests in parallel, so a shared
    /// file name would make them clobber each other's state.
    fn temp_state_path(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("daly-bms-{tag}.json"));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn read_state(path: &Path) -> CoulombState {
        let bytes = std::fs::read(path).expect("state file exists");
        serde_json::from_slice(&bytes).expect("state file parses")
    }

    /// The four persisted totals of one device, in `CoulombEntry` field order.
    fn counters(m: &Metrics, sn: &str) -> [f64; 4] {
        [
            m.charge_amp_hours.with_label_values(&[sn]).get(),
            m.discharge_amp_hours.with_label_values(&[sn]).get(),
            m.charge_watt_hours.with_label_values(&[sn]).get(),
            m.discharge_watt_hours.with_label_values(&[sn]).get(),
        ]
    }

    fn persisted(state: &CoulombState, sn: &str) -> [f64; 4] {
        let e = &state.devices[sn];
        [e.charge_ah, e.discharge_ah, e.charge_wh, e.discharge_wh]
    }

    /// Extract the value of an exact `metric{labels}` line from a render.
    fn metric_value(body: &str, series: &str) -> Option<f64> {
        body.lines()
            .find_map(|l| l.strip_prefix(series)?.trim().parse().ok())
    }

    #[test]
    fn persisted_state_never_below_exported_counters() {
        // The production regression: the state file fell one frame behind the
        // counters, so the restored counter was lower than what /metrics had
        // served and Prometheus read it as a reset.
        let path = temp_state_path("never-below");
        let m = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            state_min_interval: Duration::ZERO,
            ..Default::default()
        });
        // A current near zero is what made the production delta ~4e-12.
        let mut checked = 0usize;
        for i in 0..50 {
            m.accumulate_coulombs(
                "SN1",
                &frame(Some(0.01), Some(26.0)),
                1_000.0 + f64::from(i) * 200.0,
            );
            if !path.exists() {
                // The first frame only sets the baseline, producing no delta and
                // therefore no write: idle frames must not cost an fsync.
                continue;
            }
            let file = persisted(&read_state(&path), "SN1");
            let exported = counters(&m, "SN1");
            for (f, e) in file.iter().zip(exported.iter()) {
                assert!(f >= e, "file {f} < exported {e} after frame {i}");
                checked += 1;
            }
        }
        // Without this the test passes vacuously if writes stop happening at
        // all — every iteration would take the `continue` above.
        assert!(
            checked > 100,
            "state file was barely written: {checked} checks"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn restart_never_decreases_counters() {
        let path = temp_state_path("restart-monotonic");
        let before = {
            let m = Metrics::new(MetricsOptions {
                coulomb_state_path: Some(path.clone()),
                state_min_interval: Duration::ZERO,
                ..Default::default()
            });
            for i in 0..10 {
                m.accumulate_coulombs(
                    "SN1",
                    &frame(Some(-3.0), Some(26.5)),
                    1_000.0 + f64::from(i) * 60.0,
                );
            }
            m.persist_coulombs();
            counters(&m, "SN1")
        };

        let m2 = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            ..Default::default()
        });
        m2.restore_coulombs();
        let after = counters(&m2, "SN1");
        for (a, b) in after.iter().zip(before.iter()) {
            assert!(a >= b, "counter decreased across restart: {a} < {b}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_failure_freezes_counters_and_recovers() {
        // Point the state path *inside* a regular file, so create_dir_all fails.
        // No root and no side effects needed to exercise the error branch.
        let blocker = temp_state_path("write-failure-blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let m = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(blocker.join("coulombs.json")),
            state_min_interval: Duration::ZERO,
            ..Default::default()
        });

        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 900.0);
        assert_eq!(
            counters(&m, "SN1"),
            [0.0; 4],
            "counters advanced despite a failed write"
        );
        let body = m.render().1;
        assert!(
            body.contains("daly_bms_state_write_errors_total{stage=\"create_dir\"}"),
            "no write error reported: {body}"
        );

        // The deltas are queued, not lost: once writes succeed they all land.
        let good = temp_state_path("write-failure-recovered");
        let recovered = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(good.clone()),
            state_min_interval: Duration::ZERO,
            ..Default::default()
        });
        recovered.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0);
        recovered.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 900.0);
        assert!(
            (counters(&recovered, "SN1")[0] - 2.5).abs() < 1e-9,
            "expected the same 2.5 Ah the failing instance withheld"
        );

        let _ = std::fs::remove_file(&blocker);
        let _ = std::fs::remove_file(&good);
    }

    #[test]
    fn idle_frames_keep_the_freshness_gauge_current() {
        // A resting battery reports exactly 0 A, which integrates to a zero
        // delta and therefore never writes. The gauge must still advance, or
        // the staleness alert fires on a healthy exporter — as it did in
        // production on 2026-08-26.
        let path = temp_state_path("idle-freshness");
        let m = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            state_min_interval: Duration::ZERO,
            ..Default::default()
        });
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 900.0);
        let after_write = m.state_last_write_timestamp.get();
        assert!(after_write > 1.0e9, "no write happened at all");

        // Settle into idle first: the frame that *transitions* from 10 A to 0 A
        // still integrates half the trapezoid, so it writes. Only once both
        // endpoints are zero does the delta vanish.
        m.accumulate_coulombs("SN1", &frame(Some(0.0), Some(26.0)), 2_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(0.0), Some(26.0)), 2_200.0);

        // From here on every frame is a no-op write.
        m.state_last_write_timestamp.set(0.0);
        for i in 1..=5 {
            m.accumulate_coulombs(
                "SN1",
                &frame(Some(0.0), Some(26.0)),
                2_400.0 + f64::from(i) * 200.0,
            );
        }
        assert!(
            m.state_last_write_timestamp.get() > 1.0e9,
            "freshness gauge went stale while idle"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn failing_writes_are_throttled_too() {
        // Throttling on successes alone would leave a broken disk retrying — with
        // two fsyncs — on every frame, and a single unauthenticated POST can
        // carry ~100 of them.
        let blocker = temp_state_path("failing-writes-throttled");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let m = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(blocker.join("coulombs.json")),
            state_min_interval: Duration::from_secs(3600),
            ..Default::default()
        });

        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0);
        for i in 1..=20 {
            m.accumulate_coulombs(
                "SN1",
                &frame(Some(10.0), Some(26.0)),
                1_000.0 + f64::from(i) * 60.0,
            );
        }
        let errors = m
            .state_write_errors
            .with_label_values(&[WriteStage::CreateDir.as_str()])
            .get();
        assert_eq!(errors, 1, "each frame retried the failing write: {errors}");

        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn increments_apply_immediately_without_persistence() {
        // Without persistence there is nothing to wait for, so increments must
        // still reach the counters immediately.
        let m = Metrics::new(MetricsOptions::default());
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 900.0);
        assert!((counters(&m, "SN1")[0] - 2.5).abs() < 1e-9);
    }

    #[test]
    fn throttled_increments_are_deferred_not_lost() {
        let path = temp_state_path("throttled");
        let m = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            state_min_interval: Duration::from_secs(3600),
            ..Default::default()
        });
        // First pending delta writes (no previous write to throttle against).
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 900.0);
        let after_first = counters(&m, "SN1")[0];
        assert!((after_first - 2.5).abs() < 1e-9);

        // Everything after that is inside the window and stays pending.
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 1_800.0);
        assert_eq!(
            counters(&m, "SN1")[0],
            after_first,
            "throttled delta was exported before it was written"
        );

        // A forced flush (shutdown) applies it.
        m.persist_coulombs();
        assert!(
            (counters(&m, "SN1")[0] - 5.0).abs() < 1e-9,
            "throttled delta was lost instead of deferred"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_coulombs_is_idempotent() {
        let path = temp_state_path("persist-idempotent");
        let m = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            state_min_interval: Duration::from_secs(3600),
            ..Default::default()
        });
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(10.0), Some(26.0)), 1_000.0 + 900.0);
        m.persist_coulombs();
        let once = counters(&m, "SN1");
        m.persist_coulombs();
        assert_eq!(counters(&m, "SN1"), once, "second persist double-counted");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn drop_reason_series_exist_at_zero_from_startup() {
        // `increase()` over a series that is born at its first increment
        // returns nothing, so the first-ever drop cannot trip an alert. Every
        // reason must therefore be exported as 0 before anything is dropped.
        let m = Metrics::new(MetricsOptions::default());
        let body = m.render().1;
        for reason in DROP_REASONS {
            let line = format!("daly_bms_frames_dropped_total{{reason=\"{reason}\"}} 0");
            assert!(body.contains(&line), "missing {line} in:\n{body}");
        }
        assert!(
            body.contains("daly_bms_coulomb_samples_rejected_total{reason=\"clamped_delta\"} 0")
        );
    }

    #[test]
    fn long_gap_at_the_plausible_maximum_is_not_clamped() {
        // 99 A at 33 V over the full 900 s gap cap is 24.75 Ah — large, but it
        // is exactly what the gate and the gap cap already permit, so it is a
        // recoverable reading after a comms drop, not an anomaly. Clamping it
        // would silently under-count energy after every WiFi outage.
        let m = Metrics::new(MetricsOptions::default());
        m.accumulate_coulombs("SN1", &frame(Some(99.0), Some(33.0)), 1_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(99.0), Some(33.0)), 1_000.0 + 900.0);
        let [ah, _, wh, _] = counters(&m, "SN1");
        assert!(
            (ah - 99.0 * 0.25).abs() < 1e-9,
            "delta was clamped: {ah} Ah"
        );
        assert!((wh - 99.0 * 33.0 * 0.25).abs() < 1e-9, "clamped: {wh} Wh");
        assert!(
            m.render()
                .1
                .contains("daly_bms_coulomb_samples_rejected_total{reason=\"clamped_delta\"} 0"),
            "a legitimate long gap must not be reported as clamped"
        );
    }

    #[test]
    fn operator_lowered_frame_ceiling_is_enforced() {
        // The per-frame ceiling is raised to whatever the gate and the gap cap
        // already allow, so it only bites when an operator deliberately sets it
        // below that — e.g. because their pack cannot physically take 25 Ah.
        let m = Metrics::new(MetricsOptions {
            max_frame_amp_hours: 1.0,
            max_frame_watt_hours: 30.0,
            ..Default::default()
        });
        m.accumulate_coulombs("SN1", &frame(Some(20.0), Some(26.0)), 1_000.0);
        m.accumulate_coulombs("SN1", &frame(Some(20.0), Some(26.0)), 1_000.0 + 900.0);
        let [ah, _, wh, _] = counters(&m, "SN1");
        // Uncapped this would be 5 Ah / 130 Wh.
        assert!((ah - 1.0).abs() < 1e-9, "amp-hours not clamped: {ah}");
        assert!((wh - 30.0).abs() < 1e-9, "watt-hours not clamped: {wh}");
        assert!(
            m.render()
                .1
                .contains("daly_bms_coulomb_samples_rejected_total{reason=\"clamped_delta\"}")
        );
    }

    #[test]
    fn restore_seeds_the_write_freshness_gauge() {
        // Otherwise the staleness alert reads "not written since the epoch"
        // whenever an idle battery gives the exporter nothing to persist.
        let path = temp_state_path("restore-seeds-gauge");
        std::fs::write(&path, br#"{"devices":{}}"#).unwrap();
        let m = Metrics::new(MetricsOptions {
            coulomb_state_path: Some(path.clone()),
            ..Default::default()
        });
        assert_eq!(m.state_last_write_timestamp.get(), 0.0);
        m.restore_coulombs();
        assert!(m.state_last_write_timestamp.get() > 1.0e9);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bump_ulp_is_monotonic_and_safe() {
        assert!(bump_ulp(1.0) > 1.0);
        assert!(
            bump_ulp(1.0) - 1.0 < 1e-15,
            "nudge must be one ULP, not more"
        );
        assert_eq!(bump_ulp(0.0), 0.0, "an untouched counter must stay at zero");
        assert_eq!(bump_ulp(-1.0), -1.0);
        assert!(bump_ulp(f64::MAX).is_finite(), "must not overflow to +Inf");
    }

    #[test]
    fn write_file_durable_replaces_atomically() {
        let dir = std::env::temp_dir().join("daly-bms-durable-write-test");
        let _ = std::fs::remove_dir_all(&dir);
        // The parent directory does not exist yet: the write must create it.
        let path = dir.join("state.json");

        write_file_durable(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        write_file_durable(&path, b"second").expect("overwrite");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(
            !path.with_extension("tmp").exists(),
            "temp file left behind after a successful write"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_durable_reports_the_failing_stage() {
        // Parent path component is a regular file, so create_dir_all fails. This
        // needs no root and leaves nothing behind but the temp file itself.
        let blocker = std::env::temp_dir().join("daly-bms-durable-write-blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();

        let err = write_file_durable(&blocker.join("state.json"), b"x")
            .expect_err("writing under a regular file must fail");
        assert_eq!(err.0, WriteStage::CreateDir);

        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn old_state_file_without_energy_loads() {
        // A pre-v0.1.5 state file has only the *_ah fields; the *_wh fields must
        // default to 0.0 (serde default) so restore does not fail.
        let json = r#"{"devices":{"SN1":{"charge_ah":1.5,"discharge_ah":2.0}}}"#;
        let state: CoulombState = serde_json::from_str(json).expect("legacy file loads");
        let e = &state.devices["SN1"];
        assert_eq!((e.charge_ah, e.discharge_ah), (1.5, 2.0));
        assert_eq!((e.charge_wh, e.discharge_wh), (0.0, 0.0));
    }

    #[test]
    fn admit_caps_distinct_devices() {
        let m = Metrics::new(MetricsOptions {
            max_devices: 2,
            ..Default::default()
        });
        // admit reserves the slot, so devices that never decode still count.
        assert!(m.admit("A"), "1st device reserved");
        assert!(m.admit("B"), "2nd device reserved");
        assert!(m.admit("A"), "known device stays admitted");
        assert!(!m.admit("C"), "3rd distinct device rejected at cap");

        let unlimited = Metrics::new(MetricsOptions {
            max_devices: 0,
            ..Default::default()
        });
        assert!(unlimited.admit("anything"), "0 = unlimited");
        assert!(unlimited.admit("another"));
    }
}
