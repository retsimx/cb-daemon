//! CRC-8 helpers for Android Auto protocol framing.
//!
//! Algorithm: init `0x00`, poly `0xB2`, xorout `0xFF`, reflected (LSB-first /
//! right-shift; XOR poly when LSB is set).

/// CRC-8: init 0x00, poly 0xB2, xorout 0xFF, reflected (LSB-first / right-shift;
/// XOR poly when LSB set).
#[must_use]
pub fn crc8(data: &[u8]) -> u8 {
    const POLY: u8 = 0xB2;
    const XOROUT: u8 = 0xFF;

    let mut crc: u8 = 0x00;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x01 != 0 {
                crc = (crc >> 1) ^ POLY;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ XOROUT
}

#[cfg(test)]
mod tests {
    use super::crc8;

    #[test]
    fn golden_ping() {
        assert_eq!(crc8(b"Ping"), 0xdb);
    }

    #[test]
    fn golden_set_can() {
        assert_eq!(crc8(b"setCAN "), 0xb2);
    }

    #[test]
    fn golden_ack_can() {
        assert_eq!(crc8(b"ackCAN 1"), 0xaa);
    }

    #[test]
    fn golden_get_system_data() {
        assert_eq!(crc8(b"getSystemData"), 0x15);
    }
}
