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

    /// Whether any slot exists for `(unit_type, unit_id)` (any register / zone).
    #[must_use]
    pub fn has_unit(&self, unit_type: UnitType, unit_id: UnitId) -> bool {
        self.slots
            .keys()
            .any(|k| k.unit_type == unit_type && k.unit_id == unit_id)
    }

    /// Prefer `hint` when present in the bank; else the smallest `unit_id` seen for
    /// `unit_type`; else `hint` (even if empty); else [`UnitId::ZERO`].
    ///
    /// Used by the WebSocket snapshot path so live AOA dumps (real unit ids) are
    /// not filtered through the mock feeder id (`abcde`).
    #[must_use]
    pub fn preferred_unit_id(&self, unit_type: UnitType, hint: Option<UnitId>) -> UnitId {
        if let Some(hint) = hint
            && self.has_unit(unit_type, hint)
        {
            return hint;
        }
        let mut best: Option<UnitId> = None;
        for key in self.slots.keys() {
            if key.unit_type != unit_type {
                continue;
            }
            best = Some(match best {
                None => key.unit_id,
                Some(cur) if key.unit_id.get() < cur.get() => key.unit_id,
                Some(cur) => cur,
            });
        }
        best.or(hint).unwrap_or(UnitId::ZERO)
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

    /// All slots for `(unit_type, unit_id)` as tablet-bound [`CanRecord`]s (stable order).
    ///
    /// Dest is always [`crate::wire::Dest::Tablet`] — mailbox identity ignores dest.
    #[must_use]
    pub fn records_for_unit(&self, unit_type: UnitType, unit_id: UnitId) -> Vec<CanRecord> {
        use crate::wire::Dest;
        let mut keys: Vec<&BankKey> = self
            .slots
            .keys()
            .filter(|k| k.unit_type == unit_type && k.unit_id == unit_id)
            .collect();
        keys.sort_by_key(|k| (k.reg.get(), k.zone.unwrap_or(0)));
        keys.into_iter()
            .filter_map(|k| {
                let data = self.slots.get(k).copied()?;
                Some(CanRecord {
                    unit_type: k.unit_type,
                    dest: Dest::Tablet,
                    unit_id: k.unit_id,
                    reg: k.reg,
                    data,
                })
            })
            .collect()
    }

    /// Distinct unit types with at least one slot, ascending by byte value.
    #[must_use]
    pub fn unit_types(&self) -> Vec<UnitType> {
        let mut types: Vec<UnitType> = self.slots.keys().map(|k| k.unit_type).collect();
        types.sort_by_key(|t| t.get());
        types.dedup();
        types
    }

    /// Distinct unit ids for `unit_type` with at least one slot, ascending by value.
    #[must_use]
    pub fn unit_ids(&self, unit_type: UnitType) -> Vec<UnitId> {
        let mut ids: Vec<UnitId> = self
            .slots
            .keys()
            .filter(|k| k.unit_type == unit_type)
            .map(|k| k.unit_id)
            .collect();
        ids.sort_by_key(|id| id.get());
        ids.dedup();
        ids
    }

    /// All slots for `unit_type` across every unit id as tablet-bound [`CanRecord`]s.
    ///
    /// Multi-unit sibling of [`Self::records_for_unit`]: sorted by
    /// `(unit_id, reg, zone)`; dest is always [`crate::wire::Dest::Tablet`].
    #[must_use]
    pub fn records_for_any_unit(&self, unit_type: UnitType) -> Vec<CanRecord> {
        use crate::wire::Dest;
        let mut keys: Vec<&BankKey> = self
            .slots
            .keys()
            .filter(|k| k.unit_type == unit_type)
            .collect();
        keys.sort_by_key(|k| (k.unit_id.get(), k.reg.get(), k.zone.unwrap_or(0)));
        keys.into_iter()
            .filter_map(|k| {
                let data = self.slots.get(k).copied()?;
                Some(CanRecord {
                    unit_type: k.unit_type,
                    dest: Dest::Tablet,
                    unit_id: k.unit_id,
                    reg: k.reg,
                    data,
                })
            })
            .collect()
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

    #[test]
    fn preferred_unit_id_prefers_hint_when_present() {
        // Regression: WS snapshots must not stick to mock feeder id `abcde`
        // when the live dump uses another unit (e.g. `11111`).
        let mut bank = RegisterBank::new();
        let live = UnitId::try_new(0x0_11111).unwrap();
        let mock = UnitId::try_new(0x0_ABCDE).unwrap();
        let hint = live;

        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: live,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: mock,
            reg: RegId::new(0x06),
            data: [1; 7],
        });

        assert_eq!(bank.preferred_unit_id(UnitType::AIRCON, Some(hint)), live);
        assert_eq!(bank.preferred_unit_id(UnitType::AIRCON, Some(mock)), mock);
    }

    #[test]
    fn preferred_unit_id_falls_back_to_smallest_seen() {
        let mut bank = RegisterBank::new();
        let a = UnitId::try_new(0x0_11111).unwrap();
        let b = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: b,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: a,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        // Missing hint → smallest numeric id among AIRCON units.
        assert_eq!(bank.preferred_unit_id(UnitType::AIRCON, None), a);
        // Hint absent from bank → still smallest seen (not the missing hint).
        let missing = UnitId::try_new(0x0_FFFFF).unwrap();
        assert_eq!(bank.preferred_unit_id(UnitType::AIRCON, Some(missing)), a);
    }

    #[test]
    fn preferred_unit_id_empty_bank_uses_hint_or_zero() {
        let bank = RegisterBank::new();
        let hint = UnitId::try_new(0x0_11111).unwrap();
        assert_eq!(bank.preferred_unit_id(UnitType::AIRCON, Some(hint)), hint);
        assert_eq!(bank.preferred_unit_id(UnitType::AIRCON, None), UnitId::ZERO);
    }

    #[test]
    fn unit_types_distinct_ascending() {
        let mut bank = RegisterBank::new();
        let type08 = UnitType::new(0x08);
        let id = UnitId::try_new(0x0_11111).unwrap();
        bank.apply(&CanRecord {
            unit_type: type08,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: id,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        bank.apply(&CanRecord {
            unit_type: type08,
            dest: Dest::Tablet,
            unit_id: UnitId::try_new(0x0_ABCDE).unwrap(),
            reg: RegId::new(0x05),
            data: [1; 7],
        });

        assert_eq!(bank.unit_types(), vec![UnitType::AIRCON, type08]);
        assert_eq!(bank.unit_types().len(), 2);
    }

    #[test]
    fn unit_ids_ascending_per_type() {
        let mut bank = RegisterBank::new();
        let small = UnitId::try_new(0x0_11111).unwrap();
        let large = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: large,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: small,
            reg: RegId::new(0x06),
            data: [0; 7],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: large,
            reg: RegId::new(0x06),
            data: [1; 7],
        });

        assert_eq!(bank.unit_ids(UnitType::AIRCON), vec![small, large]);
        assert_eq!(bank.unit_ids(UnitType::new(0x08)), vec![large]);
        assert!(bank.unit_ids(UnitType::new(0x02)).is_empty());
    }

    #[test]
    fn records_for_any_unit_across_ids_sorted_and_tablet() {
        let mut bank = RegisterBank::new();
        let small = UnitId::try_new(0x0_11111).unwrap();
        let large = UnitId::try_new(0x0_ABCDE).unwrap();
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: large,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        });
        // Zone-bearing reg 0x03: zone from data[0], applied out of zone order.
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: small,
            reg: RegId::new(0x03),
            data: [0x02, 0xe4, 0x00, 0x03, 0x00, 0x00, 0x00],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: small,
            reg: RegId::new(0x03),
            data: [0x01, 0xe4, 0x00, 0x03, 0x00, 0x00, 0x00],
        });
        bank.apply(&CanRecord {
            unit_type: UnitType::AIRCON,
            dest: Dest::Tablet,
            unit_id: small,
            reg: RegId::new(0x05),
            data: [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
        });
        // 08-type record must not leak into the 07 projection.
        bank.apply(&CanRecord {
            unit_type: UnitType::new(0x08),
            dest: Dest::Tablet,
            unit_id: small,
            reg: RegId::new(0x06),
            data: [1; 7],
        });

        let records = bank.records_for_any_unit(UnitType::AIRCON);
        let summary: Vec<(u32, u8, u8)> = records
            .iter()
            .map(|r| (r.unit_id.get(), r.reg.get(), r.data[0]))
            .collect();
        assert_eq!(
            summary,
            vec![
                (small.get(), 0x03, 0x01),
                (small.get(), 0x03, 0x02),
                (small.get(), 0x05, 0x01),
                (large.get(), 0x05, 0x01),
            ]
        );
        assert!(records.iter().all(|r| r.dest == Dest::Tablet));
        assert!(records.iter().all(|r| r.unit_type == UnitType::AIRCON));
    }
}
