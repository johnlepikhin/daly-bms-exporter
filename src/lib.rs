//! Daly BMS exporter library.
//!
//! Receives raw Modbus frames forwarded from the Hlktech WiFi module, decodes
//! them into typed telemetry and exposes Prometheus metrics. See
//! `doc/daly-bms-protocol.md` for the wire protocol.

#![forbid(unsafe_code)]

pub mod config;
pub mod decode;
pub mod error;
pub mod metrics;
pub mod modbus;
pub mod payload;
pub mod server;
