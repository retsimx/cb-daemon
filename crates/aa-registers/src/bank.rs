//! In-memory mailbox bank for CAN2 register payloads.
//!
//! Keys are `(unit_type, unit_id, reg)` for ordinary registers, and the same
//! triple plus `data[0]` as zone for zone-bearing registers (`0x03`, `0x04`).
//! Wire `dest` is never part of mailbox identity.

use std::collections::HashMap;

use crate::ids::{RegId, UnitId, UnitType};
use crate::typed::DecodedRegister;
use crate::wire::CanRecord;

/// Register ids whose bank key includes zone byte `data[0]`.
pub const ZONE_BEARING_REGS: &[u8] = &[0x03, 0x04];

/// Returns `true` if `reg` is in the fixed zone-bearing set.
#[must_use]
pub const fn is_zone_bearing(reg: RegId) -> bool {
    matches!(reg.get(), 0x03 | 0x04)
}

/// Mailbox identity: unit + register, optionally zone for zone-bearing regs.
///
/// Does not include wire destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BankKey {
    /// Top-level unit type.
    pub unit_type: UnitType,
    /// 20-bit unit identifier.
    pub unit_id: UnitId,
    /// Register identifier.
    pub reg: RegId,
    /// Zone from `data[0]` when [`is_zone_bearing`]; otherwise `None`.
    pub zone: Option<u8>,
}

impl BankKey {
    /// Build a key from a wire record (ignores `dest`).
    #[must_use]
    pub const fn from_record(record: &CanRecord) -> Self {
        let zone = if is_zone_bearing(record.reg) {
            Some(record.data[0])
        } else {
            None
        };
        Self {
            unit_type: record.unit_type,
            unit_id: record.unit_id,
            reg: record.reg,
            zone,
        }
    }

    /// Non-zone key `(unit_type, unit_id, reg)`.
    #[must_use]
    pub const fn non_zone(unit_type: UnitType, unit_id: UnitId, reg: RegId) -> Self {
        Self {
            unit_type,
            unit_id,
            reg,
            zone: None,
        }
    }

    /// Zone-bearing key including explicit zone byte.
    #[must_use]
    pub const fn with_zone(unit_type: UnitType, unit_id: UnitId, reg: RegId, zone: u8) -> Self {
        Self {
            unit_type,
            unit_id,
            reg,
            zone: Some(zone),
        }
    }
}

/// Last-write-wins mailbox of opaque 7-byte payloads.
#[derive(Debug, Default, Clone)]
pub struct RegisterBank {
    slots: HashMap<BankKey, [u8; 7]>,
}

impl RegisterBank {
    /// Create an empty bank.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of occupied mailbox slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// `true` when no slots are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Upsert `record.data` under the key derived from the record.
    ///
    /// Last-write-wins. Re-applying identical bytes leaves the bank unchanged
    /// (same length and same stored payload).
    pub fn apply(&mut self, record: &CanRecord) {
        let key = BankKey::from_record(record);
        if let Some(existing) = self.slots.get_mut(&key) {
            if *existing == record.data {
                return;
            }
            *existing = record.data;
            return;
        }
        self.slots.insert(key, record.data);
    }

    /// Look up a non-zone register slot.
    #[must_use]
    pub fn get(&self, unit_type: UnitType, unit_id: UnitId, reg: RegId) -> Option<[u8; 7]> {
        self.slots
            .get(&BankKey::non_zone(unit_type, unit_id, reg))
            .copied()
    }

    /// Look up a zone-bearing register slot by zone byte.
    #[must_use]
    pub fn get_zone(
        &self,
        unit_type: UnitType,
        unit_id: UnitId,
        reg: RegId,
        zone: u8,
    ) -> Option<[u8; 7]> {
        self.slots
            .get(&BankKey::with_zone(unit_type, unit_id, reg, zone))
            .copied()
    }

    /// Look up by an explicit [`BankKey`].
    #[must_use]
    pub fn get_key(&self, key: &BankKey) -> Option<[u8; 7]> {
        self.slots.get(key).copied()
    }

    /// Look up a non-zone slot and decode via `(reg, data)` only (no dest).
    #[must_use]
    pub fn get_decoded(
        &self,
        unit_type: UnitType,
        unit_id: UnitId,
        reg: RegId,
    ) -> Option<DecodedRegister> {
        self.get(unit_type, unit_id, reg)
            .map(|data| DecodedRegister::from_reg_data(reg, data))
    }

    /// Look up a zone-bearing slot and decode via `(reg, data)` only (no dest).
    #[must_use]
    pub fn get_zone_decoded(
        &self,
        unit_type: UnitType,
        unit_id: UnitId,
        reg: RegId,
        zone: u8,
    ) -> Option<DecodedRegister> {
        self.get_zone(unit_type, unit_id, reg, zone)
            .map(|data| DecodedRegister::from_reg_data(reg, data))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::wire::{CanRecord, Dest};

    fn record(reg: u8, data: [u8; 7]) -> CanRecord {
        CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_ABCDE).unwrap(),
            reg: RegId::new(reg),
            data,
        }
    }

    #[test]
    fn apply_dump_then_get_by_key() {
        let mut bank = RegisterBank::new();
        let r05 = CanRecord::parse_one("0703abcde0501010330000100").unwrap();
        let r03 = CanRecord::parse_one("0703abcde0301e40030000000").unwrap();

        bank.apply(&r05);
        bank.apply(&r03);

        assert_eq!(bank.len(), 2);
        assert_eq!(
            bank.get(r05.unit_type, r05.unit_id, r05.reg),
            Some(r05.data)
        );
        assert_eq!(
            bank.get_zone(r03.unit_type, r03.unit_id, r03.reg, r03.data[0]),
            Some(r03.data)
        );
        // Dest is not part of identity: ControlBox write hits same slot.
        let mut cb = r05.clone();
        cb.dest = Dest::ControlBox;
        assert_eq!(BankKey::from_record(&cb), BankKey::from_record(&r05));
    }

    #[test]
    fn reapply_identical_is_idempotent() {
        let mut bank = RegisterBank::new();
        let r = CanRecord::parse_one("0703abcde0501010330000100").unwrap();
        bank.apply(&r);
        let len_before = bank.len();
        let bytes_before = bank.get(r.unit_type, r.unit_id, r.reg).unwrap();

        bank.apply(&r);
        assert_eq!(bank.len(), len_before);
        assert_eq!(bank.get(r.unit_type, r.unit_id, r.reg), Some(bytes_before));
    }

    #[test]
    fn distinct_zones_for_reg_03() {
        let mut bank = RegisterBank::new();
        let z1 = record(0x03, [0x01, 0xe4, 0x00, 0x03, 0x00, 0x00, 0x00]);
        let z2 = record(0x03, [0x02, 0xe4, 0x00, 0x03, 0x00, 0x00, 0x00]);

        bank.apply(&z1);
        bank.apply(&z2);

        assert_eq!(bank.len(), 2);
        assert_eq!(
            bank.get_zone(z1.unit_type, z1.unit_id, z1.reg, 0x01),
            Some(z1.data)
        );
        assert_eq!(
            bank.get_zone(z2.unit_type, z2.unit_id, z2.reg, 0x02),
            Some(z2.data)
        );
        assert!(is_zone_bearing(RegId::new(0x03)));
        assert!(is_zone_bearing(RegId::new(0x04)));
        assert!(!is_zone_bearing(RegId::new(0x05)));
        assert_eq!(ZONE_BEARING_REGS, &[0x03, 0x04]);
    }

    #[test]
    fn non_zone_reg_05_ignores_byte0_as_zone() {
        let mut bank = RegisterBank::new();
        let a = record(0x05, [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00]);
        let b = record(0x05, [0x02, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00]);

        bank.apply(&a);
        bank.apply(&b);

        // One slot: byte0 is payload, not a zone key component.
        assert_eq!(bank.len(), 1);
        assert_eq!(
            bank.get(a.unit_type, a.unit_id, a.reg),
            Some(b.data),
            "last write wins on the single non-zone slot"
        );
        assert_eq!(BankKey::from_record(&a).zone, None);
        assert_eq!(BankKey::from_record(&b).zone, None);
        assert_eq!(BankKey::from_record(&a), BankKey::from_record(&b));
    }
}
