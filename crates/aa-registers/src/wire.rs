//! CAN2 register wire codec: parse and build fixed-width hex records.
//!
//! Encode helpers (`push_hex_*`) live here. Decode reuses [`crate::ids`] hex
//! parsing. Elsewhere prefer [`UnitType`] / [`UnitId`] / [`RegId`] and
//! structured payloads over raw hex.

use crate::ids::{IdError, RegId, UnitId, UnitType};

/// Fixed hex length of one CAN2 register record (no spaces).
pub const RECORD_HEX_LEN: usize = 25;

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

/// Destination / direction byte on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dest {
    /// Destined for the Control Box (`0x01`).
    ControlBox,
    /// Destined for the Tablet (`0x03`).
    Tablet,
    /// Any other destination byte (round-trips unchanged).
    Unknown(u8),
}

impl Dest {
    /// Map a raw destination byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x01 => Self::ControlBox,
            0x03 => Self::Tablet,
            other => Self::Unknown(other),
        }
    }

    /// Raw destination byte used on the wire.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::ControlBox => 0x01,
            Self::Tablet => 0x03,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for Dest {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<Dest> for u8 {
    fn from(value: Dest) -> Self {
        value.to_u8()
    }
}

/// One CAN2 register record (unit type, dest, unit id, register, 7-byte data).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanRecord {
    /// Top-level unit type (e.g. aircon `0x07`).
    pub unit_type: UnitType,
    /// Wire destination / direction.
    pub dest: Dest,
    /// 20-bit unit identifier.
    pub unit_id: UnitId,
    /// Register identifier.
    pub reg: RegId,
    /// Opaque 7-byte register payload.
    pub data: [u8; 7],
}

impl CanRecord {
    /// Encode as a lowercase hex string with no spaces (fixed 25 chars).
    #[must_use]
    pub fn to_wire(&self) -> String {
        let mut out = String::with_capacity(RECORD_HEX_LEN);
        push_hex_u8(&mut out, self.unit_type.get());
        push_hex_u8(&mut out, self.dest.to_u8());
        // Unit id is always 5 lowercase hex digits (20-bit).
        push_hex_nibble(&mut out, ((self.unit_id.get() >> 16) & 0x0f) as u8);
        push_hex_u8(&mut out, ((self.unit_id.get() >> 8) & 0xff) as u8);
        push_hex_u8(&mut out, (self.unit_id.get() & 0xff) as u8);
        push_hex_u8(&mut out, self.reg.get());
        for &b in &self.data {
            push_hex_u8(&mut out, b);
        }
        debug_assert_eq!(out.len(), RECORD_HEX_LEN);
        out
    }

    /// Parse one fixed-width hex record (no surrounding `setCAN` / `getCAN`).
    ///
    /// # Errors
    ///
    /// - [`WireError::Incomplete`] if `s` is empty or not exactly 25 hex chars
    /// - [`WireError::BadHex`] if any character is not a hex digit
    /// - [`WireError::InvalidId`] if a field fails domain validation
    pub fn parse_one(s: &str) -> Result<Self, WireError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(WireError::Incomplete);
        }
        if s.len() != RECORD_HEX_LEN {
            return Err(WireError::Incomplete);
        }
        if !s.bytes().all(is_hex_digit) {
            return Err(WireError::BadHex);
        }

        let unit_type = UnitType::from_hex(&s[0..2]).map_err(WireError::from)?;
        let dest = Dest::from_u8(parse_hex_byte(&s[2..4])?);
        let unit_id = UnitId::from_hex(&s[4..9]).map_err(WireError::from)?;
        let reg = RegId::from_hex(&s[9..11]).map_err(WireError::from)?;

        let mut data = [0u8; 7];
        for (i, slot) in data.iter_mut().enumerate() {
            let start = 11 + i * 2;
            *slot = parse_hex_byte(&s[start..start + 2])?;
        }

        Ok(Self {
            unit_type,
            dest,
            unit_id,
            reg,
            data,
        })
    }

    /// Parse whitespace-separated records; skip lone `"1"` tokens (getCAN marker).
    ///
    /// Fail-fast: the first malformed non-`"1"` token returns an error.
    ///
    /// # Errors
    ///
    /// Propagates [`CanRecord::parse_one`] errors for any bad token.
    pub fn parse_many(s: &str) -> Result<Vec<Self>, WireError> {
        let mut out = Vec::new();
        for token in s.split_whitespace() {
            if token == "1" {
                continue;
            }
            out.push(Self::parse_one(token)?);
        }
        Ok(out)
    }
}

/// Errors produced while parsing CAN2 hex records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Input ended early or length is not exactly one record.
    Incomplete,
    /// A character was not a valid hexadecimal digit.
    BadHex,
    /// An identifier field failed domain validation.
    InvalidId,
}

impl From<IdError> for WireError {
    fn from(value: IdError) -> Self {
        match value {
            IdError::BadHex => Self::BadHex,
            IdError::OutOfRange => Self::InvalidId,
        }
    }
}

fn push_hex_u8(out: &mut String, value: u8) {
    push_hex_nibble(out, value >> 4);
    push_hex_nibble(out, value & 0x0f);
}

fn push_hex_nibble(out: &mut String, nibble: u8) {
    out.push(HEX_LOWER[usize::from(nibble & 0x0f)] as char);
}

fn parse_hex_byte(s: &str) -> Result<u8, WireError> {
    crate::ids::parse_hex_u8(s).map_err(WireError::from)
}

const fn is_hex_digit(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn assert_round_trip(wire: &str) {
        let parsed = CanRecord::parse_one(wire).expect("parse_one");
        assert_eq!(parsed.to_wire(), wire.to_ascii_lowercase());
        let again = CanRecord::parse_one(&parsed.to_wire()).unwrap();
        assert_eq!(again, parsed);
    }

    #[test]
    fn round_trip_reg_01_zone_config() {
        // CB → tablet zone config from aa_interop README.
        assert_round_trip("0703abcde0120030101000000");
    }

    #[test]
    fn round_trip_reg_03_zone_state() {
        assert_round_trip("0703abcde0301e40030000000");
    }

    #[test]
    fn round_trip_reg_05_system_status() {
        assert_round_trip("0703abcde0501010330000100");
    }

    #[test]
    fn round_trip_reg_06_flush() {
        // Tablet → CB flush-all (unit id zero, all-zero payload).
        assert_round_trip("0701000000600000000000000");
    }

    #[test]
    fn round_trip_reg_06_firmware_style() {
        // CB → tablet firmware/status style payload on reg 06.
        assert_round_trip("0703abcde0601020300000000");
    }

    #[test]
    fn parse_many_skips_lone_one() {
        let input = "1 0703abcde0120030101000000 0703abcde0501010330000100";
        let records = CanRecord::parse_many(input).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].reg.get(), 0x01);
        assert_eq!(records[1].reg.get(), 0x05);
    }

    #[test]
    fn parse_many_fails_fast_on_malformed() {
        let input = "0703abcde0120030101000000 badtoken 0703abcde0501010330000100";
        let err = CanRecord::parse_many(input).unwrap_err();
        assert!(matches!(
            err,
            WireError::Incomplete | WireError::BadHex | WireError::InvalidId
        ));
    }

    #[test]
    fn edge_bad_length() {
        assert_eq!(
            CanRecord::parse_one("0703abcde012003010100000"),
            Err(WireError::Incomplete)
        );
        assert_eq!(
            CanRecord::parse_one("0703abcde01200301010000000"),
            Err(WireError::Incomplete)
        );
    }

    #[test]
    fn edge_non_hex() {
        assert_eq!(
            CanRecord::parse_one("0703abcde01200301010000gg"),
            Err(WireError::BadHex)
        );
    }

    #[test]
    fn edge_empty() {
        assert_eq!(CanRecord::parse_one(""), Err(WireError::Incomplete));
        assert_eq!(CanRecord::parse_one("   "), Err(WireError::Incomplete));
        assert!(CanRecord::parse_many("").unwrap().is_empty());
        assert!(CanRecord::parse_many("   ").unwrap().is_empty());
    }

    #[test]
    fn edge_one_only() {
        assert!(CanRecord::parse_many("1").unwrap().is_empty());
        assert!(CanRecord::parse_many("1 1 1").unwrap().is_empty());
    }

    #[test]
    fn dest_unknown_round_trip() {
        let wire = "0702abcde0501010330000100";
        let parsed = CanRecord::parse_one(wire).unwrap();
        assert_eq!(parsed.dest, Dest::Unknown(0x02));
        assert_eq!(parsed.to_wire(), wire);
        assert_eq!(CanRecord::parse_one(&parsed.to_wire()).unwrap(), parsed);
    }

    #[test]
    fn dest_known_variants() {
        let cb = CanRecord::parse_one("0701000000600000000000000").unwrap();
        assert_eq!(cb.dest, Dest::ControlBox);
        let tab = CanRecord::parse_one("0703abcde0120030101000000").unwrap();
        assert_eq!(tab.dest, Dest::Tablet);
    }
}
