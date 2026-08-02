//! Typed identifiers for CAN2 register records.
//!
//! Hex formatting and parsing live here so callers outside the wire edge can
//! work with structured values instead of raw `u8` / hex strings.

use core::fmt;

/// Maximum inclusive value for a 20-bit unit id (`0xFFFFF`).
pub const UNIT_ID_MAX: u32 = 0x000F_FFFF;

/// Error returned when an identifier cannot be constructed from a raw value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    /// Value exceeds the domain (e.g. unit id above 20 bits).
    OutOfRange,
    /// Hex string has the wrong length or non-hex digits.
    BadHex,
}

/// Top-level unit type byte (e.g. aircon [`UnitType::AIRCON`], lights `0x02`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnitType(u8);

impl UnitType {
    /// Aircon / HVAC unit type on the wire (`0x07`).
    pub const AIRCON: Self = Self(0x07);

    /// Wrap a raw unit-type byte.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Borrow the raw byte.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Parse a fixed-width 2-digit lowercase/uppercase hex byte.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::BadHex`] if `s` is not exactly two hex digits.
    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        Ok(Self(parse_hex_u8(s)?))
    }
}

impl From<u8> for UnitType {
    fn from(value: u8) -> Self {
        Self(value)
    }
}

impl From<UnitType> for u8 {
    fn from(value: UnitType) -> Self {
        value.0
    }
}

impl fmt::Display for UnitType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}", self.0)
    }
}

/// 20-bit control-box / unit identifier (`UUUUU` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnitId(u32);

impl UnitId {
    /// Zero unit id (flush-all / broadcast addressing).
    pub const ZERO: Self = Self(0);

    /// Wrap a raw 20-bit unit id.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::OutOfRange`] if `value` exceeds [`UNIT_ID_MAX`].
    pub const fn try_new(value: u32) -> Result<Self, IdError> {
        if value > UNIT_ID_MAX {
            Err(IdError::OutOfRange)
        } else {
            Ok(Self(value))
        }
    }

    /// Borrow the raw 20-bit value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Parse a fixed-width 5-digit hex unit id.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::BadHex`] if `s` is not exactly five hex digits, or
    /// [`IdError::OutOfRange`] if the decoded value exceeds 20 bits (unreachable
    /// for a well-formed 5-nibble string).
    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        if s.len() != 5 {
            return Err(IdError::BadHex);
        }
        let mut value: u32 = 0;
        for b in s.bytes() {
            value = (value << 4) | u32::from(hex_nibble(b)?);
        }
        Self::try_new(value)
    }
}

impl TryFrom<u32> for UnitId {
    type Error = IdError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<UnitId> for u32 {
    fn from(value: UnitId) -> Self {
        value.0
    }
}

impl fmt::Display for UnitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:05x}", self.0)
    }
}

/// Register identifier byte (`RR` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegId(u8);

impl RegId {
    /// Wrap a raw register id byte.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Borrow the raw byte.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Parse a fixed-width 2-digit hex register id.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::BadHex`] if `s` is not exactly two hex digits.
    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        Ok(Self(parse_hex_u8(s)?))
    }
}

impl From<u8> for RegId {
    fn from(value: u8) -> Self {
        Self(value)
    }
}

impl From<RegId> for u8 {
    fn from(value: RegId) -> Self {
        value.0
    }
}

impl fmt::Display for RegId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}", self.0)
    }
}

/// Parse exactly two hex digits into a byte (shared with [`crate::wire`]).
pub(crate) fn parse_hex_u8(s: &str) -> Result<u8, IdError> {
    if s.len() != 2 {
        return Err(IdError::BadHex);
    }
    let bytes = s.as_bytes();
    Ok((hex_nibble(bytes[0])? << 4) | hex_nibble(bytes[1])?)
}

pub(crate) const fn hex_nibble(b: u8) -> Result<u8, IdError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(IdError::BadHex),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn unit_id_round_trip_hex() {
        let id = UnitId::from_hex("abcde").unwrap();
        assert_eq!(id.get(), 0x0_ABCDE);
        assert_eq!(id.to_string(), "abcde");
    }

    #[test]
    fn unit_id_rejects_over_20_bits() {
        assert_eq!(UnitId::try_from(0x0010_0000), Err(IdError::OutOfRange));
    }

    #[test]
    fn unit_type_and_reg_display() {
        assert_eq!(UnitType::new(0x07).to_string(), "07");
        assert_eq!(RegId::new(0x0a).to_string(), "0a");
    }
}
