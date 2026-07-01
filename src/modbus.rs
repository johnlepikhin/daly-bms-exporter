//! Low-level Modbus RTU parsing. No domain/block semantics live here — the
//! caller (see [`crate::decode`]) decides what the registers mean.

use crate::error::DecodeError;

/// Slave address of the Daly BMS on the RTU bus.
const BMS_ADDRESS: u8 = 0xD2;
/// Function code 0x03 = Read Holding Registers.
const FUNC_READ_HOLDING: u8 = 0x03;

/// Modbus CRC-16 (polynomial 0xA001 reflected, init 0xFFFF). Transmitted
/// little-endian in the frame.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Parse a function-0x03 response ADU (hex string) into 16-bit big-endian
/// registers, validating address, function, declared byte count and CRC.
///
/// Frame layout: `addr(1) func(1) bytecount(1) <bytecount payload> crc(2 LE)`.
///
/// # Errors
///
/// Returns [`DecodeError::BadHex`] if `hex_str` is not valid hex,
/// [`DecodeError::TooShort`] if the frame is shorter than the minimum ADU,
/// [`DecodeError::BadAddress`] / [`DecodeError::BadFunction`] if the address or
/// function code do not match the BMS read-holding-registers response,
/// [`DecodeError::BadByteCount`] if the declared byte count disagrees with the
/// frame length, and [`DecodeError::CrcMismatch`] if the trailing CRC is wrong.
pub fn parse_response(hex_str: &str) -> Result<Vec<u16>, DecodeError> {
    let raw = hex::decode(hex_str)?;
    // Minimum frame: addr + func + bytecount + 2-byte CRC.
    if raw.len() < 5 {
        return Err(DecodeError::TooShort { len: raw.len() });
    }
    if raw[0] != BMS_ADDRESS {
        return Err(DecodeError::BadAddress(raw[0]));
    }
    if raw[1] != FUNC_READ_HOLDING {
        return Err(DecodeError::BadFunction(raw[1]));
    }
    let nbytes = raw[2] as usize;
    // The frame length must exactly match the declared byte count. This is what
    // guarantees a crafted-but-short frame is rejected before decoding.
    if raw.len() != 3 + nbytes + 2 {
        return Err(DecodeError::BadByteCount {
            declared: nbytes,
            actual: raw.len().saturating_sub(5),
        });
    }
    let crc_pos = 3 + nbytes;
    let expected = u16::from_le_bytes([raw[crc_pos], raw[crc_pos + 1]]);
    let got = crc16(&raw[..crc_pos]);
    if expected != got {
        return Err(DecodeError::CrcMismatch { expected, got });
    }
    let registers = raw[3..crc_pos]
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    Ok(registers)
}

/// Extract the start register of a Modbus request (the `Command` field) so the
/// caller can dispatch the register block. Request layout:
/// `addr func start(2 BE) qty(2 BE) crc(2)`.
///
/// # Errors
///
/// Returns [`DecodeError::BadHex`] if `command_hex` is not valid hex and
/// [`DecodeError::TooShort`] if the frame is shorter than the request layout.
pub fn request_start_register(command_hex: &str) -> Result<u16, DecodeError> {
    let raw = hex::decode(command_hex)?;
    if raw.len() < 6 {
        return Err(DecodeError::TooShort { len: raw.len() });
    }
    Ok(u16::from_be_bytes([raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real request frames from doc/daly-bms-protocol.md §3: the trailing two
    // bytes are the CRC in little-endian, so the CRC value is byte-swapped.
    #[test]
    fn crc16_realtime_request() {
        // D2 03 00 00 00 7E  ->  CRC bytes D6 49  ->  0x49D6
        assert_eq!(crc16(&[0xD2, 0x03, 0x00, 0x00, 0x00, 0x7E]), 0x49D6);
    }

    #[test]
    fn crc16_config_request() {
        // D2 03 00 80 00 70  ->  CRC bytes 56 65  ->  0x6556
        assert_eq!(crc16(&[0xD2, 0x03, 0x00, 0x80, 0x00, 0x70]), 0x6556);
    }

    /// Build a valid response ADU from registers (for round-trip tests).
    pub(crate) fn build_frame(regs: &[u16]) -> String {
        let mut body = vec![0xD2, 0x03, (regs.len() * 2) as u8];
        for r in regs {
            body.extend_from_slice(&r.to_be_bytes());
        }
        let crc = crc16(&body);
        body.extend_from_slice(&crc.to_le_bytes());
        hex::encode(body)
    }

    #[test]
    fn parse_response_round_trip() {
        let regs: Vec<u16> = (0..112).collect();
        let frame = build_frame(&regs);
        assert_eq!(parse_response(&frame).unwrap(), regs);
    }

    #[test]
    fn parse_response_rejects_bad_crc() {
        let mut frame = build_frame(&[1, 2, 3]);
        // Corrupt the last hex nibble (part of the CRC).
        let last = frame.pop().unwrap();
        frame.push(if last == '0' { '1' } else { '0' });
        assert!(matches!(
            parse_response(&frame),
            Err(DecodeError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn parse_response_rejects_short_frame() {
        assert!(matches!(
            parse_response("D203"),
            Err(DecodeError::TooShort { .. })
        ));
    }

    #[test]
    fn parse_response_rejects_bytecount_mismatch() {
        // Declares 0xE0 (224) bytes but supplies far fewer; must not panic.
        assert!(matches!(
            parse_response("D203E00001AABB"),
            Err(DecodeError::BadByteCount { .. })
        ));
    }

    #[test]
    fn start_register_dispatch() {
        assert_eq!(request_start_register("D2030000007ED649").unwrap(), 0x0000);
        assert_eq!(request_start_register("D203008000705665").unwrap(), 0x0080);
    }
}
