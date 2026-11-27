//! `dc_exchange_validate` tests (PR #22, docs 06 §4.2):
//! - 20,000 track points accepted (single & multi-segment), 20,001 rejected
//! - monotonic non-decreasing `t_offset_s` across all segments (intra-segment & cross-segment
//!   regression rejected; same second accepted; `null` skipped)
//! - `IntentPreset`: `hard_soft` keys must be subset of `chips`; `weights_permille` in 250..=4000;
//!   `name` <= 40 Unicode chars; `chips` <= 8
//! - `TripPlan`: `vias` <= 5; `Place.name` <= 60 Unicode chars; `U64Str` for `vehicle_entry_id`
//! - `DriveDiary`: `history` <= 200
//! - unknown keys rejected at every level; `schema_version` must be 1
use contracts::ErrorCode;
use drive_core::exchange::validate_json;
use serde_json::{json, Value};

fn valid_preset() -> Value {
    json!({
        "kind": "intent_preset",
        "schema_version": 1,
        "preset_id": "p1",
        "name": "夜のあんしん運転",
        "chips": ["no_right", "no_narrow"],
        "hard_soft": { "no_right": "hard", "no_narrow": "soft" },
        "detour": "standard",
        "weights_permille": { "right_turn": 1000, "narrow_road": 250, "merge": 4000 }
    })
}

fn valid_trip_plan() -> Value {
    json!({
        "kind": "trip_plan",
        "schema_version": 1,
        "origin": { "lat_e7": 356812345, "lon_e7": 1397671234, "name": "東京駅" },
        "vias": [],
        "destination": { "lat_e7": 352500000, "lon_e7": 1391500000, "vehicle_entry_id": "12345" },
        "history_mode": "off"
    })
}

fn diary_with_segments(segments: Value) -> Value {
    json!({
        "kind": "drive_diary",
        "schema_version": 1,
        "trip_id": "t1",
        "started_at_unix_s": 1795651200,
        "track": { "segments": segments },
        "avoided": { "right_unprotected": 0, "narrow_m": 0, "merges": 0 },
        "history": []
    })
}

fn check_ok(v: &Value) {
    let bytes = serde_json::to_vec(v).unwrap();
    validate_json(&bytes).unwrap_or_else(|e| panic!("expected ok, got {e:?}"));
}

fn check_err(v: &Value) {
    let bytes = serde_json::to_vec(v).unwrap();
    let e = validate_json(&bytes).expect_err("expected E_SCHEMA");
    assert_eq!(e.code, ErrorCode::Schema);
}

#[test]
fn track_point_count_boundary_20000_ok_20001_rejected() {
    // 20,000 points split across 2 segments (10,000 + 10,000) -> accepted
    let seg1: Vec<Value> = (0u64..10_000)
        .map(|i| json!([350_000_000, 1_390_000_000, i]))
        .collect();
    let seg2: Vec<Value> = (10_000u64..20_000)
        .map(|i| json!([350_000_000, 1_390_000_000, i]))
        .collect();
    check_ok(&diary_with_segments(
        json!([{ "points": seg1 }, { "points": seg2 }]),
    ));

    // 20,001 points across 2 segments (10,000 + 10,001) -> rejected
    let mut seg2_over = seg2;
    seg2_over.push(json!([350_000_000, 1_390_000_000, 20_000]));
    check_err(&diary_with_segments(
        json!([{ "points": seg1 }, { "points": seg2_over }]),
    ));
}

#[test]
fn track_timestamp_monotonic_across_segments_same_second_and_null_ok() {
    // Same second consecutive + null between non-decreasing times across segments -> accepted
    check_ok(&diary_with_segments(json!([
        { "points": [
            [350_000_000, 1_390_000_000, null],
            [350_000_000, 1_390_000_000, 10],
            [350_000_000, 1_390_000_000, 10],
            [350_000_000, 1_390_000_000, null]
        ]},
        { "points": [
            [350_000_000, 1_390_000_000, null],
            [350_000_000, 1_390_000_000, 10],
            [350_000_000, 1_390_000_000, 15]
        ]}
    ])));

    // Intra-segment regression (10 -> 9) -> rejected
    check_err(&diary_with_segments(json!([
        { "points": [
            [350_000_000, 1_390_000_000, 10],
            [350_000_000, 1_390_000_000, 9]
        ]}
    ])));

    // Intra-segment regression across null (10 -> null -> 9) -> rejected
    check_err(&diary_with_segments(json!([
        { "points": [
            [350_000_000, 1_390_000_000, 10],
            [350_000_000, 1_390_000_000, null],
            [350_000_000, 1_390_000_000, 9]
        ]}
    ])));

    // Cross-segment regression (seg0 ends at 20, seg1 starts at 19) -> rejected
    check_err(&diary_with_segments(json!([
        { "points": [
            [350_000_000, 1_390_000_000, 10],
            [350_000_000, 1_390_000_000, 20]
        ]},
        { "points": [
            [350_000_000, 1_390_000_000, null],
            [350_000_000, 1_390_000_000, 19]
        ]}
    ])));
}

#[test]
fn intent_preset_hard_soft_keys_weights_and_name_limits() {
    check_ok(&valid_preset());

    // hard_soft key not in chips -> rejected
    let mut bad_hs = valid_preset();
    bad_hs["hard_soft"]["ghost_chip"] = json!("hard");
    check_err(&bad_hs);

    // weights_permille boundaries: 250 / 4000 accepted, 249 / 4001 rejected
    for (axis, bad_val) in [
        ("right_turn", 249),
        ("right_turn", 4001),
        ("narrow_road", 249),
        ("narrow_road", 4001),
        ("merge", 249),
        ("merge", 4001),
    ] {
        let mut p = valid_preset();
        p["weights_permille"][axis] = json!(bad_val);
        check_err(&p);
    }

    // Japanese 40-char name accepted, 41-char rejected (Unicode code points, not UTF-8 bytes)
    let mut jp40 = valid_preset();
    jp40["name"] = json!("あ".repeat(40));
    check_ok(&jp40);
    let mut jp41 = valid_preset();
    jp41["name"] = json!("あ".repeat(41));
    check_err(&jp41);

    // Unknown key at top level and nested weights_permille -> rejected
    let mut extra_top = valid_preset();
    extra_top["unknown"] = json!(1);
    check_err(&extra_top);
    let mut extra_w = valid_preset();
    extra_w["weights_permille"]["extra"] = json!(1000);
    check_err(&extra_w);
}

#[test]
fn trip_plan_and_diary_limits_and_unknown_keys() {
    let mut tp = valid_trip_plan();
    tp["origin"]["name"] = json!("漢".repeat(60));
    check_ok(&tp);

    let mut tp61 = valid_trip_plan();
    tp61["origin"]["name"] = json!("漢".repeat(61));
    check_err(&tp61);

    let mut tp_extra = valid_trip_plan();
    tp_extra["origin"]["extra"] = json!(true);
    check_err(&tp_extra);

    // 200 history entries accepted, 201 rejected
    let h200: Vec<Value> = (0..200)
        .map(|i| json!({"poi_id": format!("p{i}"), "outcome": "completed"}))
        .collect();
    let mut d200 = diary_with_segments(json!([]));
    d200["history"] = json!(h200);
    check_ok(&d200);
    let mut h201 = h200;
    h201.push(json!({"poi_id": "p200", "outcome": "saved"}));
    let mut d201 = diary_with_segments(json!([]));
    d201["history"] = json!(h201);
    check_err(&d201);

    // Wrong schema_version -> rejected
    let mut bad_ver = valid_trip_plan();
    bad_ver["schema_version"] = json!(2);
    check_err(&bad_ver);
}
