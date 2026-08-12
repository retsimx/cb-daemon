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
