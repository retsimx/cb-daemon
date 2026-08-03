//! Golden serde round-trips and acceptance tests for `aa-mailbox`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use aa_mailbox::{
    AckStatus, ClientMessage, EncodeError, ServerMessage, SnapshotBody, SystemStatusDto,
    ZoneConfigDto, ZoneDto, apply_records_to_bank, apply_snapshot_body_to_bank,
    records_from_update, snapshot_from_bank,
};
use aa_registers::{
    CanRecord, DecodedRegister, Dest, Fan, FreshAir, Mode, Power, RegId, RegisterBank, SensorType,
    SystemStatus, UnitId, UnitType, ZoneConfig, ZoneState,
};
use serde_json::{Value, json};

#[allow(clippy::unwrap_used, clippy::expect_used)]
fn unit_id() -> UnitId {
    UnitId::from_hex("abcde").unwrap()
}

#[test]
fn golden_mailbox_snapshot_round_trip() {
    let raw = include_str!("fixtures/mailbox_snapshot.json");
    let msg: ServerMessage = serde_json::from_str(raw).expect("deserialize snapshot");
    let ServerMessage::MailboxSnapshot {
        ref unit_id,
        ref system_status,
        ref zone_config,
        ref zones,
        ref can_records,
    } = msg
    else {
        panic!("expected mailbox_snapshot");
    };
    assert_eq!(unit_id, "abcde");
    assert!(system_status.is_some());
    assert!(zone_config.is_some());
    assert_eq!(zones.as_ref().map(BTreeMap::len), Some(2));
    // Fixture predates can_records; omitted field deserializes as None.
    assert!(can_records.is_none());

    let encoded = serde_json::to_value(&msg).expect("serialize");
    let expected: Value = serde_json::from_str(raw).expect("parse fixture");
    assert_eq!(encoded, expected);
}

#[test]
fn golden_mailbox_event_round_trip() {
    let raw = include_str!("fixtures/mailbox_event.json");
    let msg: ServerMessage = serde_json::from_str(raw).expect("deserialize event");
    assert!(matches!(msg, ServerMessage::MailboxEvent { .. }));
    let encoded: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    let expected: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(encoded, expected);
}

#[test]
fn golden_ack_success_round_trip() {
    let raw = include_str!("fixtures/ack_success.json");
    let msg: ServerMessage = serde_json::from_str(raw).expect("deserialize ack");
    match &msg {
        ServerMessage::Ack {
            msg_id,
            status,
            reason,
        } => {
            assert_eq!(msg_id, "req-101");
            assert_eq!(*status, AckStatus::Success);
            assert!(reason.is_none());
        }
        _ => panic!("expected ack"),
    }
    let encoded: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    let expected: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(encoded, expected);
}

#[test]
fn golden_ack_error_round_trip() {
    let raw = include_str!("fixtures/ack_error.json");
    let msg: ServerMessage = serde_json::from_str(raw).expect("deserialize ack error");
    match &msg {
        ServerMessage::Ack { status, reason, .. } => {
            assert_eq!(*status, AckStatus::Error);
            assert_eq!(reason.as_deref(), Some("register write rejected"));
        }
        _ => panic!("expected ack"),
    }
    let encoded: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    let expected: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(encoded, expected);
}

#[test]
fn golden_error_round_trip() {
    let raw = include_str!("fixtures/error.json");
    let msg: ServerMessage = serde_json::from_str(raw).expect("deserialize error");
    assert!(matches!(msg, ServerMessage::Error { .. }));
    let encoded: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    let expected: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(encoded, expected);
}

#[test]
fn golden_mailbox_update_round_trip() {
    let raw = include_str!("fixtures/mailbox_update.json");
    let msg: ClientMessage = serde_json::from_str(raw).expect("deserialize update");
    assert!(matches!(msg, ClientMessage::MailboxUpdate { .. }));
    let encoded: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    let expected: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(encoded, expected);
}

#[test]
fn golden_command_round_trip() {
    let raw = include_str!("fixtures/command.json");
    let msg: ClientMessage = serde_json::from_str(raw).expect("deserialize command");
    match &msg {
        ClientMessage::Command { msg_id, action } => {
            assert_eq!(msg_id, "req-201");
            assert_eq!(action, "resync_mailbox");
        }
        ClientMessage::MailboxUpdate { .. } => panic!("expected command"),
    }
    let encoded: Value = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
    let expected: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(encoded, expected);
}

#[test]
#[allow(clippy::too_many_lines)]
fn acceptance_snapshot_bank_round_trip() {
    let mut bank = RegisterBank::new();
    let status = SystemStatus {
        power: Power::On,
        mode: Mode::Cool,
        fan: Fan::Auto,
        set_temp_x2: 45,
        myzone_id: 1,
        fresh_air: FreshAir::Off,
        rf_sys_id: 7,
    };
    let cfg = ZoneConfig {
        header: 0x20,
        num_zones: 4,
        num_constant: 1,
        constant: [1, 0, 0],
        filter_clean: false,
    };
    let zone = ZoneState {
        zone: 1,
        open: true,
        percent: 100,
        sensor: SensorType::Wired,
        set_temp_x2: 45,
        meas_int: 23,
        meas_dec: 1,
    };
    apply_records_to_bank(
        &mut bank,
        &[
            CanRecord {
                unit_type: UnitType::AIRCON,
                dest: Dest::Tablet,
                unit_id: unit_id(),
                reg: RegId::new(0x05),
                data: status.into(),
            },
            CanRecord {
                unit_type: UnitType::AIRCON,
                dest: Dest::Tablet,
                unit_id: unit_id(),
                reg: RegId::new(0x01),
                data: cfg.into(),
            },
            CanRecord {
                unit_type: UnitType::AIRCON,
                dest: Dest::Tablet,
                unit_id: unit_id(),
                reg: RegId::new(0x03),
                data: zone.into(),
            },
        ],
    );

    let snap = snapshot_from_bank(&bank, UnitType::AIRCON, unit_id());
    let ServerMessage::MailboxSnapshot {
        unit_id: uid,
        system_status,
        zone_config,
        zones,
        can_records,
    } = snap
    else {
        panic!("expected snapshot");
    };
    let body = SnapshotBody {
        unit_id: uid,
        system_status,
        zone_config,
        zones,
        can_records,
    };

    let mut bank2 = RegisterBank::new();
    apply_snapshot_body_to_bank(&mut bank2, UnitType::AIRCON, unit_id(), &body).unwrap();
    let snap2 = snapshot_from_bank(&bank2, UnitType::AIRCON, unit_id());
    // Typed DTO fields round-trip; can_records also re-emit from the bank but may
    // differ in bytes not represented in JSON (zone-config constant ids, rf_sys_id).
    assert_eq!(body.system_status, {
        let ServerMessage::MailboxSnapshot {
            system_status: s, ..
        } = &snap2
        else {
            panic!("expected snapshot");
        };
        s.clone()
    });
    assert_eq!(body.zone_config, {
        let ServerMessage::MailboxSnapshot { zone_config: c, .. } = &snap2 else {
            panic!("expected snapshot");
        };
        c.clone()
    });
    assert_eq!(body.zones, {
        let ServerMessage::MailboxSnapshot { zones: z, .. } = &snap2 else {
            panic!("expected snapshot");
        };
        z.clone()
    });
    let ServerMessage::MailboxSnapshot {
        can_records: Some(recs2),
        ..
    } = snap2
    else {
        panic!("expected can_records on resnapshot");
    };
    assert_eq!(body.can_records.as_ref().map(Vec::len), Some(recs2.len()));
}

#[test]
fn acceptance_system_status_update_record() {
    let payload = json!({
        "power": "on",
        "mode": "cool",
        "fan": "high",
        "target_temp_c": 23.0,
        "myzone_id": 0,
        "fresh_air": false
    });
    let records =
        records_from_update(UnitType::AIRCON, unit_id(), "system_status", &payload).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reg.get(), 0x05);
    assert_eq!(records[0].dest, Dest::ControlBox);
    assert_eq!(records[0].unit_type, UnitType::AIRCON);
}

#[test]
fn acceptance_zone_state_update_record() {
    let payload = json!({
        "zone_id": 2,
        "open": false,
        "damper_pct": 50,
        "sensor_type": "rf",
        "target_temp_c": 21.0,
        "measured_temp_c": 20.5
    });
    let records = records_from_update(UnitType::AIRCON, unit_id(), "zone_state", &payload).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reg.get(), 0x03);
    assert_eq!(records[0].dest, Dest::ControlBox);
    let DecodedRegister::ZoneState(z) = records[0].decode() else {
        panic!("expected ZoneState");
    };
    assert_eq!(z.zone, 2);
    assert!(!z.open);
    assert_eq!(z.percent, 50);
}

#[test]
fn acceptance_unsupported_register() {
    let err =
        records_from_update(UnitType::AIRCON, unit_id(), "sensor_pairing", &json!({})).unwrap_err();
    assert!(matches!(err, EncodeError::UnsupportedRegister(_)));
}

#[test]
fn omit_empty_optional_snapshot_fields() {
    let msg = ServerMessage::MailboxSnapshot {
        unit_id: "abcde".to_owned(),
        system_status: None,
        zone_config: None,
        zones: None,
        can_records: None,
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "mailbox_snapshot");
    assert_eq!(v["unit_id"], "abcde");
    assert!(v.get("system_status").is_none());
    assert!(v.get("zone_config").is_none());
    assert!(v.get("zones").is_none());
    assert!(v.get("can_records").is_none());
}

#[test]
fn dto_constructors_smoke() {
    let _ = SystemStatusDto {
        power: "on".to_owned(),
        mode: "cool".to_owned(),
        fan: "auto".to_owned(),
        target_temp_c: 22.5,
        myzone_id: 1,
        fresh_air: false,
    };
    let _ = ZoneConfigDto {
        total_zones: 4,
        constant_zones: 1,
        filter_clean_required: false,
    };
    let _ = ZoneDto {
        open: true,
        damper_pct: 100,
        sensor_type: "wired".to_owned(),
        target_temp_c: 22.5,
        measured_temp_c: 23.1,
    };
}
