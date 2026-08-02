//! Typed decode/encode for known CAN2 registers (`01` / `03` / `05` / `06`).
//!
//! Reg `06` always decodes as [`FirmwareStatus`] (mailbox bytes). Flush is a
//! wire command via [`CanRecord::flush_all`], never a [`DecodedRegister`] variant.
//! Fresh-air mapping follows the `aa_interop` README table (`00`/`01`/`02`).

mod enums;
mod payloads;

pub use enums::{Fan, FreshAir, Mode, Power, SensorType};
pub use payloads::{FirmwareStatus, SystemStatus, ZoneConfig, ZoneState};

use crate::ids::{RegId, UnitId, UnitType};
use crate::wire::{CanRecord, Dest};

/// Typed view of a register payload keyed by [`RegId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecodedRegister {
    /// Reg `01` zone configuration.
    ZoneConfig(ZoneConfig),
    /// Reg `03` zone state.
    ZoneState(ZoneState),
    /// Reg `05` system status.
    SystemStatus(SystemStatus),
    /// Reg `06` firmware status (never a flush command).
    FirmwareStatus(FirmwareStatus),
    /// Any other register id: opaque 7-byte payload.
    Unknown {
        /// Register identifier.
        reg: RegId,
        /// Opaque payload.
        data: [u8; 7],
    },
}

impl DecodedRegister {
    /// Decode `(reg, data)` without wire destination.
    ///
    /// Reg `06` always yields [`Self::FirmwareStatus`].
    #[must_use]
    pub fn from_reg_data(reg: RegId, data: [u8; 7]) -> Self {
        match reg.get() {
            0x01 => Self::ZoneConfig(ZoneConfig::from(data)),
            0x03 => Self::ZoneState(ZoneState::from(data)),
            0x05 => Self::SystemStatus(SystemStatus::from(data)),
            0x06 => Self::FirmwareStatus(FirmwareStatus::from(data)),
            _ => Self::Unknown { reg, data },
        }
    }

    /// Register id for this decoded value.
    #[must_use]
    pub const fn reg_id(self) -> RegId {
        match self {
            Self::ZoneConfig(_) => RegId::new(0x01),
            Self::ZoneState(_) => RegId::new(0x03),
            Self::SystemStatus(_) => RegId::new(0x05),
            Self::FirmwareStatus(_) => RegId::new(0x06),
            Self::Unknown { reg, .. } => reg,
        }
    }

    /// Encode back to a 7-byte payload.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 7] {
        match self {
            Self::ZoneConfig(v) => v.into(),
            Self::ZoneState(v) => v.into(),
            Self::SystemStatus(v) => v.into(),
            Self::FirmwareStatus(v) => v.into(),
            Self::Unknown { data, .. } => data,
        }
    }
}

impl CanRecord {
    /// Decode this record's `(reg, data)` into a [`DecodedRegister`].
    ///
    /// Reg `06` is always [`DecodedRegister::FirmwareStatus`] (never a flush
    /// variant). Use [`Self::is_flush`] / [`Self::flush_all`] for flush commands.
    #[must_use]
    pub fn decode(&self) -> DecodedRegister {
        DecodedRegister::from_reg_data(self.reg, self.data)
    }

    /// Build a tablet→CB flush-all command: aircon unit type, [`Dest::ControlBox`] dest,
    /// unit id `0`, reg `06`, all-zero data.
    #[must_use]
    pub const fn flush_all() -> Self {
        Self {
            unit_type: UnitType::AIRCON,
            dest: Dest::ControlBox,
            unit_id: UnitId::ZERO,
            reg: RegId::new(0x06),
            data: [0; 7],
        }
    }

    /// `true` when this record matches [`Self::flush_all`] (aircon unit type,
    /// [`Dest::ControlBox`], unit id 0, reg `06`, all-zero payload).
    #[must_use]
    pub fn is_flush(&self) -> bool {
        self == &Self::flush_all()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bank::RegisterBank;
    use crate::wire::CanRecord;

    #[test]
    fn zone_config_round_trip() {
        let original = ZoneConfig {
            header: 0x20,
            num_zones: 3,
            num_constant: 1,
            constant: [0x01, 0x00, 0x00],
            filter_clean: false,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x20, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00]);
        let decoded = ZoneConfig::from(bytes);
        assert_eq!(decoded, original);

        let record = CanRecord::parse_one("0703abcde0120030101000000").unwrap();
        assert_eq!(record.decode(), DecodedRegister::ZoneConfig(original));
        assert_eq!(
            DecodedRegister::ZoneConfig(original).to_bytes(),
            record.data
        );
        assert_eq!(
            DecodedRegister::ZoneConfig(original).reg_id(),
            RegId::new(0x01)
        );
    }

    #[test]
    fn zone_state_round_trip_and_packing() {
        // open + 100% => 0xe4; set_temp_x2 = 0x30 => 24.0°C
        let original = ZoneState {
            zone: 0x01,
            open: true,
            percent: 100,
            sensor: SensorType::NoSensor,
            set_temp_x2: 0x30,
            meas_int: 0,
            meas_dec: 0,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0xe4);
        assert_eq!(bytes[3], 0x30);
        assert_eq!(ZoneState::from(bytes), original);

        // Closed + 50%
        let closed = ZoneState {
            zone: 0x02,
            open: false,
            percent: 50,
            sensor: SensorType::Rf,
            set_temp_x2: 0x2a,
            meas_int: 0x18,
            meas_dec: 0x02,
        };
        let b: [u8; 7] = closed.into();
        assert_eq!(b[1], 50);
        assert_eq!(ZoneState::from(b), closed);

        let record = CanRecord::parse_one("0703abcde0301e40030000000").unwrap();
        let DecodedRegister::ZoneState(zs) = record.decode() else {
            panic!("expected ZoneState");
        };
        assert!(zs.open);
        assert_eq!(zs.percent, 100);
        assert_eq!(zs.zone, 0x01);
        assert_eq!(zs.set_temp_x2, 0x30);
        assert_eq!(zs.sensor, SensorType::NoSensor);
    }

    #[test]
    fn system_status_round_trip_and_fresh_air() {
        let original = SystemStatus {
            power: Power::On,
            mode: Mode::Cool,
            fan: Fan::High,
            set_temp_x2: 0x30,
            myzone_id: 0,
            fresh_air: FreshAir::Off,
            rf_sys_id: 0,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00]);
        assert_eq!(SystemStatus::from(bytes), original);

        assert_eq!(FreshAir::from_u8(0x00), FreshAir::None);
        assert_eq!(FreshAir::from_u8(0x01), FreshAir::Off);
        assert_eq!(FreshAir::from_u8(0x02), FreshAir::On);
        assert_eq!(FreshAir::On.to_u8(), 0x02);

        let record = CanRecord::parse_one("0703abcde0501010330000100").unwrap();
        assert_eq!(record.decode(), DecodedRegister::SystemStatus(original));
    }

    #[test]
    fn firmware_status_round_trip_never_flush_variant() {
        let original = FirmwareStatus {
            fw_major: 0x01,
            fw_minor: 0x02,
            cb_type: 0x03,
            rf_fw_major: 0x00,
        };
        let bytes: [u8; 7] = original.into();
        assert_eq!(bytes, [0x01, 0x02, 0x03, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(FirmwareStatus::from(bytes), original);

        let record = CanRecord::parse_one("0703abcde0601020300000000").unwrap();
        assert_eq!(record.decode(), DecodedRegister::FirmwareStatus(original));
        // All-zero reg 06 still decodes as FirmwareStatus, not a Flush variant.
        let zeros = CanRecord::flush_all();
        assert!(matches!(
            zeros.decode(),
            DecodedRegister::FirmwareStatus(FirmwareStatus {
                fw_major: 0,
                fw_minor: 0,
                cb_type: 0,
                rf_fw_major: 0,
            })
        ));
    }

    #[test]
    fn unknown_enum_passthrough() {
        assert_eq!(Power::from_u8(0xfe), Power::Unknown(0xfe));
        assert_eq!(Power::Unknown(0xfe).to_u8(), 0xfe);
        assert_eq!(Mode::from_u8(0xfe), Mode::Unknown(0xfe));
        assert_eq!(Mode::Unknown(0xfe).to_u8(), 0xfe);
        assert_eq!(Fan::from_u8(0xfe), Fan::Unknown(0xfe));
        assert_eq!(Fan::Unknown(0xfe).to_u8(), 0xfe);
        assert_eq!(FreshAir::from_u8(0xfe), FreshAir::Unknown(0xfe));
        assert_eq!(FreshAir::Unknown(0xfe).to_u8(), 0xfe);
        assert_eq!(SensorType::from_u8(0xfe), SensorType::Unknown(0xfe));
        assert_eq!(SensorType::Unknown(0xfe).to_u8(), 0xfe);

        let status = SystemStatus {
            power: Power::Unknown(0xfe),
            mode: Mode::Unknown(0xfe),
            fan: Fan::Unknown(0xfe),
            set_temp_x2: 0x20,
            myzone_id: 1,
            fresh_air: FreshAir::Unknown(0xfe),
            rf_sys_id: 0,
        };
        let bytes: [u8; 7] = status.into();
        assert_eq!(bytes[0], 0xfe);
        assert_eq!(bytes[1], 0xfe);
        assert_eq!(bytes[2], 0xfe);
        assert_eq!(bytes[5], 0xfe);
        assert_eq!(SystemStatus::from(bytes), status);
    }

    #[test]
    fn flush_all_and_is_flush() {
        let flush = CanRecord::flush_all();
        assert_eq!(flush.to_wire(), "0701000000600000000000000");
        assert!(flush.is_flush());
        assert_eq!(flush.unit_type, UnitType::AIRCON);
        assert_eq!(flush.dest, Dest::ControlBox);
        assert_eq!(flush.unit_id.get(), 0);
        assert_eq!(flush.reg.get(), 0x06);
        assert_eq!(flush.data, [0; 7]);

        let firmware = CanRecord::parse_one("0703abcde0601020300000000").unwrap();
        assert!(!firmware.is_flush());
        let almost = CanRecord::parse_one("0701000000600000000000001").unwrap();
        assert!(!almost.is_flush());

        // Same shape but non-aircon unit type must not be flush (aligned with flush_all).
        let other_type = CanRecord {
            unit_type: UnitType::new(0x02),
            dest: Dest::ControlBox,
            unit_id: UnitId::ZERO,
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        assert!(!other_type.is_flush());
    }

    #[test]
    fn unknown_register_passthrough() {
        let record = CanRecord::parse_one("0703abcde0a00000000000000").unwrap();
        let decoded = record.decode();
        assert_eq!(
            decoded,
            DecodedRegister::Unknown {
                reg: RegId::new(0x0a),
                data: [0; 7],
            }
        );
        assert_eq!(decoded.reg_id(), RegId::new(0x0a));
        assert_eq!(decoded.to_bytes(), [0; 7]);
    }

    #[test]
    fn getcan_fragment_applies_into_bank() {
        // README-style initial dump fragment (subset of known regs).
        let fragment = "\
1 \
0703abcde0120030101000000 \
0703abcde0501010330000100 \
0703abcde0301e40030000000 \
0703abcde0302640030000000 \
0703abcde0601020300000000";
        let records = CanRecord::parse_many(fragment).unwrap();
        let mut bank = RegisterBank::new();
        for r in &records {
            bank.apply(r);
        }

        let unit_type = UnitType::AIRCON;
        let unit_id = UnitId::from_hex("abcde").unwrap();

        let cfg = bank
            .get_decoded(unit_type, unit_id, RegId::new(0x01))
            .expect("zone config");
        assert!(matches!(
            cfg,
            DecodedRegister::ZoneConfig(ZoneConfig {
                header: 0x20,
                num_zones: 3,
                num_constant: 1,
                ..
            })
        ));

        let sys = bank
            .get_decoded(unit_type, unit_id, RegId::new(0x05))
            .expect("system status");
        assert_eq!(
            sys,
            DecodedRegister::SystemStatus(SystemStatus {
                power: Power::On,
                mode: Mode::Cool,
                fan: Fan::High,
                set_temp_x2: 0x30,
                myzone_id: 0,
                fresh_air: FreshAir::Off,
                rf_sys_id: 0,
            })
        );

        let z1 = bank
            .get_zone_decoded(unit_type, unit_id, RegId::new(0x03), 0x01)
            .expect("zone 1");
        let DecodedRegister::ZoneState(zs1) = z1 else {
            panic!("expected ZoneState");
        };
        assert_eq!(zs1.zone, 0x01);
        assert!(zs1.open);
        assert_eq!(zs1.percent, 100);

        let z2 = bank
            .get_zone_decoded(unit_type, unit_id, RegId::new(0x03), 0x02)
            .expect("zone 2");
        let DecodedRegister::ZoneState(zs2) = z2 else {
            panic!("expected ZoneState");
        };
        assert_eq!(zs2.zone, 0x02);
        assert!(!zs2.open);
        assert_eq!(zs2.percent, 100);

        let fw = bank
            .get_decoded(unit_type, unit_id, RegId::new(0x06))
            .expect("firmware");
        assert!(matches!(fw, DecodedRegister::FirmwareStatus(_)));
    }

    #[test]
    fn from_bytes_structs() {
        let data = [0x20_u8, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00];
        let cfg = ZoneConfig::from(data);
        assert_eq!(cfg.num_zones, 3);
        let back: [u8; 7] = cfg.into();
        assert_eq!(back, data);
    }
}
