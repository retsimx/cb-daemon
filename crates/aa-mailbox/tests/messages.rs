//! D-1 protocol test suite for `aa-mailbox` (issue #27).
//!
//! Covers per-register DTO↔bytes round trips, raw-hex passthrough, enum byte
//! mappings, golden JSON shapes for all message types, and zone-bearing
//! address handling. Wire layouts are normative from design D-1 (epic #26).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use aa_mailbox::{
    AckStatus, ClientMessage, EncodeError, ServerMessage, StatusState, decode_payload,
    encode_payload, event_body, snapshot_units,
};
use aa_registers::{CanRecord, RegId, RegisterBank};
use serde_json::{Value, json};

fn assert_wire_round_trip(reg: RegId, payload: &Value, expected: [u8; 7]) {
    let bytes = encode_payload(reg, payload).unwrap();
    assert_eq!(bytes, expected, "reg {reg}: unexpected wire bytes");
    let decoded = decode_payload(reg, bytes).unwrap();
    assert_eq!(&decoded, payload, "reg {reg}: DTO JSON did not round-trip");
}

// --- 1. Per-register round trips -------------------------------------------

#[test]
fn reg01_zone_config_round_trip() {
    assert_wire_round_trip(
        RegId::new(0x01),
        &json!({
            "header": 0x20,
            "total_zones": 3,
            "constant_zones": 1,
            "constant_zone_ids": [1, 0, 0],
            "filter_clean_required": false,
        }),
        [0x20, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00],
    );
}

#[test]
fn reg02_unit_activation_round_trip() {
    assert_wire_round_trip(
        RegId::new(0x02),
        &json!({
            "unit_type": "panasonic",
            "activation_status": "code_enabled",
            "dict_fw_major": 2,
            "dict_fw_minor": 3,
        }),
        [0x01, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00],
    );
}

#[test]
fn reg03_zone_state_round_trip() {
    // Zone is part of the CAN address, not the payload: the codec stamps byte
    // 0 as 0x00 on encode and drops it on decode, so the DTO JSON (which has
    // no zone field) round-trips exactly.
    assert_wire_round_trip(
        RegId::new(0x03),
        &json!({
            "open": true,
            "damper_pct": 100,
            "sensor_type": "wired",
            "target_temp_c": 24.0,
            "measured_temp_c": 23.1,
        }),
        [0x00, 0xE4, 0x02, 0x30, 0x17, 0x01, 0x00],
    );
}

#[test]
fn reg04_zone_limits_round_trip() {
    // Zone byte stamped 0x00 on encode (same address limitation as reg 03),
    // dropped on decode.
    assert_wire_round_trip(
        RegId::new(0x04),
        &json!({
            "min_damper": 10,
            "max_damper": 90,
            "motion_status": 1,
            "motion_config": 2,
            "zone_error": 0,
            "rssi": 45,
        }),
        [0x00, 0x0a, 0x5a, 0x01, 0x02, 0x00, 0x2d],
    );
}

#[test]
fn reg05_system_status_round_trip() {
    assert_wire_round_trip(
        RegId::new(0x05),
        &json!({
            "power": "on",
            "mode": "cool",
            "fan": "high",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false,
            "rf_sys_id": 0,
        }),
        [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00],
    );
}

#[test]
fn reg06_firmware_round_trip() {
    assert_wire_round_trip(
        RegId::new(0x06),
        &json!({
            "fw_major": 1,
            "fw_minor": 2,
            "cb_type": 3,
            "rf_fw_major": 0,
        }),
        [0x01, 0x02, 0x03, 0x00, 0x00, 0x00, 0x00],
    );
}

#[test]
fn reg08_system_error_round_trip() {
    assert_wire_round_trip(
        RegId::new(0x08),
        &json!({ "error_code": "E1234" }),
        [0x45, 0x31, 0x32, 0x33, 0x34, 0x00, 0x00],
    );
}

#[test]
fn reg09_activation_code_round_trip() {
    // Lowercase code round-trips exactly; uppercase normalizes to lowercase on
    // decode (covered by reg09_activation_code_uppercase_normalized).
    assert_wire_round_trip(
        RegId::new(0x09),
        &json!({
            "action": "set_code",
            "unlock_code": "a1b2",
            "activation_days": 30,
        }),
        [0x00, 0xa1, 0xb2, 0x1e, 0x00, 0x00, 0x00],
    );
}

#[test]
fn reg09_activation_code_uppercase_normalized() {
    let bytes = encode_payload(
        RegId::new(0x09),
        &json!({ "action": "unlock", "unlock_code": "A1B2", "activation_days": 7 }),
    )
    .unwrap();
    assert_eq!(bytes, [0x01, 0xa1, 0xb2, 0x07, 0x00, 0x00, 0x00]);
    let decoded = decode_payload(RegId::new(0x09), bytes).unwrap();
    assert_eq!(
        decoded,
        json!({ "action": "unlock", "unlock_code": "a1b2", "activation_days": 7 })
    );
}

#[test]
fn reg0a_unit_announcement_round_trip() {
    assert_wire_round_trip(RegId::new(0x0a), &json!({}), [0; 7]);
}

#[test]
fn reg12_sensor_pairing_read_round_trip() {
    assert_wire_round_trip(
        RegId::new(0x12),
        &json!({
            "sensor_uid": "01613d",
            "pairing": true,
            "sensor_rev": 5,
        }),
        [0x01, 0x61, 0x3d, 0x01, 0x05, 0x00, 0x00],
    );
}

#[test]
fn reg12_sensor_pairing_write_encode_path() {
    let bytes = encode_payload(
        RegId::new(0x12),
        &json!({ "sensor_uid": "01613d", "zone": 2 }),
    )
    .unwrap();
    assert_eq!(bytes, [0x01, 0x61, 0x3d, 0x02, 0x00, 0x00, 0x00]);
    // Decode always returns the read shape; a write echo's zone is not
    // recoverable from the bytes (documented codec behaviour).
    let decoded = decode_payload(RegId::new(0x12), bytes).unwrap();
    assert_eq!(
        decoded,
        json!({ "sensor_uid": "01613d", "pairing": true, "sensor_rev": 0 })
    );
}

#[test]
fn reg12_read_shape_wins_on_encode() {
    // A payload matching both shapes (all four fields) is treated as a read:
    // byte 3 carries `pairing`, byte 4 `sensor_rev`, and `zone` is dropped.
    let bytes = encode_payload(
        RegId::new(0x12),
        &json!({ "sensor_uid": "01613d", "pairing": false, "sensor_rev": 9, "zone": 3 }),
    )
    .unwrap();
    assert_eq!(bytes, [0x01, 0x61, 0x3d, 0x00, 0x09, 0x00, 0x00]);
    let decoded = decode_payload(RegId::new(0x12), bytes).unwrap();
    assert_eq!(
        decoded,
        json!({ "sensor_uid": "01613d", "pairing": false, "sensor_rev": 9 })
    );
}

#[test]
fn reg13_info_byte_round_trip() {
    assert_wire_round_trip(
        RegId::new(0x13),
        &json!({ "info_byte": 7 }),
        [0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    );
}

#[test]
fn reg26_rf_device_pairing_round_trip() {
    assert_wire_round_trip(
        RegId::new(0x26),
        &json!({
            "pairing_control": 1,
            "rf_device_type": 2,
            "zone_channel": 3,
        }),
        [0x01, 0x02, 0x03, 0x00, 0x00, 0x00, 0x00],
    );
}

#[test]
fn reg27_rf_device_calibration_round_trip() {
    // Wire order is channel BEFORE up/down position (normative).
    assert_wire_round_trip(
        RegId::new(0x27),
        &json!({
            "calibration_control": 1,
            "channel": 5,
            "up_down_position": 7,
        }),
        [0x01, 0x05, 0x07, 0x00, 0x00, 0x00, 0x00],
    );
}

#[test]
fn reg27_raw_hex_confirms_channel_before_position() {
    let bytes = encode_payload(RegId::new(0x27), &json!("01050700000000")).unwrap();
    assert_eq!(bytes, [0x01, 0x05, 0x07, 0x00, 0x00, 0x00, 0x00]);
    let decoded = decode_payload(RegId::new(0x27), bytes).unwrap();
    assert_eq!(
        decoded,
        json!({
            "calibration_control": 1,
            "channel": 5,
            "up_down_position": 7,
        })
    );
}

// --- 2. Raw-hex passthrough ------------------------------------------------

#[test]
fn raw_hex_reg05_acceptance_criterion() {
    // Explicit acceptance criterion from design D-1:
    // "01010330000100" <-> reg-05 bytes [0x01,0x01,0x03,0x30,0x00,0x01,0x00].
    let bytes = encode_payload(RegId::new(0x05), &json!("01010330000100")).unwrap();
    assert_eq!(bytes, [0x01, 0x01, 0x03, 0x30, 0x00, 0x01, 0x00]);
    // Known registers decode to the typed DTO; re-encoding reproduces the
    // raw bytes exactly, closing the hex->bytes->DTO->bytes loop.
    let decoded = decode_payload(RegId::new(0x05), bytes).unwrap();
    assert_eq!(
        decoded,
        json!({
            "power": "on",
            "mode": "cool",
            "fan": "high",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false,
            "rf_sys_id": 0,
        })
    );
    assert_eq!(encode_payload(RegId::new(0x05), &decoded).unwrap(), bytes);
}

#[test]
fn raw_hex_unknown_registers_passthrough() {
    let cases = [
        (0x16, "a1b2c3d4e5f607"),
        (0x17, "00112233445566"),
        (0x1d, "deadbeefcafe00"),
        (0x1e, "ffffffffffffff"),
    ];
    for (reg, hex) in cases {
        let bytes = encode_payload(RegId::new(reg), &json!(hex)).unwrap();
        let decoded = decode_payload(RegId::new(reg), bytes).unwrap();
        assert_eq!(decoded, json!(hex), "reg {reg:#04x} decode");
        assert_eq!(
            encode_payload(RegId::new(reg), &decoded).unwrap(),
            bytes,
            "reg {reg:#04x} re-encode"
        );
    }
}

#[test]
fn raw_hex_accepts_uppercase() {
    let bytes = encode_payload(RegId::new(0x16), &json!("0102030405060F")).unwrap();
    assert_eq!(bytes, [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x0f]);
}

#[test]
fn raw_hex_rejects_wrong_length_and_non_hex() {
    let err = encode_payload(RegId::new(0x16), &json!("0102030405060")).unwrap_err();
    assert!(
        matches!(err, EncodeError::BadHex(_)),
        "13 chars must be BadHex"
    );
    let err = encode_payload(RegId::new(0x16), &json!("0102030405060z")).unwrap_err();
    assert!(
        matches!(err, EncodeError::BadHex(_)),
        "non-hex must be BadHex"
    );
    let err = encode_payload(RegId::new(0x16), &json!("zzzzzzzzzzzzzz")).unwrap_err();
    assert!(matches!(err, EncodeError::BadHex(_)));
}

#[test]
fn unknown_register_rejects_typed_object() {
    let err = encode_payload(RegId::new(0x16), &json!({ "info_byte": 1 })).unwrap_err();
    assert!(matches!(err, EncodeError::UnknownRegister(_)));
}

#[test]
fn unknown_wire_enum_falls_back_to_raw_hex() {
    // Power byte 0x07 is outside the DTO enums: decode falls back to the raw
    // lowercase hex string instead of a lossy typed object.
    let decoded =
        decode_payload(RegId::new(0x05), [0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).unwrap();
    assert_eq!(decoded, json!("07000000000000"));
    // Sensor type byte 0x05 on reg 03 (zone byte 0x00).
    let decoded =
        decode_payload(RegId::new(0x03), [0x00, 0x00, 0x05, 0x30, 0x17, 0x01, 0x00]).unwrap();
    assert_eq!(decoded, json!("00000530170100"));
}

// --- 3. Enum byte mapping ---------------------------------------------------

#[test]
fn enum_mode_bytes() {
    let cases = [
        ("cool", 0x01),
        ("heat", 0x02),
        ("vent", 0x03),
        ("auto", 0x04),
        ("dry", 0x05),
        ("myauto", 0x06),
    ];
    for (wire, byte) in cases {
        let payload = json!({
            "power": "on",
            "mode": wire,
            "fan": "auto",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false,
            "rf_sys_id": 0,
        });
        let bytes = encode_payload(RegId::new(0x05), &payload).unwrap();
        assert_eq!(bytes[1], byte, "mode={wire}");
        let decoded = decode_payload(RegId::new(0x05), bytes).unwrap();
        assert_eq!(decoded["mode"], wire, "mode={wire} decode");
    }
}

#[test]
fn enum_fan_bytes() {
    let cases = [
        ("off", 0x00),
        ("low", 0x01),
        ("medium", 0x02),
        ("high", 0x03),
        ("auto", 0x04),
        ("auto_aa", 0x05),
    ];
    for (wire, byte) in cases {
        let payload = json!({
            "power": "on",
            "mode": "cool",
            "fan": wire,
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false,
            "rf_sys_id": 0,
        });
        let bytes = encode_payload(RegId::new(0x05), &payload).unwrap();
        assert_eq!(bytes[2], byte, "fan={wire}");
        let decoded = decode_payload(RegId::new(0x05), bytes).unwrap();
        assert_eq!(decoded["fan"], wire, "fan={wire} decode");
    }
}

#[test]
fn enum_power_bytes() {
    let cases = [("on", 0x01), ("off", 0x00)];
    for (wire, byte) in cases {
        let payload = json!({
            "power": wire,
            "mode": "cool",
            "fan": "auto",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false,
            "rf_sys_id": 0,
        });
        let bytes = encode_payload(RegId::new(0x05), &payload).unwrap();
        assert_eq!(bytes[0], byte, "power={wire}");
        let decoded = decode_payload(RegId::new(0x05), bytes).unwrap();
        assert_eq!(decoded["power"], wire, "power={wire} decode");
    }
}

#[test]
fn enum_sensor_type_bytes() {
    let cases = [
        ("no_sensor", 0x00),
        ("rf", 0x01),
        ("wired", 0x02),
        ("rf2can_booster", 0x03),
        ("rf_x", 0x04),
    ];
    for (wire, byte) in cases {
        let payload = json!({
            "open": true,
            "damper_pct": 100,
            "sensor_type": wire,
            "target_temp_c": 24.0,
            "measured_temp_c": 23.0,
        });
        let bytes = encode_payload(RegId::new(0x03), &payload).unwrap();
        assert_eq!(bytes[2], byte, "sensor_type={wire}");
        let decoded = decode_payload(RegId::new(0x03), bytes).unwrap();
        assert_eq!(decoded["sensor_type"], wire, "sensor_type={wire} decode");
    }
}

#[test]
fn enum_activation_status_bytes() {
    let cases = [("no_code", 0x00), ("code_enabled", 0x01), ("expired", 0x02)];
    for (wire, byte) in cases {
        let payload = json!({
            "unit_type": "daikin",
            "activation_status": wire,
            "dict_fw_major": 0,
            "dict_fw_minor": 0,
        });
        let bytes = encode_payload(RegId::new(0x02), &payload).unwrap();
        assert_eq!(bytes[1], byte, "activation_status={wire}");
        let decoded = decode_payload(RegId::new(0x02), bytes).unwrap();
        assert_eq!(
            decoded["activation_status"], wire,
            "activation_status={wire} decode"
        );
    }
}

#[test]
fn enum_action_bytes() {
    let cases = [("set_code", 0x00), ("unlock", 0x01)];
    for (wire, byte) in cases {
        let payload = json!({
            "action": wire,
            "unlock_code": "a1b2",
            "activation_days": 0,
        });
        let bytes = encode_payload(RegId::new(0x09), &payload).unwrap();
        assert_eq!(bytes[0], byte, "action={wire}");
        let decoded = decode_payload(RegId::new(0x09), bytes).unwrap();
        assert_eq!(decoded["action"], wire, "action={wire} decode");
    }
}

#[test]
fn enum_unit_type_bytes() {
    // Documented mapping (non-normative in issue #27, but codec-supported).
    let cases = [
        ("daikin", 0x00),
        ("panasonic", 0x01),
        ("fujitsu", 0x02),
        ("samsung_dvm", 0x03),
    ];
    for (wire, byte) in cases {
        let payload = json!({
            "unit_type": wire,
            "activation_status": "no_code",
            "dict_fw_major": 1,
            "dict_fw_minor": 2,
        });
        let bytes = encode_payload(RegId::new(0x02), &payload).unwrap();
        assert_eq!(bytes[0], byte, "unit_type={wire}");
        let decoded = decode_payload(RegId::new(0x02), bytes).unwrap();
        assert_eq!(decoded["unit_type"], wire, "unit_type={wire} decode");
    }
}

// --- 4. Golden JSON message shapes ------------------------------------------

#[test]
fn golden_write_omitted_addressing() {
    let msg = ClientMessage::Write {
        msg_id: "req-1".to_owned(),
        unit_type: None,
        unit_id: None,
        register: "05".to_owned(),
        zone: None,
        payload: json!({
            "power": "on",
            "mode": "cool",
            "fan": "high",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false,
            "rf_sys_id": 0,
        }),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "write");
    assert!(v.get("unit_type").is_none());
    assert!(v.get("unit_id").is_none());
    assert!(v.get("zone").is_none());
    let expected: Value = serde_json::from_str(
        r#"{"type":"write","msg_id":"req-1","register":"05","payload":{"power":"on","mode":"cool","fan":"high","target_temp_c":24.0,"myzone_id":0,"fresh_air":false,"rf_sys_id":0}}"#,
    )
    .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ClientMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_write_full_addressing() {
    let msg = ClientMessage::Write {
        msg_id: "req-1".to_owned(),
        unit_type: Some("07".to_owned()),
        unit_id: Some("11111".to_owned()),
        register: "03".to_owned(),
        zone: Some(1),
        payload: json!({
            "open": true,
            "damper_pct": 100,
            "sensor_type": "wired",
            "target_temp_c": 24.0,
            "measured_temp_c": 23.1,
        }),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "write");
    assert_eq!(v["unit_type"], "07");
    assert_eq!(v["unit_id"], "11111");
    assert_eq!(v["zone"], 1);
    let expected: Value = serde_json::from_str(
        r#"{"type":"write","msg_id":"req-1","unit_type":"07","unit_id":"11111","register":"03","zone":1,"payload":{"open":true,"damper_pct":100,"sensor_type":"wired","target_temp_c":24.0,"measured_temp_c":23.1}}"#,
    )
    .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ClientMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_read_message() {
    let msg = ClientMessage::Read {
        msg_id: "req-2".to_owned(),
        unit_type: None,
        unit_id: None,
        register: "03".to_owned(),
        zone: Some(1),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "read");
    assert_eq!(v["zone"], 1);
    assert!(v.get("unit_type").is_none());
    assert!(v.get("unit_id").is_none());
    let expected: Value =
        serde_json::from_str(r#"{"type":"read","msg_id":"req-2","register":"03","zone":1}"#)
            .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ClientMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_command_message() {
    let msg = ClientMessage::Command {
        msg_id: "req-3".to_owned(),
        action: "resync".to_owned(),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "command");
    assert_eq!(v["action"], "resync");
    let expected: Value =
        serde_json::from_str(r#"{"type":"command","msg_id":"req-3","action":"resync"}"#).unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ClientMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_snapshot_message() {
    let mut registers = BTreeMap::new();
    registers.insert(
        "01".to_owned(),
        json!({
            "header": 0x20,
            "total_zones": 3,
            "constant_zones": 1,
            "constant_zone_ids": [1, 0, 0],
            "filter_clean_required": false,
        }),
    );
    let msg = ServerMessage::Snapshot {
        units: BTreeMap::from([("07:11111".to_owned(), registers)]),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "snapshot");
    let expected: Value = serde_json::from_str(
        r#"{"type":"snapshot","units":{"07:11111":{"01":{"header":32,"total_zones":3,"constant_zones":1,"constant_zone_ids":[1,0,0],"filter_clean_required":false}}}}"#,
    )
    .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_snapshot_nested_zones_and_raw_hex() {
    let mut registers = BTreeMap::new();
    registers.insert(
        "03".to_owned(),
        json!({
            "1": {
                "open": true,
                "damper_pct": 100,
                "sensor_type": "wired",
                "target_temp_c": 24.0,
                "measured_temp_c": 23.1,
            }
        }),
    );
    registers.insert("16".to_owned(), json!("a1b2c3d4e5f607"));
    let msg = ServerMessage::Snapshot {
        units: BTreeMap::from([("07:11111".to_owned(), registers)]),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["units"]["07:11111"]["03"]["1"]["damper_pct"], 100);
    assert_eq!(v["units"]["07:11111"]["16"], "a1b2c3d4e5f607");
    let back: ServerMessage = serde_json::from_value(v).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn golden_event_message() {
    let msg = ServerMessage::Event {
        unit_type: "07".to_owned(),
        unit_id: "11111".to_owned(),
        register: "05".to_owned(),
        zone: None,
        payload: json!({
            "power": "on",
            "mode": "cool",
            "fan": "high",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false,
            "rf_sys_id": 0,
        }),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "event");
    assert!(v.get("zone").is_none());
    let expected: Value = serde_json::from_str(
        r#"{"type":"event","unit_type":"07","unit_id":"11111","register":"05","payload":{"power":"on","mode":"cool","fan":"high","target_temp_c":24.0,"myzone_id":0,"fresh_air":false,"rf_sys_id":0}}"#,
    )
    .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_event_message_with_zone() {
    let msg = ServerMessage::Event {
        unit_type: "07".to_owned(),
        unit_id: "11111".to_owned(),
        register: "04".to_owned(),
        zone: Some(2),
        payload: json!({
            "min_damper": 10,
            "max_damper": 90,
            "motion_status": 1,
            "motion_config": 2,
            "zone_error": 0,
            "rssi": 45,
        }),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "event");
    assert_eq!(v["zone"], 2);
    let expected: Value = serde_json::from_str(
        r#"{"type":"event","unit_type":"07","unit_id":"11111","register":"04","zone":2,"payload":{"min_damper":10,"max_damper":90,"motion_status":1,"motion_config":2,"zone_error":0,"rssi":45}}"#,
    )
    .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_read_result_message() {
    let msg = ServerMessage::ReadResult {
        msg_id: "req-4".to_owned(),
        unit_type: "07".to_owned(),
        unit_id: "11111".to_owned(),
        register: "03".to_owned(),
        zone: Some(1),
        payload: json!({
            "open": true,
            "damper_pct": 100,
            "sensor_type": "wired",
            "target_temp_c": 24.0,
            "measured_temp_c": 23.1,
        }),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "read_result");
    assert_eq!(v["msg_id"], "req-4");
    assert_eq!(v["zone"], 1);
    let expected: Value = serde_json::from_str(
        r#"{"type":"read_result","msg_id":"req-4","unit_type":"07","unit_id":"11111","register":"03","zone":1,"payload":{"open":true,"damper_pct":100,"sensor_type":"wired","target_temp_c":24.0,"measured_temp_c":23.1}}"#,
    )
    .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_ack_success_message() {
    let msg = ServerMessage::Ack {
        msg_id: "req-5".to_owned(),
        status: AckStatus::Success,
        reason: None,
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "ack");
    assert_eq!(v["status"], "success");
    assert!(v.get("reason").is_none());
    let expected: Value =
        serde_json::from_str(r#"{"type":"ack","msg_id":"req-5","status":"success"}"#).unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_ack_error_message() {
    let msg = ServerMessage::Ack {
        msg_id: "req-5".to_owned(),
        status: AckStatus::Error,
        reason: Some("register write rejected".to_owned()),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "ack");
    assert_eq!(v["status"], "error");
    assert_eq!(v["reason"], "register write rejected");
    let expected: Value = serde_json::from_str(
        r#"{"type":"ack","msg_id":"req-5","status":"error","reason":"register write rejected"}"#,
    )
    .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_status_synced_message() {
    let msg = ServerMessage::Status {
        state: StatusState::Synced,
        detail: None,
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "status");
    assert_eq!(v["state"], "synced");
    assert!(v.get("detail").is_none());
    let expected: Value = serde_json::from_str(r#"{"type":"status","state":"synced"}"#).unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_status_with_detail_message() {
    let msg = ServerMessage::Status {
        state: StatusState::LinkDown,
        detail: Some("tcp disconnected".to_owned()),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "status");
    assert_eq!(v["state"], "link_down");
    assert_eq!(v["detail"], "tcp disconnected");
    let expected: Value = serde_json::from_str(
        r#"{"type":"status","state":"link_down","detail":"tcp disconnected"}"#,
    )
    .unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_error_message() {
    let msg = ServerMessage::Error {
        message: "boom".to_owned(),
        reason: Some("detail".to_owned()),
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["message"], "boom");
    assert_eq!(v["reason"], "detail");
    let expected: Value =
        serde_json::from_str(r#"{"type":"error","message":"boom","reason":"detail"}"#).unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

#[test]
fn golden_error_without_reason_message() {
    let msg = ServerMessage::Error {
        message: "boom".to_owned(),
        reason: None,
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["type"], "error");
    assert!(v.get("reason").is_none());
    let expected: Value = serde_json::from_str(r#"{"type":"error","message":"boom"}"#).unwrap();
    assert_eq!(v, expected);
    assert_eq!(
        serde_json::from_value::<ServerMessage>(expected).unwrap(),
        msg
    );
}

// --- 5. Zone-bearing address handling ---------------------------------------

#[test]
fn write_with_zone_deserializes_zone() {
    let raw = r#"{"type":"write","msg_id":"req-1","register":"03","zone":1,"payload":{"open":true,"damper_pct":100,"sensor_type":"wired","target_temp_c":24.0,"measured_temp_c":23.1}}"#;
    let msg: ClientMessage = serde_json::from_str(raw).unwrap();
    let ClientMessage::Write {
        register,
        zone,
        payload,
        ..
    } = &msg
    else {
        panic!("expected write");
    };
    assert_eq!(register, "03");
    assert_eq!(*zone, Some(1));
    assert_eq!(payload["sensor_type"], "wired");
}

#[test]
fn write_without_zone_deserializes_none() {
    let raw = r#"{"type":"write","msg_id":"req-1","register":"05","payload":{"power":"on","mode":"cool","fan":"high","target_temp_c":24.0,"myzone_id":0,"fresh_air":false,"rf_sys_id":0}}"#;
    let msg: ClientMessage = serde_json::from_str(raw).unwrap();
    let ClientMessage::Write { register, zone, .. } = &msg else {
        panic!("expected write");
    };
    assert_eq!(register, "05");
    assert!(zone.is_none());
}

#[test]
fn read_with_zone_deserializes_zone() {
    let raw = r#"{"type":"read","msg_id":"req-2","register":"03","zone":1}"#;
    let msg: ClientMessage = serde_json::from_str(raw).unwrap();
    let ClientMessage::Read { zone, .. } = &msg else {
        panic!("expected read");
    };
    assert_eq!(*zone, Some(1));
}

// --- 6. Multi-unit snapshot projection (design D-3) ------------------------

#[test]
fn snapshot_units_covers_all_unit_types() {
    let mut bank = RegisterBank::new();
    bank.apply(&CanRecord::parse_one("0703111110501010330000100").unwrap());
    bank.apply(&CanRecord::parse_one("0803111110501010330000100").unwrap());
    bank.apply(&CanRecord::parse_one("0803abcde0601020300000000").unwrap());

    let units = snapshot_units(&bank);
    assert_eq!(
        units.keys().collect::<Vec<_>>(),
        vec!["07:11111", "08:11111", "08:abcde"]
    );
}

#[test]
fn snapshot_units_decodes_dtos_and_nested_zones() {
    let mut bank = RegisterBank::new();
    // Reg 05 (non-zone) and reg 03 zone 1 (zone-bearing) for the same unit.
    bank.apply(&CanRecord::parse_one("0703111110501010330000100").unwrap());
    bank.apply(&CanRecord::parse_one("0703111110301e40230170100").unwrap());

    let units = snapshot_units(&bank);
    let unit = &units["07:11111"];
    assert_eq!(
        unit["05"],
        json!({
            "power": "on",
            "mode": "cool",
            "fan": "high",
            "target_temp_c": 24.0,
            "myzone_id": 0,
            "fresh_air": false,
            "rf_sys_id": 0,
        })
    );
    assert_eq!(
        unit["03"]["1"],
        json!({
            "open": true,
            "damper_pct": 100,
            "sensor_type": "wired",
            "target_temp_c": 24.0,
            "measured_temp_c": 23.1,
        })
    );
}

#[test]
fn event_body_zone_bearing_and_plain_records() {
    // Zone-bearing reg 03: zone is taken from data[0].
    let record = CanRecord::parse_one("0703111110301e40030000000").unwrap();
    let (register, zone, payload) = event_body(&record);
    assert_eq!(register, "03");
    assert_eq!(zone, Some(1));
    assert_eq!(payload["damper_pct"], 100);

    // Non-zone reg 05: no zone, decoded DTO payload.
    let record = CanRecord::parse_one("0703111110501010330000100").unwrap();
    let (register, zone, payload) = event_body(&record);
    assert_eq!(register, "05");
    assert_eq!(zone, None);
    assert_eq!(payload["power"], "on");

    // Unknown register falls back to the raw 14-char hex string.
    let record = CanRecord::parse_one("07031111116a1b2c3d4e5f607").unwrap();
    let (register, zone, payload) = event_body(&record);
    assert_eq!(register, "16");
    assert_eq!(zone, None);
    assert_eq!(payload, json!("a1b2c3d4e5f607"));
}
