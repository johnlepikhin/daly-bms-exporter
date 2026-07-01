//! Prometheus metric families and the update logic that maps decoded telemetry
//! onto them. All families are labelled by `sn`; per-cell/sensor series are
//! pruned when a device reports fewer cells so stale readings don't linger.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus::{
    CounterVec, Encoder, GaugeVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
use serde::{Deserialize, Serialize};

use crate::decode::{ConfigData, Limits, RealtimeData};

/// On-disk snapshot of the coulomb counters, keyed by serial.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CoulombState {
    devices: BTreeMap<String, CoulombEntry>,
}

/// Persisted charge/discharge totals (amp-hours) for one device.
#[derive(Debug, Serialize, Deserialize)]
struct CoulombEntry {
    charge_ah: f64,
    discharge_ah: f64,
}

/// Per-device record of which cell/sensor series currently exist, so we can
/// remove series that disappear (e.g. cell count shrinks or device goes away).
#[derive(Default)]
struct LastSeries {
    cells: Vec<u32>,
    sensors: Vec<u32>,
    /// Coulomb-counter state: time and pack current of the previous frame.
    last_coulomb_ts: Option<f64>,
    last_current: Option<f64>,
    /// Last label-tuple written to `device_info`, so the previous (possibly
    /// stale) series can be removed before a differing tuple is set. Guards
    /// against unbounded series growth from a device varying its identity
    /// strings (`daly_bms_device_info` cardinality bomb).
    last_device_info: Option<[String; 5]>,
    /// Real pack serial decoded from the realtime block (register data), used to
    /// populate the `serial` label of `daly_bms_device_info`. `None` until a
    /// realtime frame carrying a serial has been seen.
    realtime_serial: Option<String>,
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

    // Exporter self-observability.
    http_requests: IntCounterVec,   // {endpoint, status}
    frames_decoded: IntCounterVec,  // {block}
    frames_dropped: IntCounterVec,  // {reason}
    last_frame_timestamp: GaugeVec, // {sn}

    /// Integration-interval cap for the coulomb counter (seconds).
    coulomb_max_gap_secs: f64,
    /// Hard cap on distinct tracked serials (`0` = unlimited).
    max_devices: usize,
    /// If set, coulomb totals are persisted here and restored on startup.
    coulomb_state_path: Option<PathBuf>,
    seen: Mutex<HashMap<String, LastSeries>>,
}

/// Register a `GaugeVec` on the registry (name collisions are a startup bug).
fn register_gauge_vec(reg: &Registry, name: &str, help: &str, labels: &[&str]) -> GaugeVec {
    let m = GaugeVec::new(Opts::new(name, help), labels).expect("valid metric");
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
    /// Construct the metric registry and all metric families.
    ///
    /// `coulomb_max_gap_secs` caps the coulomb-counter integration interval
    /// (seconds); `max_devices` is a hard cap on distinct tracked serials
    /// (`0` = unlimited); `coulomb_state_path`, if set, persists the coulomb
    /// totals to that file (call [`Metrics::restore_coulombs`] after construction).
    ///
    /// # Panics
    ///
    /// Panics only at startup if a metric name is duplicate or invalid (a
    /// programming bug).
    #[expect(clippy::too_many_lines)]
    pub fn new(
        coulomb_max_gap_secs: f64,
        max_devices: usize,
        coulomb_state_path: Option<PathBuf>,
    ) -> Self {
        let r = Registry::new();
        Self {
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
            coulomb_max_gap_secs,
            max_devices,
            coulomb_state_path,
            registry: r,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a telemetry frame for `sn` should be processed. Returns `true`
    /// for an already-tracked device or while under the `max_devices` cap;
    /// `false` once the cap is reached for a new serial (bounds cardinality).
    pub fn admit(&self, sn: &str) -> bool {
        if self.max_devices == 0 {
            return true;
        }
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.contains_key(sn) {
            return true;
        }
        if seen.len() < self.max_devices {
            // Reserve the slot now, so that even frames which never decode (and
            // thus never reach `update_*`) still count against the cap — e.g.
            // `mark_seen` mints an `sn`-labelled series unconditionally.
            seen.insert(sn.to_string(), LastSeries::default());
            true
        } else {
            false
        }
    }

    /// Apply a decoded realtime frame, pruning stale per-cell/sensor series.
    pub fn update_realtime(&self, sn: &str, d: &RealtimeData) {
        {
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            // Avoid allocating a String key on every frame: only insert when the
            // device is new, then take a mutable borrow of the existing entry.
            if !seen.contains_key(sn) {
                if self.max_devices != 0 && seen.len() >= self.max_devices {
                    return; // cap reached; do not create a new device entry
                }
                seen.insert(sn.to_string(), LastSeries::default());
            }
            let entry = seen.get_mut(sn).expect("just inserted or present");

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
        // build+prune+set runs under the `seen` lock so the metric mutation is
        // serialized with the tracked-identity state.
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if !seen.contains_key(sn) {
            if self.max_devices != 0 && seen.len() >= self.max_devices {
                return; // cap reached; do not create a new device entry
            }
            seen.insert(sn.to_string(), LastSeries::default());
        }
        let entry = seen.get_mut(sn).expect("just inserted or present");

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
    /// the `seen` lock so the metric mutation stays serialized with `entry`.
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
    /// (coulomb counting). Call once per accepted realtime frame with the
    /// wall-clock time; trapezoidal over the interval since the previous frame.
    pub fn accumulate_coulombs(&self, sn: &str, current_a: Option<f64>, now_secs: f64) {
        let Some(cur) = current_a else { return };
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if !seen.contains_key(sn) {
            if self.max_devices != 0 && seen.len() >= self.max_devices {
                return; // cap reached; do not create a new device entry
            }
            seen.insert(sn.to_string(), LastSeries::default());
        }
        let entry = seen.get_mut(sn).expect("just inserted or present");
        if let (Some(last_ts), Some(last_cur)) = (entry.last_coulomb_ts, entry.last_current) {
            let (charge, discharge) =
                coulomb_increment(last_cur, cur, now_secs - last_ts, self.coulomb_max_gap_secs);
            if charge > 0.0 {
                self.charge_amp_hours
                    .with_label_values(&[sn])
                    .inc_by(charge);
            }
            if discharge > 0.0 {
                self.discharge_amp_hours
                    .with_label_values(&[sn])
                    .inc_by(discharge);
            }
        }
        entry.last_coulomb_ts = Some(now_secs);
        entry.last_current = Some(cur);
        // The mutable borrow of `entry` ends here; persist the totals (no-op if
        // persistence is not configured) while still holding the `seen` lock.
        self.write_coulomb_state(&seen);
    }

    /// Restore the coulomb counters from `coulomb_state_path` (if configured),
    /// seeding the `charge/discharge_amp_hours` counters so totals survive a
    /// restart. A missing file is a normal first run; a corrupt file is logged
    /// and ignored. Call once at startup after [`Metrics::new`].
    pub fn restore_coulombs(&self) {
        let Some(path) = &self.coulomb_state_path else {
            return;
        };
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
                tracing::warn!(error = %e, "corrupt coulomb state; starting fresh");
                return;
            }
        };
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        for (sn, e) in &state.devices {
            if e.charge_ah > 0.0 {
                self.charge_amp_hours
                    .with_label_values(&[sn])
                    .inc_by(e.charge_ah);
            }
            if e.discharge_ah > 0.0 {
                self.discharge_amp_hours
                    .with_label_values(&[sn])
                    .inc_by(e.discharge_ah);
            }
            // Track the serial so future writes keep persisting it.
            seen.entry(sn.clone()).or_default();
        }
        tracing::info!(devices = state.devices.len(), "restored coulomb counters");
    }

    /// Persist the coulomb counters now (e.g. on graceful shutdown). No-op if
    /// persistence is not configured.
    pub fn persist_coulombs(&self) {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        self.write_coulomb_state(&seen);
    }

    /// Atomically write the coulomb totals of all tracked devices to
    /// `coulomb_state_path`. Must be called while holding the `seen` lock.
    fn write_coulomb_state(&self, seen: &HashMap<String, LastSeries>) {
        let Some(path) = &self.coulomb_state_path else {
            return;
        };
        let mut state = CoulombState::default();
        for sn in seen.keys() {
            let charge_ah = self.charge_amp_hours.with_label_values(&[sn]).get();
            let discharge_ah = self.discharge_amp_hours.with_label_values(&[sn]).get();
            state.devices.insert(
                sn.clone(),
                CoulombEntry {
                    charge_ah,
                    discharge_ah,
                },
            );
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        let write = serde_json::to_vec(&state)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = write {
            tracing::warn!(error = %e, path = %path.display(), "cannot persist coulomb state");
        }
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

    /// Count a dropped frame by reason.
    pub fn record_dropped(&self, reason: &str) {
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

/// Trapezoidal coulomb increment over an interval, split by direction. Returns
/// `(charge_ah, discharge_ah)` (one is always zero). `dt` is clamped to
/// `max_gap` so a data gap doesn't integrate a stale current; non-positive `dt`
/// yields zero.
fn coulomb_increment(prev_i: f64, cur_i: f64, dt_secs: f64, max_gap: f64) -> (f64, f64) {
    if dt_secs <= 0.0 {
        return (0.0, 0.0);
    }
    let dt = dt_secs.min(max_gap);
    let ah = (prev_i + cur_i) / 2.0 * dt / 3600.0;
    if ah >= 0.0 { (ah, 0.0) } else { (0.0, -ah) }
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

    #[test]
    fn render_contains_updated_series() {
        let m = Metrics::new(900.0, 1024, None);
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
        let m = Metrics::new(900.0, 1024, None);
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
        let m = Metrics::new(900.0, 1024, None);
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
    fn coulomb_increment_directions_and_gap_cap() {
        // 10 A charge for 1 h, but dt capped to 900 s -> 10*900/3600 = 2.5 Ah.
        assert_eq!(coulomb_increment(10.0, 10.0, 3600.0, 900.0), (2.5, 0.0));
        // -20 A discharge for 900 s -> 5.0 Ah discharge.
        let (c, d) = coulomb_increment(-20.0, -20.0, 900.0, 900.0);
        assert!(c == 0.0 && (d - 5.0).abs() < 1e-9);
        // Non-positive dt -> nothing.
        assert_eq!(coulomb_increment(10.0, 10.0, 0.0, 900.0), (0.0, 0.0));
        // Trapezoidal average across a sign change (avg = 0) -> nothing.
        assert_eq!(coulomb_increment(10.0, -10.0, 100.0, 900.0), (0.0, 0.0));
    }

    #[test]
    fn accumulate_coulombs_integrates_between_frames() {
        let m = Metrics::new(900.0, 1024, None);
        // First frame just sets the baseline (no increment).
        m.accumulate_coulombs("SN1", Some(10.0), 1_000.0);
        // Second frame 3600 s later, 10 A -> dt capped to 900 -> 2.5 Ah charge.
        m.accumulate_coulombs("SN1", Some(10.0), 1_000.0 + 3600.0);
        let body = m.render().1;
        assert!(
            body.contains("daly_bms_charge_amp_hours_total{sn=\"SN1\"} 2.5"),
            "unexpected body: {body}"
        );
    }

    #[test]
    fn coulomb_state_persists_across_restart() {
        let path = std::env::temp_dir().join("daly-bms-coulomb-persist-test.json");
        let _ = std::fs::remove_file(&path);
        {
            let m = Metrics::new(900.0, 1024, Some(path.clone()));
            m.accumulate_coulombs("SN1", Some(10.0), 1_000.0); // baseline
            m.accumulate_coulombs("SN1", Some(10.0), 1_000.0 + 3600.0); // +2.5 Ah, writes file
        }
        // A fresh instance (simulating a restart) restores the persisted total.
        let m2 = Metrics::new(900.0, 1024, Some(path.clone()));
        m2.restore_coulombs();
        let body = m2.render().1;
        assert!(
            body.contains("daly_bms_charge_amp_hours_total{sn=\"SN1\"} 2.5"),
            "restored body: {body}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn admit_caps_distinct_devices() {
        let m = Metrics::new(900.0, 2, None);
        // admit reserves the slot, so devices that never decode still count.
        assert!(m.admit("A"), "1st device reserved");
        assert!(m.admit("B"), "2nd device reserved");
        assert!(m.admit("A"), "known device stays admitted");
        assert!(!m.admit("C"), "3rd distinct device rejected at cap");

        let unlimited = Metrics::new(900.0, 0, None);
        assert!(unlimited.admit("anything"), "0 = unlimited");
        assert!(unlimited.admit("another"));
    }
}
