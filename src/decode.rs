//! Domain decoding: map Modbus registers to typed telemetry per
//! `doc/daly-bms-protocol.md` §4 (realtime) and §5 (config).
//!
//! Every field is read through `.get()` and is `Option`/`Vec`: the input is
//! untrusted device data, so a short or garbage frame yields absent fields, not
//! a panic.

use crate::error::DecodeError;

/// Physical limits used to clamp untrusted count registers.
const MAX_CELLS: u16 = 32; // cell-voltage slots span registers 0x00..0x1F
const MAX_TEMPS: u16 = 8; // external-sensor slots span registers 0x20..0x27
/// Start register of the configuration block (§5); config addresses are
/// relative to this base within the parsed register vector.
const CFG_BASE: usize = 0x80;

// --- Realtime register map (block 0x0000, §4). Indices are absolute within the
// parsed vector, which starts at register 0x00. ---

/// First cell-voltage register; cell N (1-based) lives at `REG_CELL_BASE + N-1`.
const REG_CELL_BASE: usize = 0x00;
/// First external-temperature register; sensor N at `REG_TEMP_BASE + N-1`.
const REG_TEMP_BASE: usize = 0x20;
const REG_PACK_VOLTAGE: usize = 0x28;
const REG_CURRENT: usize = 0x29;
const REG_SOC: usize = 0x2A;
const REG_CELL_MAX_V: usize = 0x2B;
const REG_CELL_MIN_V: usize = 0x2C;
const REG_TEMP_MAX: usize = 0x2D;
const REG_TEMP_MIN: usize = 0x2E;
const REG_REMAINING_CAPACITY: usize = 0x30;
const REG_CELL_COUNT: usize = 0x31;
const REG_TEMP_COUNT: usize = 0x32;
const REG_CYCLES: usize = 0x33;
const REG_BALANCER_ACTIVE: usize = 0x34;
const REG_CHARGE_MOS: usize = 0x35;
const REG_DISCHARGE_MOS: usize = 0x36;
const REG_CELL_AVG_V: usize = 0x37;
const REG_CELL_DELTA_V: usize = 0x38;
const REG_ALARM_BITS: usize = 0x3B;
const REG_BALANCE_CURRENT_RT: usize = 0x40;
const REG_BALANCING_CELLS: usize = 0x41;
const REG_MOS_TEMP: usize = 0x42;
const REG_SERIAL_START: usize = 0x57;
const REG_SERIAL_END: usize = 0x5D;

// --- Config register map (block 0x0080, §5). Addresses are absolute (0x80..);
// index into the parsed vector is `addr - CFG_BASE`. ---

const REG_RATED_CAPACITY: usize = 0x80;
const REG_CELL_REFERENCE: usize = 0x81;
const REG_CELL_HIGH_WARN: usize = 0x8B;
const REG_CELL_HIGH_PROT: usize = 0x8C;
const REG_CELL_LOW_WARN: usize = 0x8D;
const REG_CELL_LOW_PROT: usize = 0x8E;
const REG_PACK_HIGH_WARN: usize = 0x8F;
const REG_PACK_HIGH_PROT: usize = 0x90;
const REG_PACK_LOW_WARN: usize = 0x91;
const REG_PACK_LOW_PROT: usize = 0x92;
const REG_DISCHARGE_OC_WARN: usize = 0x93;
const REG_DISCHARGE_OC_PROT: usize = 0x94;
const REG_CHARGE_OC_WARN: usize = 0x95;
const REG_CHARGE_OC_PROT: usize = 0x96;
const REG_CHARGE_TEMP_HIGH_WARN: usize = 0x97;
const REG_CHARGE_TEMP_HIGH_PROT: usize = 0x98;
const REG_CHARGE_TEMP_LOW_WARN: usize = 0x99;
const REG_CHARGE_TEMP_LOW_PROT: usize = 0x9A;
const REG_DISCHARGE_TEMP_HIGH_WARN: usize = 0x9B;
const REG_DISCHARGE_TEMP_HIGH_PROT: usize = 0x9C;
const REG_DIFF_TEMP: usize = 0x9D;
const REG_BALANCE_CURRENT_CFG: usize = 0xA2;
const REG_FAN_ON_TEMP: usize = 0xA8;
const REG_SW_VERSION_START: usize = 0xA9;
const REG_SW_VERSION_END: usize = 0xAF;
const REG_HW_VERSION_START: usize = 0xB1;
const REG_HW_VERSION_END: usize = 0xB7;
const REG_MACHINE_CODE_START: usize = 0xB9;
const REG_MACHINE_CODE_END: usize = 0xC0;
const REG_BALANCE_ENABLE: usize = 0xCF;

/// Which register block a response carries, dispatched from the request's start
/// register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Realtime,
    Config,
}

impl Block {
    /// Dispatch a register block from the request's start register.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::UnknownBlock`] if `start` is not a recognized
    /// block start register (i.e. not `0x0000` realtime or `0x0080` config).
    pub fn from_start_register(start: u16) -> Result<Self, DecodeError> {
        match start {
            0x0000 => Ok(Block::Realtime),
            0x0080 => Ok(Block::Config),
            other => Err(DecodeError::UnknownBlock(other)),
        }
    }
}

// --- Encoding helpers (§4/§5). All widen to i32 before offsetting. ---

/// Current in amperes: `(raw - 30000) * 0.1`. Positive = charge, negative = discharge.
fn amps(raw: u16) -> f64 {
    f64::from(i32::from(raw) - 30000) * 0.1
}
/// Temperature in Celsius: `raw - 40`.
fn celsius(raw: u16) -> f64 {
    f64::from(i32::from(raw) - 40)
}
/// Differential temperature in Celsius: raw value with no -40 offset.
fn celsius_delta(raw: u16) -> f64 {
    f64::from(raw)
}
/// Decivalue: `raw * 0.1` (pack voltage V, SOC %, capacity Ah, balance current A).
fn deci(raw: u16) -> f64 {
    f64::from(raw) * 0.1
}
/// Cell voltage in volts from millivolts.
fn cell_volts(mv: u16) -> f64 {
    f64::from(mv) / 1000.0
}

/// Decode an ASCII field spanning register indices `[start, end]` (inclusive).
fn ascii(regs: &[u16], start: usize, end: usize) -> Option<String> {
    if start >= regs.len() {
        return None;
    }
    let end = end.min(regs.len() - 1);
    let mut bytes = Vec::with_capacity((end - start + 1) * 2);
    for r in &regs[start..=end] {
        bytes.extend_from_slice(&r.to_be_bytes());
    }
    // Drop ALL control bytes (including interior ones like '\n'), not just the
    // leading/trailing run: this value becomes a log field and Prometheus label
    // downstream, where embedded control chars would corrupt output.
    let cleaned: String = String::from_utf8_lossy(&bytes)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A protection threshold stored as a `(warning, protection)` pair (§5.2).
#[derive(Debug, Default, Clone, Copy)]
pub struct Limits {
    /// Warning-level threshold value.
    pub warning: Option<f64>,
    /// Protection-level threshold value.
    pub protection: Option<f64>,
}

/// Realtime telemetry (block 0x0000, §4).
#[derive(Debug, Default)]
pub struct RealtimeData {
    /// `(cell_number, volts)` in V, 1-based, empty slots skipped.
    pub cells_v: Vec<(u32, f64)>,
    /// `(sensor_number, celsius)` in °C, 1-based.
    pub temps_c: Vec<(u32, f64)>,
    /// Pack voltage (V).
    pub pack_v: Option<f64>,
    /// Pack current (A); positive = charge, negative = discharge.
    pub current_a: Option<f64>,
    /// State of charge (%).
    pub soc_pct: Option<f64>,
    /// Highest cell voltage (V).
    pub cell_max_v: Option<f64>,
    /// Lowest cell voltage (V).
    pub cell_min_v: Option<f64>,
    /// Highest sensor temperature (°C).
    pub temp_max_c: Option<f64>,
    /// Lowest sensor temperature (°C).
    pub temp_min_c: Option<f64>,
    /// Remaining capacity (Ah).
    pub remaining_ah: Option<f64>,
    /// Charge/discharge cycle count.
    pub cycles: Option<u16>,
    /// Charge MOSFET state (true = on).
    pub charge_mos: Option<bool>,
    /// Discharge MOSFET state (true = on).
    pub discharge_mos: Option<bool>,
    /// Average cell voltage (V).
    pub cell_avg_v: Option<f64>,
    /// Max−min cell voltage delta (V).
    pub cell_delta_v: Option<f64>,
    /// Whether the balancer is currently active.
    pub balancer_active: Option<bool>,
    /// Balancing current (A).
    pub balance_current_a: Option<f64>,
    /// Number of cells currently being balanced.
    pub balancing_cells: Option<u16>,
    /// MOSFET temperature (°C).
    pub mos_temp_c: Option<f64>,
    /// Raw alarm/protection status bitmask.
    pub alarm_bits: Option<u16>,
    /// Device serial / identifier string.
    pub serial: Option<String>,
}

impl RealtimeData {
    /// Decode a realtime (0x0000) register block. Total: never panics on short
    /// or garbage input — absent registers yield `None`.
    pub fn from_registers(regs: &[u16]) -> Self {
        // `reg` reads by absolute register index into the parsed vector.
        let reg = |addr: usize| regs.get(addr).copied();

        // Counts are untrusted: clamp to the physical maximum before iterating.
        let cell_count = reg(REG_CELL_COUNT).unwrap_or(0).min(MAX_CELLS) as usize;
        let mut cells_v = Vec::new();
        for slot in 0..cell_count {
            if let Some(mv) = reg(REG_CELL_BASE + slot)
                && mv != 0
            {
                cells_v.push((slot as u32 + 1, cell_volts(mv)));
            }
        }

        let temp_count = reg(REG_TEMP_COUNT).unwrap_or(0).min(MAX_TEMPS) as usize;
        let mut temps_c = Vec::new();
        for slot in 0..temp_count {
            if let Some(raw) = reg(REG_TEMP_BASE + slot)
                && raw != 0
            {
                temps_c.push((slot as u32 + 1, celsius(raw)));
            }
        }

        Self {
            cells_v,
            temps_c,
            pack_v: reg(REG_PACK_VOLTAGE).map(deci),
            current_a: reg(REG_CURRENT).map(amps),
            soc_pct: reg(REG_SOC).map(deci),
            cell_max_v: reg(REG_CELL_MAX_V).map(cell_volts),
            cell_min_v: reg(REG_CELL_MIN_V).map(cell_volts),
            temp_max_c: reg(REG_TEMP_MAX).map(celsius),
            temp_min_c: reg(REG_TEMP_MIN).map(celsius),
            remaining_ah: reg(REG_REMAINING_CAPACITY).map(deci),
            cycles: reg(REG_CYCLES),
            charge_mos: reg(REG_CHARGE_MOS).map(|v| v != 0),
            discharge_mos: reg(REG_DISCHARGE_MOS).map(|v| v != 0),
            cell_avg_v: reg(REG_CELL_AVG_V).map(cell_volts),
            cell_delta_v: reg(REG_CELL_DELTA_V).map(cell_volts),
            balancer_active: reg(REG_BALANCER_ACTIVE).map(|v| v != 0),
            balance_current_a: reg(REG_BALANCE_CURRENT_RT).map(amps),
            balancing_cells: reg(REG_BALANCING_CELLS),
            mos_temp_c: reg(REG_MOS_TEMP).map(celsius),
            alarm_bits: reg(REG_ALARM_BITS),
            serial: ascii(regs, REG_SERIAL_START, REG_SERIAL_END),
        }
    }
}

/// Configuration and protection thresholds (block 0x0080, §5).
#[derive(Debug, Default)]
pub struct ConfigData {
    /// Rated pack capacity (Ah).
    pub rated_capacity_ah: Option<f64>,
    /// Nominal/reference cell voltage (V).
    pub cell_reference_v: Option<f64>,
    /// Cell over-voltage warning/protection thresholds (V).
    pub cell_high_v: Limits,
    /// Cell under-voltage warning/protection thresholds (V).
    pub cell_low_v: Limits,
    /// Pack over-voltage warning/protection thresholds (V).
    pub pack_high_v: Limits,
    /// Pack under-voltage warning/protection thresholds (V).
    pub pack_low_v: Limits,
    /// Discharge over-current warning/protection thresholds (A).
    pub discharge_overcurrent_a: Limits,
    /// Charge over-current warning/protection thresholds (A).
    pub charge_overcurrent_a: Limits,
    /// Charge high-temperature warning/protection thresholds (°C).
    pub charge_temp_high_c: Limits,
    /// Charge low-temperature warning/protection thresholds (°C).
    pub charge_temp_low_c: Limits,
    /// Discharge high-temperature warning/protection thresholds (°C).
    pub discharge_temp_high_c: Limits,
    /// Inter-sensor temperature difference limit (°C).
    pub diff_temp_c: Option<f64>,
    /// Fan activation temperature (°C).
    pub fan_on_temp_c: Option<f64>,
    /// Whether cell balancing is enabled.
    pub balance_enable: Option<bool>,
    /// Balancing current setpoint (A).
    pub balance_current_a: Option<f64>,
    /// Device machine code / model identifier string.
    pub machine_code: Option<String>,
    /// Firmware (software) version string.
    pub sw_version: Option<String>,
    /// Hardware version string.
    pub hw_version: Option<String>,
}

impl ConfigData {
    /// Decode a config (0x0080) register block. Total: never panics on short
    /// or garbage input — absent registers yield `None`.
    pub fn from_registers(regs: &[u16]) -> Self {
        // `reg` reads by absolute config address (0x80..); the parsed vector
        // starts at the block base, so index == addr - CFG_BASE.
        let reg = |addr: usize| {
            addr.checked_sub(CFG_BASE)
                .and_then(|i| regs.get(i).copied())
        };
        // `rel_idx` converts an absolute config address to its vector index for
        // the ASCII span helper.
        let rel_idx = |addr: usize| addr - CFG_BASE;

        Self {
            rated_capacity_ah: reg(REG_RATED_CAPACITY).map(deci),
            cell_reference_v: reg(REG_CELL_REFERENCE).map(cell_volts),
            cell_high_v: Limits {
                warning: reg(REG_CELL_HIGH_WARN).map(cell_volts),
                protection: reg(REG_CELL_HIGH_PROT).map(cell_volts),
            },
            cell_low_v: Limits {
                warning: reg(REG_CELL_LOW_WARN).map(cell_volts),
                protection: reg(REG_CELL_LOW_PROT).map(cell_volts),
            },
            pack_high_v: Limits {
                warning: reg(REG_PACK_HIGH_WARN).map(deci),
                protection: reg(REG_PACK_HIGH_PROT).map(deci),
            },
            pack_low_v: Limits {
                warning: reg(REG_PACK_LOW_WARN).map(deci),
                protection: reg(REG_PACK_LOW_PROT).map(deci),
            },
            discharge_overcurrent_a: Limits {
                warning: reg(REG_DISCHARGE_OC_WARN).map(amps),
                protection: reg(REG_DISCHARGE_OC_PROT).map(amps),
            },
            charge_overcurrent_a: Limits {
                warning: reg(REG_CHARGE_OC_WARN).map(amps),
                protection: reg(REG_CHARGE_OC_PROT).map(amps),
            },
            charge_temp_high_c: Limits {
                warning: reg(REG_CHARGE_TEMP_HIGH_WARN).map(celsius),
                protection: reg(REG_CHARGE_TEMP_HIGH_PROT).map(celsius),
            },
            charge_temp_low_c: Limits {
                warning: reg(REG_CHARGE_TEMP_LOW_WARN).map(celsius),
                protection: reg(REG_CHARGE_TEMP_LOW_PROT).map(celsius),
            },
            discharge_temp_high_c: Limits {
                warning: reg(REG_DISCHARGE_TEMP_HIGH_WARN).map(celsius),
                protection: reg(REG_DISCHARGE_TEMP_HIGH_PROT).map(celsius),
            },
            // Differential temperature is a delta with no -40 offset (§5.2).
            diff_temp_c: reg(REG_DIFF_TEMP).map(celsius_delta),
            fan_on_temp_c: reg(REG_FAN_ON_TEMP).map(celsius),
            balance_enable: reg(REG_BALANCE_ENABLE).map(|v| v != 0),
            balance_current_a: reg(REG_BALANCE_CURRENT_CFG).map(deci),
            sw_version: ascii(
                regs,
                rel_idx(REG_SW_VERSION_START),
                rel_idx(REG_SW_VERSION_END),
            ),
            hw_version: ascii(
                regs,
                rel_idx(REG_HW_VERSION_START),
                rel_idx(REG_HW_VERSION_END),
            ),
            machine_code: ascii(
                regs,
                rel_idx(REG_MACHINE_CODE_START),
                rel_idx(REG_MACHINE_CODE_END),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_dispatch() {
        assert_eq!(Block::from_start_register(0x0000).unwrap(), Block::Realtime);
        assert_eq!(Block::from_start_register(0x0080).unwrap(), Block::Config);
        assert!(matches!(
            Block::from_start_register(0x1234),
            Err(DecodeError::UnknownBlock(0x1234))
        ));
    }

    #[test]
    fn encoding_formulas_match_doc_examples() {
        assert!((amps(0x7534) - 0.4).abs() < 1e-9); // 30004 -> +0.4 A
        assert!((amps(0x73F0) - (-32.0)).abs() < 1e-9); // discharge -> negative
        assert_eq!(celsius(0x0035), 13.0);
        assert!((deci(0x010D) - 26.9).abs() < 1e-9);
        assert!((deci(0x034A) - 84.2).abs() < 1e-9);
        assert!((deci(0x0150) - 33.6).abs() < 1e-9);
        assert!((cell_volts(0x0893) - 2.195).abs() < 1e-9);
    }

    #[test]
    fn realtime_short_frame_does_not_panic() {
        let data = RealtimeData::from_registers(&[]);
        assert!(data.pack_v.is_none());
        assert!(data.cells_v.is_empty());
        assert!(data.serial.is_none());
    }

    #[test]
    fn realtime_clamps_untrusted_cell_count() {
        let mut regs = vec![0u16; 0x60];
        regs[0] = 2195; // cell 1
        regs[1] = 2200; // cell 2
        regs[0x31] = 0xFFFF; // garbage cell count -> must clamp, not loop 65535
        let data = RealtimeData::from_registers(&regs);
        assert_eq!(data.cells_v.len(), 2); // only the two non-zero slots
        assert_eq!(data.cells_v[0].0, 1);
    }

    #[test]
    fn config_uses_relative_offsets() {
        let mut regs = vec![0u16; 112]; // one full 0x70-register config block
        regs[0x80 - CFG_BASE] = 0x0190; // rated capacity -> 40.0 Ah
        regs[0xA8 - CFG_BASE] = 0x0057; // fan on -> 47 C
        regs[0xCF - CFG_BASE] = 0x0001; // balance enable
        let cfg = ConfigData::from_registers(&regs);
        assert_eq!(cfg.rated_capacity_ah, Some(40.0));
        assert_eq!(cfg.fan_on_temp_c, Some(47.0));
        assert_eq!(cfg.balance_enable, Some(true));
    }

    #[test]
    fn config_short_frame_does_not_panic() {
        let cfg = ConfigData::from_registers(&[0x0190]);
        assert_eq!(cfg.rated_capacity_ah, Some(40.0));
        assert!(cfg.balance_enable.is_none()); // 0xCF absent -> None
    }
}
