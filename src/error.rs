//! Modbus frame decoding errors.

use thiserror::Error;

/// Error decoding a Modbus RTU frame from the device. Every variant is a
/// recoverable, per-frame condition: the caller logs and skips the frame, never
/// aborting the whole request (the body is untrusted device input).
#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("invalid hex: {0}")]
    BadHex(#[from] hex::FromHexError),

    #[error("frame too short: {len} bytes")]
    TooShort { len: usize },

    #[error("unexpected Modbus address 0x{0:02X}, want 0xD2")]
    BadAddress(u8),

    #[error("unexpected Modbus function 0x{0:02X}, want 0x03")]
    BadFunction(u8),

    #[error(
        "declared byte count does not match frame length: declared {declared}, payload {actual}"
    )]
    BadByteCount { declared: usize, actual: usize },

    #[error("CRC mismatch: expected 0x{expected:04X}, computed 0x{got:04X}")]
    CrcMismatch { expected: u16, got: u16 },

    #[error("unknown register block, start register 0x{0:04X}")]
    UnknownBlock(u16),
}
