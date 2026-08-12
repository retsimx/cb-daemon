//! Encode errors for mailbox JSON ↔ register conversions.

use core::fmt;

/// Error encoding JSON / DTO input into wire [`aa_registers::CanRecord`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// Register id is not in the catalog.
    UnknownRegister(String),
    /// Raw-hex payload is malformed.
    BadHex(String),
    /// Payload JSON could not be deserialized into the expected DTO.
    BadPayload(String),
    /// Enum / string field value is not recognised.
    BadEnum {
        /// Field name (e.g. `power`, `mode`).
        field: &'static str,
        /// Offending value.
        value: String,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRegister(reg) => write!(f, "unknown register in catalog: {reg}"),
            Self::BadHex(hex) => write!(f, "malformed raw-hex payload: {hex}"),
            Self::BadPayload(msg) => write!(f, "bad register payload: {msg}"),
            Self::BadEnum { field, value } => {
                write!(f, "unsupported {field} value: {value}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Errors enforcing the D-4 write policy on register writes (issue #32).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// Whole register is read-only; writes rejected.
    ReadOnlyRegister {
        /// Register id.
        reg: u8,
    },
    /// A typed payload carries a field exposed read-only.
    ReadOnlyField {
        /// Register id.
        reg: u8,
        /// JSON DTO field name.
        field: &'static str,
    },
    /// Whole register is write-only (writable; reads rejected).
    WriteOnlyRegister {
        /// Register id.
        reg: u8,
    },
    /// Register is handled internally, not exposed over the wire.
    InternalRegister {
        /// Register id.
        reg: u8,
    },
    /// Register behaviour is not yet verified; writes rejected.
    UnverifiedRegister {
        /// Register id.
        reg: u8,
    },
    /// A wire field value is above its allowed maximum.
    OutOfRange {
        /// Display field name (issue #32).
        field: &'static str,
        /// Offending wire value.
        value: u8,
        /// Inclusive maximum.
        max: u8,
    },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadOnlyRegister { reg } => write!(f, "register {reg:02x} is read-only"),
            Self::ReadOnlyField { reg, field } => {
                write!(f, "field '{field}' is read-only on register {reg:02x}")
            }
            Self::WriteOnlyRegister { reg } => write!(f, "register {reg:02x} is write-only"),
            Self::InternalRegister { reg } => write!(f, "register {reg:02x} is handled internally"),
            Self::UnverifiedRegister { reg } => {
                write!(f, "register {reg:02x} is unverified; writes not permitted")
            }
            Self::OutOfRange { field, value, max } => {
                write!(f, "field '{field}' {value} out of range (max {max})")
            }
        }
    }
}

impl std::error::Error for WriteError {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::WriteError;

    #[test]
    fn display_read_only_register() {
        let err = WriteError::ReadOnlyRegister { reg: 0x08 };
        assert_eq!(err.to_string(), "register 08 is read-only");
    }

    #[test]
    fn display_read_only_field() {
        let err = WriteError::ReadOnlyField {
            reg: 0x03,
            field: "measured_temp_c",
        };
        assert_eq!(
            err.to_string(),
            "field 'measured_temp_c' is read-only on register 03"
        );
    }

    #[test]
    fn display_write_only_register() {
        let err = WriteError::WriteOnlyRegister { reg: 0x09 };
        assert_eq!(err.to_string(), "register 09 is write-only");
    }

    #[test]
    fn display_internal_register() {
        let err = WriteError::InternalRegister { reg: 0x07 };
        assert_eq!(err.to_string(), "register 07 is handled internally");
    }

    #[test]
    fn display_unverified_register() {
        let err = WriteError::UnverifiedRegister { reg: 0x1e };
        assert_eq!(
            err.to_string(),
            "register 1e is unverified; writes not permitted"
        );
    }

    #[test]
    fn display_out_of_range() {
        let err = WriteError::OutOfRange {
            field: "damper",
            value: 150,
            max: 100,
        };
        assert_eq!(err.to_string(), "field 'damper' 150 out of range (max 100)");
    }
}
