//! Wire payload constants and helpers for the CAN2 CB tablet side.

use aa_registers::{CanRecord, WireError};

/// Ping payload (`CRC = 0xdb`).
pub(crate) const PING: &[u8] = b"Ping";

/// Negotiate / `getSystemData` payload (`CRC = 0x15`).
pub(crate) const GET_SYSTEM_DATA: &[u8] = b"getSystemData";

/// Empty steady-state poll (`CRC = 0xb2`). Trailing space is significant.
pub(crate) const EMPTY_SET_CAN: &[u8] = b"setCAN ";

/// Ack after applying a `getCAN` (`CRC = 0xaa`).
pub(crate) const ACK_CAN: &[u8] = b"ackCAN 1";

/// Initial mailbox dump `setCAN` matching live MyAir5/USB bring-up.
///
/// Stock queues unit-type `08` flush tokens before/at open; the first non-empty
/// `setCAN` after `CAN2 in use` is what returns the large `getCAN` `MyAir5` uses
/// for `:2025`. The classic `07…06` zero-uid flush alone does not.
pub(crate) const DUMP_SET_CAN: &[u8] =
    b"setCAN 0801000000600000000000000 0801000000236000000000000";

/// Reg-06 zero-uid flush (`setCAN 0701000000600000000000000`).
///
/// `aa_interop` spec: this must be the **first** `setCAN` sent — it resets the CB
/// "dirty" flag so the CB re-sends the content of all registers. Without it, the
/// CB only reports registers it has never sent (or that changed) since the last
/// ack, so later dumps shrink to a handful of records and `MyAir5` `rawCan`
/// stays incomplete. Mirrors the seed token aaservice enqueues on USB open.
pub(crate) const DIRTY_RESET_SET_CAN: &[u8] = b"setCAN 0701000000600000000000000";

/// Substring present in a successful negotiate reply.
pub(crate) const CAN2_IN_USE: &str = "CAN2 in use";
const GET_CAN_PREFIX: &[u8] = b"getCAN";

/// Returns `true` when `payload` is exactly the Ping body.
#[must_use]
pub(crate) fn is_ping(payload: &[u8]) -> bool {
    payload == PING
}

/// Strip a leading `getCAN` and parse records via [`CanRecord::parse_many`].
///
/// Lone `"1"` tokens are skipped by `parse_many`.
///
/// # Errors
///
/// - [`WireError::Incomplete`] if the payload is not valid UTF-8 or lacks the
///   `getCAN` prefix (empty body after the prefix yields `Ok([])`)
/// - Propagates [`CanRecord::parse_many`] errors for malformed tokens
pub(crate) fn parse_get_can(payload: &[u8]) -> Result<Vec<CanRecord>, WireError> {
    let Ok(text) = std::str::from_utf8(payload) else {
        return Err(WireError::Incomplete);
    };
    let Some(body) = text.strip_prefix("getCAN") else {
        return Err(WireError::Incomplete);
    };
    CanRecord::parse_many(body)
}

/// Build a `setCAN …` payload from records (space after `setCAN`, space-separated hex).
#[must_use]
pub(crate) fn build_set_can(records: &[CanRecord]) -> Vec<u8> {
    let mut out = String::from("setCAN ");
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&record.to_wire());
    }
    out.into_bytes()
}

/// Returns `true` when `payload` looks like a `getCAN` body.
#[must_use]
pub(crate) fn is_get_can(payload: &[u8]) -> bool {
    payload.starts_with(GET_CAN_PREFIX)
}

/// Stock NACK polarity: ASCII `getCAN` payloads with `payload[7] == b'0'`
/// (e.g. `getCAN 0` / `getCAN 0000…`) request a setCAN retry.
#[must_use]
pub(crate) fn is_get_can_nack(payload: &[u8]) -> bool {
    is_get_can(payload) && payload.get(7) == Some(&b'0')
}

/// Returns `true` when negotiate reply indicates CAN2 is active.
#[must_use]
pub(crate) fn is_can2_in_use(payload: &[u8]) -> bool {
    std::str::from_utf8(payload).is_ok_and(|s| s.contains(CAN2_IN_USE))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aa_crc::crc8;
    use aa_registers::{Dest, RegId, UnitId, UnitType};

    #[test]
    fn golden_payload_crcs() {
        assert_eq!(crc8(PING), 0xdb);
        assert_eq!(crc8(GET_SYSTEM_DATA), 0x15);
        assert_eq!(crc8(EMPTY_SET_CAN), 0xb2);
        assert_eq!(crc8(ACK_CAN), 0xaa);
        assert_eq!(crc8(DUMP_SET_CAN), 0x9c);
        assert_eq!(crc8(DIRTY_RESET_SET_CAN), 0x6a);
    }

    #[test]
    fn dirty_reset_matches_aaservice_seed_token() {
        // Parity: aaservice enqueues this exact token on USB accessory open.
        assert_eq!(DIRTY_RESET_SET_CAN, b"setCAN 0701000000600000000000000");
        assert_eq!(crc8(b"setCAN 0701000000600000000000000"), 0x6a);
    }

    #[test]
    fn dump_matches_stock_myair5_unit08_flush_tokens() {
        // Regression: USB capture showed MyAir5's first productive setCAN after CAN2
        // uses unit-type 08 flush tokens; 07-only flush did not yield :2025 system data.
        assert_eq!(
            DUMP_SET_CAN,
            b"setCAN 0801000000600000000000000 0801000000236000000000000"
        );
    }

    #[test]
    fn is_ping_exact() {
        assert!(is_ping(b"Ping"));
        assert!(!is_ping(b"Ping "));
        assert!(!is_ping(b"ping"));
    }

    #[test]
    fn is_get_can_nack_polarity() {
        assert!(is_get_can_nack(b"getCAN 0"));
        assert!(is_get_can_nack(b"getCAN 0000"));
        assert!(!is_get_can_nack(b"getCAN 1"));
        assert!(!is_get_can_nack(b"getCAN 1 0703abcde0501010330000100"));
        assert!(!is_get_can_nack(b"CAN2 in use"));
    }

    #[test]
    fn parse_get_can_skips_lone_one() {
        let payload = b"getCAN 1 0703abcde0120030101000000 0703abcde0501010330000100";
        let records = parse_get_can(payload).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].reg.get(), 0x01);
        assert_eq!(records[1].reg.get(), 0x05);
    }

    #[test]
    fn parse_get_can_requires_prefix() {
        assert!(matches!(
            parse_get_can(b"0703abcde0120030101000000"),
            Err(WireError::Incomplete)
        ));
    }

    #[test]
    fn build_set_can_joins_records() {
        let r = CanRecord {
            unit_type: UnitType::new(0x07),
            dest: Dest::ControlBox,
            unit_id: UnitId::try_new(0).unwrap(),
            reg: RegId::new(0x06),
            data: [0; 7],
        };
        let bytes = build_set_can(std::slice::from_ref(&r));
        assert_eq!(bytes, b"setCAN 0701000000600000000000000");
        let two = build_set_can(&[r.clone(), r]);
        assert_eq!(
            two,
            b"setCAN 0701000000600000000000000 0701000000600000000000000"
        );
    }

    #[test]
    fn build_set_can_empty_matches_empty_poll_prefix() {
        assert_eq!(build_set_can(&[]), EMPTY_SET_CAN);
    }
}
