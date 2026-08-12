//! Per-register write policy metadata (consumed by D-6 write routing).
//!
//! [`write_policy`] returns the policy table for every register in the
//! catalog; unknown registers default to the safe [`PolicyMode::Unverified`]
//! (read-only by default).

use aa_registers::RegId;

/// Register write-policy mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    /// Writable and readable.
    ReadWrite,
    /// Read-only; writes rejected.
    ReadOnly,
    /// Writable; reads rejected.
    WriteOnly,
    /// Not exposed over the wire.
    Internal,
    /// Behavior not yet verified; read-only by default.
    Unverified,
}

/// Write policy for a single register.
///
/// For [`PolicyMode::ReadWrite`] registers, empty `writable_fields` /
/// `read_only_fields` slices mean "all fields writable" (no field-level
/// restriction). Field names are the JSON DTO field names clients send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePolicy {
    /// Access mode.
    pub mode: PolicyMode,
    /// JSON DTO field names writable by clients (empty = all fields).
    pub writable_fields: &'static [&'static str],
    /// JSON DTO field names exposed read-only (empty = none).
    pub read_only_fields: &'static [&'static str],
}

/// Resolve the write policy for a register id.
#[must_use]
pub const fn write_policy(reg: RegId) -> WritePolicy {
    match reg.get() {
        0x01 | 0x26 | 0x27 => WritePolicy {
            mode: PolicyMode::ReadWrite,
            writable_fields: &[],
            read_only_fields: &[],
        },
        0x02 | 0x06 | 0x08 | 0x0a => WritePolicy {
            mode: PolicyMode::ReadOnly,
            writable_fields: &[],
            read_only_fields: &[],
        },
        0x03 => WritePolicy {
            mode: PolicyMode::ReadWrite,
            writable_fields: &["open", "damper_pct", "target_temp_c"],
            read_only_fields: &["sensor_type", "measured_temp_c"],
        },
        0x04 => WritePolicy {
            mode: PolicyMode::ReadWrite,
            writable_fields: &["min_damper", "max_damper", "motion_config"],
            read_only_fields: &["motion_status", "zone_error", "rssi"],
        },
        0x05 => WritePolicy {
            mode: PolicyMode::ReadWrite,
            writable_fields: &[],
            read_only_fields: &["rf_sys_id"],
        },
        0x07 => WritePolicy {
            mode: PolicyMode::Internal,
            writable_fields: &[],
            read_only_fields: &[],
        },
        0x09 => WritePolicy {
            mode: PolicyMode::WriteOnly,
            writable_fields: &[],
            read_only_fields: &[],
        },
        0x12 => WritePolicy {
            mode: PolicyMode::ReadWrite,
            writable_fields: &["sensor_uid", "zone"],
            read_only_fields: &["pairing", "sensor_rev"],
        },
        _ => WritePolicy {
            mode: PolicyMode::Unverified,
            writable_fields: &[],
            read_only_fields: &[],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{PolicyMode, WritePolicy, write_policy};
    use aa_registers::RegId;

    fn policy(reg: u8) -> WritePolicy {
        write_policy(RegId::new(reg))
    }

    fn assert_policy(reg: u8, mode: PolicyMode, writable: &[&str], read_only: &[&str]) {
        let p = policy(reg);
        assert_eq!(p.mode, mode, "reg {reg:02x} mode");
        assert_eq!(p.writable_fields, writable, "reg {reg:02x} writable_fields");
        assert_eq!(
            p.read_only_fields, read_only,
            "reg {reg:02x} read_only_fields"
        );
    }

    #[test]
    fn full_policy_table() {
        assert_policy(0x01, PolicyMode::ReadWrite, &[], &[]);
        assert_policy(0x02, PolicyMode::ReadOnly, &[], &[]);
        assert_policy(
            0x03,
            PolicyMode::ReadWrite,
            &["open", "damper_pct", "target_temp_c"],
            &["sensor_type", "measured_temp_c"],
        );
        assert_policy(
            0x04,
            PolicyMode::ReadWrite,
            &["min_damper", "max_damper", "motion_config"],
            &["motion_status", "zone_error", "rssi"],
        );
        assert_policy(0x05, PolicyMode::ReadWrite, &[], &["rf_sys_id"]);
        assert_policy(0x06, PolicyMode::ReadOnly, &[], &[]);
        assert_policy(0x07, PolicyMode::Internal, &[], &[]);
        assert_policy(0x08, PolicyMode::ReadOnly, &[], &[]);
        assert_policy(0x09, PolicyMode::WriteOnly, &[], &[]);
        assert_policy(0x0a, PolicyMode::ReadOnly, &[], &[]);
        assert_policy(
            0x12,
            PolicyMode::ReadWrite,
            &["sensor_uid", "zone"],
            &["pairing", "sensor_rev"],
        );
        assert_policy(0x13, PolicyMode::Unverified, &[], &[]);
        assert_policy(0x26, PolicyMode::ReadWrite, &[], &[]);
        assert_policy(0x27, PolicyMode::ReadWrite, &[], &[]);
    }

    #[test]
    fn unknown_registers_default_to_unverified() {
        for reg in [
            0x00u8, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x14, 0x15, 0x16, 0x1e, 0x25, 0x28,
            0x2a, 0xff,
        ] {
            let p = policy(reg);
            assert_eq!(p.mode, PolicyMode::Unverified, "reg {reg:02x} mode");
            assert!(
                p.writable_fields.is_empty(),
                "reg {reg:02x} writable_fields"
            );
            assert!(
                p.read_only_fields.is_empty(),
                "reg {reg:02x} read_only_fields"
            );
        }
    }
}
