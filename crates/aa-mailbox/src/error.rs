//! Encode errors for mailbox JSON ↔ register conversions.

use core::fmt;

/// Error encoding JSON / DTO input into wire [`aa_registers::CanRecord`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// Register key is not supported for writes in this crate revision.
    UnsupportedRegister(String),
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
            Self::UnsupportedRegister(reg) => {
                write!(f, "unsupported register for mailbox update: {reg}")
            }
            Self::BadPayload(msg) => write!(f, "bad mailbox update payload: {msg}"),
            Self::BadEnum { field, value } => {
                write!(f, "unsupported {field} value: {value}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}
