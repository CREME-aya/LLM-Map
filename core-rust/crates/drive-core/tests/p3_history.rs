//! GPX-replay-style tests for P3 history commentary (docs 05 §6.2, ADR-0008, `f9d629a`).
//!
//! Straight eastbound route at 20 m/s (72 km/h) unless customized per test:
//! 10 km = 1,000,000 cm, 500 s = 500,000 ms (1 ms = 2 cm), `k_lon_q16 = 65536` so
//! `lon_e7 = origin_lon + x_cm` is exact with zero fixed-point rounding error.
use contracts::facts::{AvoidOfferEvalRequest, AvoidOfferEvalResult, CandidateFacts};
use contracts::horizon::{Arrow, Evidence, HorizonStatus, Maneuver, ManeuverId, RouteAnchor};
use contracts::override_types::{RejectCode, ValidateOverrideRequest, ValidateOverrideResult};
use contracts::route::{
    HistoryPoi, HistoryVersion, HistoryVersionKind, RouteBundle, RouteCommitRequest,
};
use contracts::tick::{
    DismissReason, Event, EventBody, Fix, PlannerCmd, SaveForLaterReason, SearchOutcome, TextClass,
    ThermalStatus, TickInput, TickOutput, UiCmd, VoiceCmd,
};
use contracts::{ErrorCode, RouteKey};
use drive_core::avoid;
use drive_core::history::{self, PoiState};
use drive_core::{Session, SessionConfig};
use serde_json::{json, Value};

const BASE_LON: i32 = 1_390_000_000;
const BASE_LAT: i32 = 350_000_000;

fn ev(kind: &str, value: Value) -> Evidence {
    let mut m = serde_json::Map::new();
    m.insert("kind".into(), json!(kind));
    match kind {
        "confirmed" => {
            m.insert("value".into(), value);
            m.insert("source_ref".into(), json!("osm:node/1"));
        }
        "unknown" => {
            m.insert("reason".into(), json!("missing"));
        }
        _ => unreachable!(),
    }
    serde_json::from_value(Value::Object(m)).unwrap()
}

fn plain_maneuver(
    rk: &RouteKey,
    ordinal: u32,
    shape_idx: u32,
    along_cm: u32,
    arrow: Arrow,
) -> Maneuver {
    Maneuver {
        id: ManeuverId {
            route_key: rk.clone(),
            leg_index: 0,
            ordinal,
        },
        anchor: RouteAnchor {
            leg_index: 0,
            leg_shape_index: shape_idx,
            along_route_cm: along_cm,
        },
        arrow,
        junction_name: None,
        road_name: None,
        route_ref: None,
        side_of_traffic: contracts::horizon::SideOfTraffic::Left,
        crossing: ev("confirmed", json!("no_crossing")),
        approach_signal: ev("unknown", Value::Null),
        right_turn_protection: ev("unknown", Value::Null),
        approach_width_mm: ev("confirmed", json!(7000)),
        departure_width_mm: ev("confirmed", json!(7000)),
        roundabout_leave_anchor: None,
        advisory_flags: vec![],
    }
}

fn complex_right_maneuver(rk: &RouteKey, ordinal: u32, shape_idx: u32, along_cm: u32) -> Maneuver {
    let mut m = plain_maneuver(rk, ordinal, shape_idx, along_cm, Arrow::Right);
    m.crossing = ev("confirmed", json!("across_oncoming"));
    m.approach_signal = ev("confirmed", json!("absent"));
    m.right_turn_protection = ev("confirmed", json!("unprotected"));
    m
}

fn poi(poi_id: &str, along_route_cm: u32, short_ms: u32, summary_ms: u32) -> HistoryPoi {
    HistoryPoi {
        poi_id: poi_id.into(),
        along_route_cm,
        versions: vec![
            HistoryVersion {
                kind: HistoryVersionKind::Short,
                content_ref: format!("pack:{poi_id}:short"),
                duration_ms: short_ms,
            },
            HistoryVersion {
                kind: HistoryVersionKind::Summary,
                content_ref: format!("pack:{poi_id}:summary"),
                duration_ms: summary_ms,
            },
        ],
    }
}

/// Straight eastbound route of `len_km` km at 20 m/s (1 km = 50 s = 50,000 ms).
/// Shape points every 1 km (index i -> i * 100_000 cm).
fn straight_route(len_km: u32, history_pois: Vec<HistoryPoi>) -> RouteBundle {
    let length_cm = len_km * 100_000;
    let eta_ms = len_km * 50_000;
    let pts: Vec<Value> = (0..=len_km).map(|k| json!([k * 100_000, 0])).collect();
    let alongs: Vec<u32> = (0..=len_km).map(|k| k * 100_000).collect();
    let bearings: Vec<u16> = vec![9_000; len_km as usize];
    let edges: Vec<Value> = (0..len_km)
        .map(|k| {
            json!({
                "edge_id": (1000 + k).to_string(),
                "from_pt": k,
                "to_pt": k + 1,
                "length_cm": 100_000,
                "cum_time_ms": (k + 1) * 50_000,
                "flags": 0
            })
        })
        .collect();
    let rk: RouteKey = serde_json::from_value(json!({
        "trip_id": "trip-hist",
        "trip_revision": "1",
        "route_revision": "0",
        "graph_version": "jp-2026.11.0"
    }))
    .unwrap();
    let arrive = plain_maneuver(&rk, 0, len_km, length_cm, Arrow::Arrive);
    let mut bundle: RouteBundle = serde_json::from_value(json!({
        "route_key": rk,
        "eta_ms": eta_ms,
        "length_cm": length_cm,
        "legs": [{
            "leg_index": 0,
            "along_route_start_cm": 0,
            "length_cm": length_cm,
            "chunks": [{
                "first_shape_index": 0,
                "origin_e7": {"lat": BASE_LAT, "lon": BASE_LON},
                "k_lat_q16": 65536,
                "k_lon_q16": 65536,
                "points_cm": pts,
                "seg_along_cm": alongs,
                "bearing_cdeg": bearings
            }],
            "edges": edges,
            "maneuvers": [arrive]
        }]
    }))
    .unwrap();
    bundle.history_pois = history_pois;
    bundle
}

fn fix_xy(at: u64, x_cm: i32, y_cm: i32) -> Fix {
    Fix {
        at_mono_ms: at,
        lat_e7: BASE_LAT + y_cm,
        lon_e7: BASE_LON + x_cm,
        acc_cm: 500,
        bearing_cdeg: Some(9_000),
        speed_cm_s: Some(2_000),
    }
}

fn fix_at(at: u64, x_cm: i32) -> Fix {
    fix_xy(at, x_cm, 0)
}

fn session_with(cfg: SessionConfig, bundle: &RouteBundle) -> (Session, RouteKey) {
    let mut s = Session::new(cfg);
    s.record_plan_ok_for_test("plan-1", bundle.route_key.clone());
    let res = s
        .route_commit(&RouteCommitRequest {
            req_id: "plan-1".into(),
            expected_route_key: None,
            bundle: bundle.clone(),
        })
        .unwrap();
    (s, res.route_key)
}

fn tick(s: &mut Session, now: u64, fix: Option<Fix>, events: Vec<Event>) -> TickOutput {
    s.tick(TickInput {
        now_mono_ms: now,
        fix,
        events,
    })
}

fn consent_ev(at: u64, on: bool) -> Event {
    Event {
        at_mono_ms: at,
        body: EventBody::HistoryConsent { on },
    }
}

fn bmk(s: &mut Session, utterance_mono_ms: u64) -> ValidateOverrideResult {
    s.validate_override(ValidateOverrideRequest {
        code_line: "BMK\n".into(),
        contract: json!({}),
        utterance_id: format!("u-{utterance_mono_ms}"),
        utterance_mono_ms,
        ref_frame_seq: s.frame_seq(),
        pending_offer_id: None,
    })
    .unwrap()
}

fn speaks_content(out: &TickOutput) -> Vec<(&str, &str)> {
    out.voice_cmds
        .iter()
        .filter_map(|v| match v {
            VoiceCmd::SpeakContent { token, content_ref } => {
                Some((token.as_str(), content_ref.as_str()))
            }
            _ => None,
        })
        .collect()
}

fn stops(out: &TickOutput) -> Vec<&str> {
    out.voice_cmds
        .iter()
        .filter_map(|v| match v {
            VoiceCmd::Stop { token } => Some(token.as_str()),
            _ => None,
        })
        .collect()
}

fn saves(out: &TickOutput) -> Vec<(&str, SaveForLaterReason)> {
    out.ui_cmds
        .iter()
        .filter_map(|u| match u {
            UiCmd::SaveForLater { poi_id, reason } => Some((poi_id.as_str(), *reason)),
            _ => None,
        })
        .collect()
}

// 1. 同意 OFF では話さない
#[test]
fn consent_off_never_speaks() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    assert!(!s.history_consent());
    // Enter corridor [100_000, 250_000] without consent.
    for (t, x) in [(1_000, 100_000), (2_000, 150_000), (3_000, 240_000)] {
        let out = tick(&mut s, t, Some(fix_at(t, x)), vec![]);
        assert!(
            out.voice_cmds.is_empty(),
            "must not speak while consent OFF"
        );
        assert_eq!(s.history_poi_state("castle"), Some(&PoiState::NotReached));
    }
}

// 2. 同意 ON で手前 2 km から Eligible になる（2 件同時なら 2 件目が Eligible のまま待つ）
#[test]
fn consent_on_becomes_eligible_at_2km_and_speaks() {
    let b = straight_route(
        10,
        vec![
            poi("castle", 300_000, 12_000, 5_000),
            poi("shrine", 305_000, 12_000, 5_000),
        ],
    );
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    // 2.0001 km before castle -> NotReached
    let out0 = tick(
        &mut s,
        1_000,
        Some(fix_at(1_000, 99_999)),
        vec![consent_ev(1_000, true)],
    );
    assert!(s.history_consent());
    assert!(out0.voice_cmds.is_empty());
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::NotReached));
    assert_eq!(s.history_poi_state("shrine"), Some(&PoiState::NotReached));

    // Exactly 2.0 km before castle (x = 100_000) -> castle becomes Eligible -> Pending -> Speaking
    let out1 = tick(&mut s, 2_000, Some(fix_at(2_000, 100_000)), vec![]);
    let sp = speaks_content(&out1);
    assert_eq!(sp.len(), 1);
    assert_eq!(sp[0].1, "pack:castle:short");
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Speaking { .. })
    ));
    assert!(out1.log.iter().any(|l| l.what == "p3_eligible"));
    // shrine is still 2.05 km ahead -> NotReached
    assert_eq!(s.history_poi_state("shrine"), Some(&PoiState::NotReached));

    // At x = 105_000 (2.0 km before shrine while castle is Speaking) -> shrine enters Eligible
    let out2 = tick(&mut s, 3_000, Some(fix_at(3_000, 105_000)), vec![]);
    assert!(out2.voice_cmds.is_empty());
    assert_eq!(s.history_poi_state("shrine"), Some(&PoiState::Eligible));
}

// 3. 手前 0.5 km を過ぎたら Expired になる
#[test]
fn passed_half_km_becomes_expired() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    // Block speaking with other TTS so POI stays Pending inside corridor
    let out0 = tick(
        &mut s,
        1_000,
        Some(fix_at(1_000, 250_000)), // exactly 0.5 km ahead -> still in corridor (Pending)
        vec![
            consent_ev(1_000, true),
            Event {
                at_mono_ms: 1_000,
                body: EventBody::TtsStarted {
                    token: "other-tts".into(),
                },
            },
        ],
    );
    assert!(out0.voice_cmds.is_empty());
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Pending { .. })
    ));

    // Move to 250_001 cm (499.99 m ahead, past 0.5 km) and release other TTS -> Expired!
    let out1 = tick(
        &mut s,
        2_000,
        Some(fix_at(2_000, 250_001)),
        vec![Event {
            at_mono_ms: 2_000,
            body: EventBody::TtsDone {
                token: "other-tts".into(),
            },
        }],
    );
    assert!(out1.voice_cmds.is_empty());
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Expired));
}

// 4. 複雑窓（next_complex_window）の中では待ち、抜けたら話す
#[test]
fn waits_inside_complex_window_and_speaks_after() {
    // 10 km route, complex unprotected right turn at 1 km (100_000 cm -> 50 s from start).
    // At x = 80_000 (200 m before turn, typical = 10 s, earliest = 8.5 s, prep = 30 s),
    // vehicle is INSIDE the complex window [start = now + 8.5s - 30s <= now, end = now + 12.5s + 5s].
    // POI is at 250_000 (2.5 km) so corridor is [50_000, 200_000].
    let mut b = straight_route(10, vec![poi("castle", 250_000, 12_000, 5_000)]);
    let rk = b.route_key.clone();
    let arrive = b.legs[0].maneuvers.pop().unwrap();
    let mut arrive = arrive;
    arrive.id.ordinal = 1;
    b.legs[0]
        .maneuvers
        .push(complex_right_maneuver(&rk, 0, 1, 100_000));
    b.legs[0].maneuvers.push(arrive);

    let (mut s, _) = session_with(
        SessionConfig {
            fix_stale_ms: 100_000,
            ..Default::default()
        },
        &b,
    );

    // At t = 100_000, x = 80_000 (inside castle's corridor AND inside complex window of turn at 100_000)
    let out0 = tick(
        &mut s,
        100_000,
        Some(fix_at(100_000, 80_000)),
        vec![consent_ev(100_000, true)],
    );
    let h0 = out0.horizon.as_ref().unwrap();
    let w0 = h0.next_complex_window.unwrap();
    assert!(100_000 >= w0.start_monotonic_ms);
    assert!(
        out0.voice_cmds.is_empty(),
        "must wait inside complex window"
    );
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Pending { .. })
    ));

    // Pass the turn (2 consecutive samples >= 100_000 at t = 105_000, 106_000)
    let _ = tick(&mut s, 105_000, Some(fix_at(105_000, 105_000)), vec![]);
    let out_pass = tick(&mut s, 106_000, Some(fix_at(106_000, 110_000)), vec![]);
    // Kept window still active until w0.end_monotonic_ms (around 100_000 + 12_500 + 5_000 = 117_500)
    let end = out_pass
        .horizon
        .as_ref()
        .and_then(|h| h.next_complex_window)
        .unwrap()
        .end_monotonic_ms
        .unwrap();
    assert!(out_pass.voice_cmds.is_empty());
    assert!(out_pass.next_wake_mono_ms.is_some_and(|w| w <= end + 1));

    // After complex window ends (end + 1 <= 100_000 + 30_000, still within 30 s Pending limit) -> speaks!
    let t_after = end + 1;
    let out_after = tick(&mut s, t_after, Some(fix_at(t_after, 120_000)), vec![]);
    let sp = speaks_content(&out_after);
    assert_eq!(sp.len(), 1);
    assert_eq!(sp[0].1, "pack:castle:short");
}

// 5. short が利用可能時間に入らず summary を選ぶ
#[test]
fn selects_summary_when_short_does_not_fit() {
    // 10 km route. Add a plain left maneuver at 2 km (200_000 cm -> 100 s).
    // At x = 152_000 (480 m before turn -> remaining 24,000 ms to turn):
    //   P1 deadline = now + 24,000 - 8,000 = now + 16,000 ms
    //   available = 16,000 - 800 (tts_prep) - 5,000 (safety) = 10,200 ms.
    // short = 12,000 ms (does NOT fit), summary = 5,000 ms (fits!).
    // POI at 300_000 -> corridor [100_000, 250_000], x = 152_000 is inside.
    let mut b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let rk = b.route_key.clone();
    let mut arrive = b.legs[0].maneuvers.pop().unwrap();
    arrive.id.ordinal = 1;
    b.legs[0]
        .maneuvers
        .push(plain_maneuver(&rk, 0, 2, 200_000, Arrow::Left));
    b.legs[0].maneuvers.push(arrive);

    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 152_000)),
        vec![consent_ev(10_000, true)],
    );
    let sp = speaks_content(&out);
    assert_eq!(sp.len(), 1);
    assert_eq!(sp[0].1, "pack:castle:summary");
}

// 6. short も summary も入らず待つ
#[test]
fn waits_when_neither_short_nor_summary_fits() {
    // Plain left maneuver at 2 km (200_000 cm -> 100 s).
    // At x = 164_000 (360 m before turn -> remaining 18,000 ms):
    //   P1 deadline = now + 18,000 - 8,000 = now + 10,000 ms
    //   available = 10,000 - 800 - 5,000 = 4,200 ms.
    // Neither short (12,000) nor summary (5,000) fits -> stays Pending.
    let mut b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let rk = b.route_key.clone();
    let mut arrive = b.legs[0].maneuvers.pop().unwrap();
    arrive.id.ordinal = 1;
    b.legs[0]
        .maneuvers
        .push(plain_maneuver(&rk, 0, 2, 200_000, Arrow::Left));
    b.legs[0].maneuvers.push(arrive);

    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 164_000)),
        vec![consent_ev(10_000, true)],
    );
    assert!(out.voice_cmds.is_empty());
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Pending {
            since_mono_ms: 10_000
        })
    ));
}

// 7. Speaking 中に P1 割り込み → stop と save_for_later{poi_id, reason: interrupted} が出て再開しない
#[test]
fn p1_deadline_interrupts_speaking_and_saves_for_later() {
    // Plain left turn at 2 km (200_000 cm). Start speaking at x = 140_000 (30 s to turn, P1 dl = now + 22 s -> short fits).
    let mut b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let rk = b.route_key.clone();
    let mut arrive = b.legs[0].maneuvers.pop().unwrap();
    arrive.id.ordinal = 1;
    b.legs[0]
        .maneuvers
        .push(plain_maneuver(&rk, 0, 2, 200_000, Arrow::Left));
    b.legs[0].maneuvers.push(arrive);

    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 140_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok = speaks_content(&out0)[0].0.to_owned();
    let p1_dl = out0
        .horizon
        .as_ref()
        .unwrap()
        .next_p1_deadline_monotonic_ms
        .unwrap();
    assert_eq!(p1_dl, 32_000);
    // next_wake includes p1_dl (or earlier stale_at)
    assert!(out0.next_wake_mono_ms.is_some());

    // Vehicle accelerates and reaches x = 185_000 (7.5 s before turn -> P1 deadline <= now!) while still Speaking
    let out1 = tick(&mut s, 15_000, Some(fix_at(15_000, 185_000)), vec![]);
    assert_eq!(stops(&out1), vec![tok.as_str()]);
    assert_eq!(
        saves(&out1),
        vec![("castle", SaveForLaterReason::Interrupted)]
    );
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Interrupted));

    // Even after passing the turn, same POI never resumes!
    let _ = tick(&mut s, 20_000, Some(fix_at(20_000, 205_000)), vec![]);
    let out_later = tick(&mut s, 21_000, Some(fix_at(21_000, 210_000)), vec![]);
    assert!(out_later.voice_cmds.is_empty());
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Interrupted));
}

// 7b. Speaking 中に外部 P0/P1 tts_started で割り込み → stop と save_for_later{interrupted}
#[test]
fn external_p0_p1_tts_started_interrupts_speaking() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok = speaks_content(&out0)[0].0.to_owned();

    // P3 tts_started echo does NOT interrupt itself
    let out_echo = tick(
        &mut s,
        10_500,
        Some(fix_at(10_500, 151_000)),
        vec![Event {
            at_mono_ms: 10_500,
            body: EventBody::TtsStarted { token: tok.clone() },
        }],
    );
    assert!(out_echo.voice_cmds.is_empty());
    assert!(out_echo.ui_cmds.is_empty());

    // External P0/P1 tts_started interrupts P3
    let out_int = tick(
        &mut s,
        11_000,
        Some(fix_at(11_000, 152_000)),
        vec![Event {
            at_mono_ms: 11_000,
            body: EventBody::TtsStarted {
                token: "p1-reroute".into(),
            },
        }],
    );
    assert_eq!(stops(&out_int), vec![tok.as_str()]);
    assert_eq!(
        saves(&out_int),
        vec![("castle", SaveForLaterReason::Interrupted)]
    );
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Interrupted));
}

// 8. 同意 OFF で話し中 → stop が出て save_for_later は出ない（Eligible/Pending も Expired）
#[test]
fn consent_off_while_speaking_stops_without_save_for_later() {
    let b = straight_route(
        10,
        vec![
            poi("castle", 300_000, 12_000, 5_000),
            poi("shrine", 305_000, 12_000, 5_000),
        ],
    );
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 105_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok = speaks_content(&out0)[0].0.to_owned();
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Speaking { .. })
    ));
    assert_eq!(s.history_poi_state("shrine"), Some(&PoiState::Eligible));

    let out1 = tick(
        &mut s,
        11_000,
        Some(fix_at(11_000, 107_000)),
        vec![consent_ev(11_000, false)],
    );
    assert_eq!(stops(&out1), vec![tok.as_str()]);
    assert!(
        saves(&out1).is_empty(),
        "consent OFF must NOT emit save_for_later"
    );
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Expired));
    assert_eq!(s.history_poi_state("shrine"), Some(&PoiState::Expired));
    assert!(!s.history_consent());
}

// 9. 同じ POI の 2 回目は話さない
#[test]
fn same_poi_speaks_only_once_per_trip() {
    let b = straight_route(
        15,
        vec![
            poi("castle", 300_000, 12_000, 5_000),
            poi("castle", 1_200_000, 12_000, 5_000),
        ],
    );
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok = speaks_content(&out0)[0].0.to_owned();
    // Finish speaking
    let _ = tick(
        &mut s,
        22_000,
        Some(fix_at(22_000, 174_000)),
        vec![Event {
            at_mono_ms: 22_000,
            body: EventBody::TtsDone { token: tok },
        }],
    );
    assert!(s.history_has_spoken("castle"));
    assert_eq!(s.history_spoken_count(), 1);

    // > 5 minutes later, reach second corridor for the same poi_id -> must NOT speak!
    let out_later = tick(&mut s, 400_000, Some(fix_at(400_000, 1_050_000)), vec![]);
    assert!(out_later.voice_cmds.is_empty());
    assert_eq!(s.history_spoken_count(), 1);
}

// 10. 前の開始から 5 分以内の 2 つ目の POI は待ち、5 分経てば話せる
#[test]
fn five_minute_interval_waits_then_speaks() {
    let b = straight_route(
        15,
        vec![
            poi("castle", 300_000, 12_000, 5_000),
            poi("shrine", 1_000_000, 12_000, 5_000),
        ],
    );
    let (mut s, _) = session_with(
        SessionConfig {
            fix_stale_ms: 100_000,
            ..Default::default()
        },
        &b,
    );
    // Start castle at t = 10_000
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok0 = speaks_content(&out0)[0].0.to_owned();
    // Finish castle at t = 22_000
    let _ = tick(
        &mut s,
        22_000,
        Some(fix_at(22_000, 174_000)),
        vec![Event {
            at_mono_ms: 22_000,
            body: EventBody::TtsDone { token: tok0 },
        }],
    );

    // Enter shrine corridor at t = 300_000 (290 s since castle started < 300 s) -> waits in Pending!
    let out_wait = tick(&mut s, 300_000, Some(fix_at(300_000, 850_000)), vec![]);
    assert!(out_wait.voice_cmds.is_empty());
    assert!(matches!(
        s.history_poi_state("shrine"),
        Some(PoiState::Pending { .. })
    ));
    // next_wake must schedule at or before 310_000 (10_000 + 300_000)
    assert_eq!(out_wait.next_wake_mono_ms, Some(310_000));

    // At t = 309_999 (still 1 ms short of 5 min) -> still waiting
    let out_almost = tick(&mut s, 309_999, None, vec![]);
    assert!(out_almost.voice_cmds.is_empty());

    // At t = 310_000 (exact 5 min) -> speaks shrine!
    let out_go = tick(&mut s, 310_000, None, vec![]);
    let sp = speaks_content(&out_go);
    assert_eq!(sp.len(), 1);
    assert_eq!(sp[0].1, "pack:shrine:short");
    assert_eq!(s.history_spoken_count(), 2);
}

// 11. 経路が変わっても（dc_route_commit 後も）同じ POI 1 旅 1 回・5 分間隔・同意状態は保持される
#[test]
fn route_change_preserves_consent_spoken_pois_and_interval() {
    let b1 = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, rk1) = session_with(
        SessionConfig {
            fix_stale_ms: 100_000,
            ..Default::default()
        },
        &b1,
    );
    // Consent ON, speak castle at t = 10_000, finish at t = 22_000
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok0 = speaks_content(&out0)[0].0.to_owned();
    let _ = tick(
        &mut s,
        22_000,
        Some(fix_at(22_000, 174_000)),
        vec![Event {
            at_mono_ms: 22_000,
            body: EventBody::TtsDone { token: tok0 },
        }],
    );

    // Replace route via dc_route_commit with new bundle containing BOTH castle (already spoken) and shrine (new)
    let b2 = straight_route(
        10,
        vec![
            poi("castle", 300_000, 12_000, 5_000),
            poi("shrine", 500_000, 12_000, 5_000),
        ],
    );
    s.issue_commit_for_test("reroute-1", rk1.clone());
    s.route_commit(&RouteCommitRequest {
        req_id: "reroute-1".into(),
        expected_route_key: Some(rk1),
        bundle: b2,
    })
    .unwrap();

    // Consent persisted!
    assert!(s.history_consent());
    assert!(s.history_has_spoken("castle"));

    // Enter castle corridor on route 2 -> must NOT speak castle again
    let out_castle = tick(&mut s, 290_000, Some(fix_at(290_000, 150_000)), vec![]);
    assert!(out_castle.voice_cmds.is_empty());

    // Enter shrine corridor at t = 300_000 (< 310_000, 5 min from 10_000) -> must wait!
    let out_shrine_wait = tick(&mut s, 300_000, Some(fix_at(300_000, 350_000)), vec![]);
    assert!(out_shrine_wait.voice_cmds.is_empty());
    assert!(matches!(
        s.history_poi_state("shrine"),
        Some(PoiState::Pending { .. })
    ));

    // At t = 310_000 (5 min elapsed across the route change) -> speaks shrine!
    let out_shrine_go = tick(&mut s, 310_000, None, vec![]);
    let sp = speaks_content(&out_shrine_go);
    assert_eq!(sp.len(), 1);
    assert_eq!(sp[0].1, "pack:shrine:short");
}

// 12. Pending のまま 30 s 超で Expired
#[test]
fn pending_expires_after_30s() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(
        SessionConfig {
            fix_stale_ms: 100_000,
            ..Default::default()
        },
        &b,
    );
    // Block with call_state{active: true} so POI enters Pending at t = 10_000
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![
            consent_ev(10_000, true),
            Event {
                at_mono_ms: 10_000,
                body: EventBody::CallState { active: true },
            },
        ],
    );
    assert!(out0.voice_cmds.is_empty());
    assert_eq!(
        s.history_poi_state("castle"),
        Some(&PoiState::Pending {
            since_mono_ms: 10_000
        })
    );
    // Wake scheduled at 10_000 + 30_000 + 1 = 40_001
    assert_eq!(out0.next_wake_mono_ms, Some(40_001));

    // At exactly 30 s (t = 40_000) -> still Pending
    let out_30 = tick(&mut s, 40_000, None, vec![]);
    assert!(out_30.voice_cmds.is_empty());
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Pending { .. })
    ));

    // At 30.001 s (t = 40_001) -> Expired even if call ends now
    let out_31 = tick(
        &mut s,
        40_001,
        None,
        vec![Event {
            at_mono_ms: 40_001,
            body: EventBody::CallState { active: false },
        }],
    );
    assert!(out_31.voice_cmds.is_empty());
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Expired));
}

// 13. Pending → Speaking のその他条件（thermal / call / audio_focus / acc / bearing / user stop）
#[test]
fn pending_preconditions_and_user_stop() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);

    // Thermal moderate blocks speaking
    let _ = tick(
        &mut s,
        1_000,
        Some(fix_at(1_000, 150_000)),
        vec![
            consent_ev(1_000, true),
            Event {
                at_mono_ms: 1_000,
                body: EventBody::Thermal {
                    status: ThermalStatus::Moderate,
                },
            },
        ],
    );
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Pending { .. })
    ));

    // Return thermal to light, but fix has no bearing -> still blocked
    let mut no_bearing = fix_at(2_000, 152_000);
    no_bearing.bearing_cdeg = None;
    let out_nb = tick(
        &mut s,
        2_000,
        Some(no_bearing),
        vec![Event {
            at_mono_ms: 2_000,
            body: EventBody::Thermal {
                status: ThermalStatus::Light,
            },
        }],
    );
    assert!(out_nb.voice_cmds.is_empty());

    // Fix with low speed (< heading_min_speed_cm_s = 140) -> bearing not usable -> blocked
    let mut slow = fix_at(3_000, 154_000);
    slow.speed_cm_s = Some(100);
    let out_slow = tick(&mut s, 3_000, Some(slow), vec![]);
    assert!(out_slow.voice_cmds.is_empty());

    // Good fix -> starts Speaking!
    let out_ok = tick(&mut s, 4_000, Some(fix_at(4_000, 156_000)), vec![]);
    let tok = speaks_content(&out_ok)[0].0.to_owned();

    // User says dict "やめて" -> Stop without save_for_later, state becomes Expired
    let out_stop = tick(
        &mut s,
        5_000,
        Some(fix_at(5_000, 158_000)),
        vec![Event {
            at_mono_ms: 5_000,
            body: EventBody::AsrResult {
                utterance_id: "u-stop".into(),
                text_class: TextClass::Dict,
                dict_norm: Some("やめて".into()),
            },
        }],
    );
    assert_eq!(stops(&out_stop), vec![tok.as_str()]);
    assert!(saves(&out_stop).is_empty());
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Expired));

    // audio_focus_lost blocks speaking on a fresh session
    let (mut s2, _) = session_with(SessionConfig::default(), &b);
    let out_af = tick(
        &mut s2,
        1_000,
        Some(fix_at(1_000, 150_000)),
        vec![
            consent_ev(1_000, true),
            Event {
                at_mono_ms: 1_000,
                body: EventBody::AudioFocusLost {},
            },
        ],
    );
    assert!(out_af.voice_cmds.is_empty());
    assert!(matches!(
        s2.history_poi_state("castle"),
        Some(PoiState::Pending { .. })
    ));
}

// 14. BMK: 話し中（Speaking）の BMK → save_for_later{poi_id, reason: bmk}
#[test]
fn bmk_speaking_saves_for_later() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    assert_eq!(speaks_content(&out0).len(), 1);

    assert_eq!(
        bmk(&mut s, 12_000),
        ValidateOverrideResult::Bookmarked {
            poi_id: "castle".into()
        }
    );
    let out1 = tick(&mut s, 12_500, Some(fix_at(12_500, 155_000)), vec![]);
    assert_eq!(saves(&out1), vec![("castle", SaveForLaterReason::Bmk)]);
}

// 15. BMK: 話し終えて（Completed）59 s で保存、61 s で NO_PENDING_TARGET
#[test]
fn bmk_completed_59s_saves_and_61s_rejects() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok = speaks_content(&out0)[0].0.to_owned();
    // Completed at t = 22_000
    let _ = tick(
        &mut s,
        22_000,
        Some(fix_at(22_000, 174_000)),
        vec![Event {
            at_mono_ms: 22_000,
            body: EventBody::TtsDone { token: tok },
        }],
    );
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Completed));

    // 59 s after completion (22_000 + 59_000 = 81_000) -> saved
    assert_eq!(
        bmk(&mut s, 81_000),
        ValidateOverrideResult::Bookmarked {
            poi_id: "castle".into()
        }
    );
    let out_59 = tick(&mut s, 81_100, None, vec![]);
    assert_eq!(saves(&out_59), vec![("castle", SaveForLaterReason::Bmk)]);

    // 61 s after completion (22_000 + 61_000 = 83_000) -> NO_PENDING_TARGET
    assert_eq!(
        bmk(&mut s, 83_000),
        ValidateOverrideResult::Rejected {
            code: RejectCode::NoPendingTarget
        }
    );
    let out_61 = tick(&mut s, 83_100, None, vec![]);
    assert!(saves(&out_61).is_empty());
}

// 16. BMK: 割り込まれた（Interrupted）直後の BMK → save_for_later{poi_id, reason: bmk}
#[test]
fn bmk_interrupted_saves_for_later() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let _ = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    // Interrupt at t = 15_000
    let out_int = tick(
        &mut s,
        15_000,
        Some(fix_at(15_000, 160_000)),
        vec![Event {
            at_mono_ms: 15_000,
            body: EventBody::TtsStarted {
                token: "p1-turn".into(),
            },
        }],
    );
    assert_eq!(
        saves(&out_int),
        vec![("castle", SaveForLaterReason::Interrupted)]
    );

    // BMK right after interruption (t = 16_000) -> saved with reason: bmk
    assert_eq!(
        bmk(&mut s, 16_000),
        ValidateOverrideResult::Bookmarked {
            poi_id: "castle".into()
        }
    );
    let out_bmk = tick(&mut s, 16_500, Some(fix_at(16_500, 163_000)), vec![]);
    assert_eq!(saves(&out_bmk), vec![("castle", SaveForLaterReason::Bmk)]);
}

// 17. BMK: 何も話していない状態での BMK（未発話 POI が近くにあっても推測せず NO_PENDING_TARGET）
#[test]
fn bmk_without_target_rejects_no_pending_target() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    // Even with vehicle tracking near POI (consent OFF, never spoken), BMK must not guess
    let _ = tick(&mut s, 10_000, Some(fix_at(10_000, 150_000)), vec![]);
    assert_eq!(
        bmk(&mut s, 10_500),
        ValidateOverrideResult::Rejected {
            code: RejectCode::NoPendingTarget
        }
    );
    let out = tick(&mut s, 11_000, None, vec![]);
    assert!(saves(&out).is_empty());
}

// 18. RouteBundle history_pois の構造検証（E_SCHEMA）
#[test]
fn history_poi_structural_validation_returns_e_schema() {
    let cases = [
        ("empty poi_id", {
            let mut p = poi("", 300_000, 12_000, 5_000);
            p.poi_id.clear();
            p
        }),
        ("beyond length", poi("far", 1_000_001, 12_000, 5_000)),
        (
            "empty versions",
            HistoryPoi {
                poi_id: "empty".into(),
                along_route_cm: 300_000,
                versions: vec![],
            },
        ),
        (
            "duplicate kind",
            HistoryPoi {
                poi_id: "dup".into(),
                along_route_cm: 300_000,
                versions: vec![
                    HistoryVersion {
                        kind: HistoryVersionKind::Short,
                        content_ref: "a".into(),
                        duration_ms: 10_000,
                    },
                    HistoryVersion {
                        kind: HistoryVersionKind::Short,
                        content_ref: "b".into(),
                        duration_ms: 8_000,
                    },
                ],
            },
        ),
    ];
    for (label, bad_poi) in cases {
        let b = straight_route(10, vec![bad_poi]);
        let mut s = Session::new(SessionConfig::default());
        s.record_plan_ok_for_test("p1", b.route_key.clone());
        let err = s
            .route_commit(&RouteCommitRequest {
                req_id: "p1".into(),
                expected_route_key: None,
                bundle: b,
            })
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Schema, "{label}: {err:?}");
    }
    // Also verify invalid SessionConfig (corridor_far < corridor_near) is rejected as E_SCHEMA
    let err = Session::from_json(br#"{"p3_corridor_far_cm": 10000, "p3_corridor_near_cm": 50000}"#)
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Schema);
    assert_eq!(HorizonStatus::Tracking, HorizonStatus::Tracking);
}

// 19. 逸脱（OffRoute）で Speaking 中は Stop + save_for_later{interrupted}、Pending は Expired
#[test]
fn off_route_interrupts_speaking_and_expires_pending() {
    let b = straight_route(
        10,
        vec![
            poi("castle", 300_000, 12_000, 5_000),
            poi("shrine", 305_000, 12_000, 5_000),
        ],
    );
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out0 = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 105_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok = speaks_content(&out0)[0].0.to_owned();
    assert_eq!(s.history_poi_state("shrine"), Some(&PoiState::Eligible));

    // 3 consecutive off-route fixes (100 m north = 10_000 cm > lateral_tol_cm 3000)
    let _ = tick(
        &mut s,
        11_000,
        Some(fix_xy(11_000, 106_000, 10_000)),
        vec![],
    );
    let _ = tick(
        &mut s,
        12_000,
        Some(fix_xy(12_000, 107_000, 10_000)),
        vec![],
    );
    let out_off = tick(
        &mut s,
        13_000,
        Some(fix_xy(13_000, 108_000, 10_000)),
        vec![],
    );
    assert_eq!(stops(&out_off), vec![tok.as_str()]);
    assert_eq!(
        saves(&out_off),
        vec![("castle", SaveForLaterReason::Interrupted)]
    );
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Interrupted));
    assert_eq!(s.history_poi_state("shrine"), Some(&PoiState::Expired));
}

// 20. 同意 OFF で止めた（Expired）POI は BMK の対象にならない（NO_PENDING_TARGET）
#[test]
fn consent_off_expired_speaking_is_not_bmk_target() {
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let _ = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    let _ = tick(
        &mut s,
        11_000,
        Some(fix_at(11_000, 152_000)),
        vec![consent_ev(11_000, false)],
    );
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Expired));
    assert_eq!(
        bmk(&mut s, 12_000),
        ValidateOverrideResult::Rejected {
            code: RejectCode::NoPendingTarget
        }
    );
}

// ---- helpers for P3 stop vs CXL and audio_focus_gained tests ----------------------------

fn dict_ev(at: u64, norm: &str) -> Event {
    Event {
        at_mono_ms: at,
        body: EventBody::AsrResult {
            utterance_id: format!("u-{at}"),
            text_class: TextClass::Dict,
            dict_norm: Some(norm.to_owned()),
        },
    }
}

fn avoid_facts(
    id: &str,
    eta_ms: u64,
    right_unprotected: u64,
    graph_version: &str,
) -> CandidateFacts {
    serde_json::from_value(json!({
        "candidate_id": id,
        "label": "through_anchor",
        "plan_request_id": "p-avoid",
        "graph_version": graph_version,
        "fingerprint": format!("fp-{id}"),
        "legal": true,
        "complete": true,
        "eta_ms": eta_ms,
        "length_m": 4_000,
        "counts": {
            "right_unprotected": right_unprotected,
            "right_signal_unknown_protection": 0,
            "right_signal_unknown": 0,
            "right_protected": 0,
            "right_entry": 0,
            "merges": 0
        },
        "narrow_m": { "confirmed": 0, "inferred": 0 },
        "unknown_width_residential_m": 0,
        "unknown_length_m": 0,
        "junctions": [],
        "through_anchors": []
    }))
    .unwrap()
}

/// Build a 10 km route with a POI at 2 km (200_000 cm) and an avoid-target unprotected right
/// turn at 3 km (300_000 cm; ETA 100 s from 1 km -> triggers avoid_chip lookahead <= 120 s,
/// while complex window starts at 300_000 cm = 100 s - 30 s = 70 s ahead, leaving > 50 s for P3).
fn route_with_poi_and_avoid_target(pois: Vec<HistoryPoi>) -> RouteBundle {
    let mut b = straight_route(10, pois);
    let rk = b.route_key.clone();
    b.legs[0].maneuvers = vec![
        complex_right_maneuver(&rk, 0, 3, 300_000),
        plain_maneuver(&rk, 1, 10, 1_000_000, Arrow::Arrive),
    ];
    b
}

/// Open a session with avoid_chip_lookahead enabled, trigger an avoid offer at x=1 km (t=10_000),
/// and return `(session, route_key, avoid_req_id, offer_id)`.
fn session_with_active_avoid_offer(
    pois: Vec<HistoryPoi>,
    consent_on: bool,
    extra_events: Vec<Event>,
) -> (Session, RouteKey, String, String, TickOutput) {
    let b = route_with_poi_and_avoid_target(pois);
    let cfg = SessionConfig {
        avoid_chip_lookahead: true,
        ..SessionConfig::default()
    };
    let (mut s, rk) = session_with(cfg, &b);
    let mut evs = Vec::new();
    if consent_on {
        evs.push(consent_ev(10_000, true));
    }
    evs.extend(extra_events);
    // At x = 100_000 cm (1 km): 1 km before POI at 200_000 cm (inside [50_000, 200_000] corridor)
    // and 2 km (100 s) before avoid target at 300_000 cm (<= 120 s trigger).
    let out1 = tick(&mut s, 10_000, Some(fix_at(10_000, 100_000)), evs);
    let req_id = out1
        .planner_cmds
        .iter()
        .find_map(|c| match c {
            PlannerCmd::Start { req_id, .. } => Some(req_id.clone()),
            _ => None,
        })
        .expect("avoid_chip search should start");
    let out2 = tick(
        &mut s,
        10_500,
        Some(fix_at(10_500, 101_000)),
        vec![Event {
            at_mono_ms: 10_500,
            body: EventBody::SearchFinished {
                req_id: req_id.clone(),
                route_key: rk.clone(),
                graph_version: rk.graph_version.clone(),
                outcome: SearchOutcome::Ok,
                facts_ref: Some("alt-f".into()),
                base_facts_ref: Some("base-f".into()),
                engine_ms: 200,
            },
        }],
    );
    assert!(out2
        .planner_cmds
        .iter()
        .any(|c| matches!(c, PlannerCmd::EvaluateOffer { .. })));
    let eval = s.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id: req_id.clone(),
        now_mono_ms: 10_600,
        alt_facts: avoid_facts("alt", 460_000, 0, &rk.graph_version),
        base_facts: avoid_facts("base", 450_000, 1, &rk.graph_version),
    });
    let offer_id = match eval {
        AvoidOfferEvalResult::Offered { avoid_offer } => avoid_offer.offer_id,
        other => panic!("expected Offered, got {other:?}"),
    };
    (s, rk, req_id, offer_id, out1)
}

// 21. P3 話し中（Speaking）の「やめて」は P3 停止のみ（同時に回避チップ提示中・undo 窓内でも CXL しない）
#[test]
fn p3_speaking_yamete_stops_p3_only_without_triggering_cxl() {
    // Case A: P3 Speaking + active AvoidOffer on screen -> "やめて" stops P3 only; offer stays!
    let (mut s, _rk, _req_id, offer_id, out1) =
        session_with_active_avoid_offer(vec![poi("castle", 200_000, 12_000, 5_000)], true, vec![]);
    let tok = speaks_content(&out1)[0].0.to_owned();
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Speaking { .. })
    ));

    let out_stop = tick(
        &mut s,
        11_000,
        Some(fix_at(11_000, 102_000)),
        vec![dict_ev(11_000, "やめて")],
    );
    assert_eq!(stops(&out_stop), vec![tok.as_str()]);
    assert!(saves(&out_stop).is_empty());
    assert_eq!(s.history_poi_state("castle"), Some(&PoiState::Expired));
    // Must NOT dismiss the avoid offer
    assert!(
        !out_stop
            .ui_cmds
            .iter()
            .any(|u| matches!(u, UiCmd::Dismiss { target_id, .. } if target_id == &offer_id)),
        "yamete while P3 Speaking must not trigger CXL: {:?}",
        out_stop.ui_cmds
    );
    // Avoid offer is still on horizon!
    let still_offered = out_stop
        .horizon
        .as_ref()
        .is_some_and(|h| h.upcoming.iter().any(|u| u.avoid_offer.is_some()));
    assert!(still_offered, "avoid offer should remain active");

    // Case B: P3 Speaking + inside 5 s avoid undo window (Consented) -> "やめて" stops P3 only;
    // must NOT cancel_commit!
    let (mut s2, _rk2, req_id2, _offer_id2, out_speak) =
        session_with_active_avoid_offer(vec![poi("castle", 200_000, 12_000, 5_000)], true, vec![]);
    let tok2 = speaks_content(&out_speak)[0].0.to_owned();
    // Consent to avoid offer via dict "はい" -> enters UndoState::Consented (5 s window)
    let out_yes = tick(
        &mut s2,
        11_000,
        Some(fix_at(11_000, 102_000)),
        vec![dict_ev(11_000, "はい")],
    );
    assert!(out_yes
        .planner_cmds
        .iter()
        .any(|c| matches!(c, PlannerCmd::Commit { req_id, .. } if req_id == &req_id2)));
    assert!(matches!(
        s2.history_poi_state("castle"),
        Some(PoiState::Speaking { .. })
    ));

    let out_stop2 = tick(
        &mut s2,
        12_000,
        Some(fix_at(12_000, 104_000)),
        vec![dict_ev(12_000, "やめて")],
    );
    assert_eq!(stops(&out_stop2), vec![tok2.as_str()]);
    assert!(saves(&out_stop2).is_empty());
    assert_eq!(s2.history_poi_state("castle"), Some(&PoiState::Expired));
    assert!(
        !out_stop2
            .planner_cmds
            .iter()
            .any(|c| matches!(c, PlannerCmd::CancelCommit { .. })),
        "yamete while P3 Speaking must not trigger undo CXL"
    );
}

// 22. P3 話し中（Speaking）の「キャンセル」は常に CXL（P3 は止めない）
#[test]
fn p3_speaking_cancel_runs_cxl_without_stopping_p3() {
    // Case A: P3 Speaking + active AvoidOffer -> "キャンセル" dismisses offer, P3 keeps Speaking!
    let (mut s, _rk, _req_id, offer_id, out1) =
        session_with_active_avoid_offer(vec![poi("castle", 200_000, 12_000, 5_000)], true, vec![]);
    assert_eq!(speaks_content(&out1).len(), 1);
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Speaking { .. })
    ));

    let out_cxl = tick(
        &mut s,
        11_000,
        Some(fix_at(11_000, 102_000)),
        vec![dict_ev(11_000, "キャンセル")],
    );
    assert!(stops(&out_cxl).is_empty(), "キャンセル must not stop P3");
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Speaking { .. })
    ));
    assert!(out_cxl.ui_cmds.iter().any(|u| matches!(
        u,
        UiCmd::Dismiss { target_id, reason: DismissReason::Cancelled } if target_id == &offer_id
    )));

    // Case B: P3 Speaking + inside 5 s undo window -> "キャンセル" triggers cancel_commit, P3 stays Speaking!
    let (mut s2, _rk2, req_id2, _offer_id2, _out_speak) =
        session_with_active_avoid_offer(vec![poi("castle", 200_000, 12_000, 5_000)], true, vec![]);
    let _ = tick(
        &mut s2,
        11_000,
        Some(fix_at(11_000, 102_000)),
        vec![dict_ev(11_000, "はい")],
    );
    let out_undo = tick(
        &mut s2,
        12_000,
        Some(fix_at(12_000, 104_000)),
        vec![dict_ev(12_000, "キャンセル")],
    );
    assert!(stops(&out_undo).is_empty(), "キャンセル must not stop P3");
    assert!(matches!(
        s2.history_poi_state("castle"),
        Some(PoiState::Speaking { .. })
    ));
    assert!(out_undo
        .planner_cmds
        .iter()
        .any(|c| matches!(c, PlannerCmd::CancelCommit { req_id } if req_id == &req_id2)));
}

// 23. P3 なし（Pending / Idle）の「やめて」は CXL（回避チップ取り下げ・undo が動き、Pending は消さない）
#[test]
fn p3_not_speaking_yamete_runs_cxl_and_preserves_pending_poi() {
    // Case A: P3 is Pending (blocked by audio_focus_lost) + active AvoidOffer
    let (mut s, _rk, _req_id, offer_id, out1) = session_with_active_avoid_offer(
        vec![poi("castle", 200_000, 12_000, 5_000)],
        true,
        vec![Event {
            at_mono_ms: 10_000,
            body: EventBody::AudioFocusLost {},
        }],
    );
    assert!(speaks_content(&out1).is_empty());
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Pending { .. })
    ));

    let out_cxl = tick(
        &mut s,
        11_000,
        Some(fix_at(11_000, 102_000)),
        vec![dict_ev(11_000, "やめて")],
    );
    assert!(stops(&out_cxl).is_empty());
    // Pending POI must NOT be expired by "やめて" when not Speaking!
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Pending { .. })
    ));
    // And CXL dismissed the avoid offer
    assert!(out_cxl.ui_cmds.iter().any(|u| matches!(
        u,
        UiCmd::Dismiss { target_id, reason: DismissReason::Cancelled } if target_id == &offer_id
    )));

    // Case B: P3 Idle (consent OFF) + inside 5 s undo window -> "やめて" triggers undo cancel_commit!
    let (mut s2, _rk2, req_id2, _offer_id2, _) =
        session_with_active_avoid_offer(vec![], false, vec![]);
    let _ = tick(
        &mut s2,
        11_000,
        Some(fix_at(11_000, 102_000)),
        vec![dict_ev(11_000, "はい")],
    );
    let out_undo = tick(
        &mut s2,
        12_000,
        Some(fix_at(12_000, 104_000)),
        vec![dict_ev(12_000, "やめて")],
    );
    assert!(out_undo
        .planner_cmds
        .iter()
        .any(|c| matches!(c, PlannerCmd::CancelCommit { req_id } if req_id == &req_id2)));
}

// 24. P3 話し中の「ストップ」「止めて」「とめて」で P3 停止（非話し中では CXL を起こさない）
#[test]
fn p3_speaking_stop_words_stop_p3_and_do_not_trigger_cxl_when_idle() {
    for phrase in ["ストップ", "止めて", "とめて"] {
        let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
        let (mut s, _) = session_with(SessionConfig::default(), &b);
        let out0 = tick(
            &mut s,
            10_000,
            Some(fix_at(10_000, 150_000)),
            vec![consent_ev(10_000, true)],
        );
        let tok = speaks_content(&out0)[0].0.to_owned();

        let out_stop = tick(
            &mut s,
            11_000,
            Some(fix_at(11_000, 152_000)),
            vec![dict_ev(11_000, phrase)],
        );
        assert_eq!(stops(&out_stop), vec![tok.as_str()], "phrase={phrase}");
        assert!(saves(&out_stop).is_empty(), "phrase={phrase}");
        assert_eq!(
            s.history_poi_state("castle"),
            Some(&PoiState::Expired),
            "phrase={phrase}"
        );

        // Outside P3 Speaking, "ストップ" / "止めて" / "とめて" must NOT trigger CXL
        let (mut s_idle, _rk, _req_id, offer_id, _) =
            session_with_active_avoid_offer(vec![], false, vec![]);
        let out_idle = tick(
            &mut s_idle,
            11_000,
            Some(fix_at(11_000, 102_000)),
            vec![dict_ev(11_000, phrase)],
        );
        assert!(
            !out_idle
                .ui_cmds
                .iter()
                .any(|u| matches!(u, UiCmd::Dismiss { target_id, .. } if target_id == &offer_id)),
            "phrase={phrase} must not trigger CXL when P3 is not Speaking"
        );
    }
}

// 25. audio_focus_lost 中は話さず、audio_focus_gained で Pending -> Speaking に進む（中断 POI は再開しない）
#[test]
fn audio_focus_gained_restores_p3_without_resuming_interrupted_poi() {
    // Case A: Pending blocked by audio_focus_lost -> audio_focus_gained -> Speaking
    let b = straight_route(10, vec![poi("castle", 300_000, 12_000, 5_000)]);
    let (mut s, _) = session_with(SessionConfig::default(), &b);
    let out_lost = tick(
        &mut s,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![
            consent_ev(10_000, true),
            Event {
                at_mono_ms: 10_000,
                body: EventBody::AudioFocusLost {},
            },
        ],
    );
    assert!(speaks_content(&out_lost).is_empty());
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Pending { .. })
    ));

    // Focus regained -> Pending transitions to Speaking immediately
    let out_gained = tick(
        &mut s,
        15_000,
        Some(fix_at(15_000, 160_000)),
        vec![Event {
            at_mono_ms: 15_000,
            body: EventBody::AudioFocusGained {},
        }],
    );
    let spoken = speaks_content(&out_gained);
    assert_eq!(spoken.len(), 1);
    assert_eq!(spoken[0].1, "pack:castle:short");
    assert!(matches!(
        s.history_poi_state("castle"),
        Some(PoiState::Speaking { .. })
    ));

    // Case B: Speaking interrupted by audio_focus_lost -> audio_focus_gained does NOT resume
    // the interrupted POI, but a later POI can speak after the 5-minute interval.
    let b2 = straight_route(
        20,
        vec![
            poi("castle", 300_000, 12_000, 5_000),
            poi("shrine", 1_100_000, 12_000, 5_000),
        ],
    );
    let (mut s2, _) = session_with(SessionConfig::default(), &b2);
    let out_s = tick(
        &mut s2,
        10_000,
        Some(fix_at(10_000, 150_000)),
        vec![consent_ev(10_000, true)],
    );
    let tok_castle = speaks_content(&out_s)[0].0.to_owned();

    // Audio focus lost while speaking -> Stop + save_for_later{interrupted}
    let out_int = tick(
        &mut s2,
        12_000,
        Some(fix_at(12_000, 154_000)),
        vec![Event {
            at_mono_ms: 12_000,
            body: EventBody::AudioFocusLost {},
        }],
    );
    assert_eq!(stops(&out_int), vec![tok_castle.as_str()]);
    assert_eq!(
        saves(&out_int),
        vec![("castle", SaveForLaterReason::Interrupted)]
    );
    assert_eq!(s2.history_poi_state("castle"), Some(&PoiState::Interrupted));

    // Regain audio focus right after -> castle MUST NOT resume!
    let out_regain = tick(
        &mut s2,
        15_000,
        Some(fix_at(15_000, 160_000)),
        vec![Event {
            at_mono_ms: 15_000,
            body: EventBody::AudioFocusGained {},
        }],
    );
    assert!(speaks_content(&out_regain).is_empty());
    assert_eq!(s2.history_poi_state("castle"), Some(&PoiState::Interrupted));

    // After 5 min (t = 310_000) near shrine (950_000 cm -> 1.5 km before 1_100_000 cm) -> shrine speaks!
    let out_next = tick(&mut s2, 310_000, Some(fix_at(310_000, 950_000)), vec![]);
    let spoken2 = speaks_content(&out_next);
    assert_eq!(spoken2.len(), 1);
    assert_eq!(spoken2[0].1, "pack:shrine:short");
}

// 26. 辞書正本（schema/slm/expected_codes.json）と Rust の DICT_STOP / DICT_CXL の完全一致検証
#[test]
fn p3_stop_and_cxl_dictionaries_match_expected_codes_json() {
    let expected: Value =
        serde_json::from_str(include_str!("../../../../schema/slm/expected_codes.json")).unwrap();
    let prefilter = &expected["deterministic_prefilter"];

    // 1. exact_matches.CXL == avoid::DICT_CXL
    let schema_cxl: Vec<&str> = prefilter["exact_matches"]["CXL"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        avoid::DICT_CXL,
        schema_cxl.as_slice(),
        "avoid::DICT_CXL must match schema/slm/expected_codes.json exact_matches.CXL"
    );

    // 2. context_scoped_matches.P3_STOP.phrases == history::DICT_STOP
    let p3_stop = &prefilter["context_scoped_matches"]["P3_STOP"];
    let schema_p3_stop: Vec<&str> = p3_stop["phrases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        history::DICT_STOP,
        schema_p3_stop.as_slice(),
        "history::DICT_STOP must match schema/slm/expected_codes.json P3_STOP.phrases"
    );

    // 3. always_cxl_phrases == DICT_CXL \ DICT_STOP
    let schema_always_cxl: Vec<&str> = p3_stop["dispatch_rules"]["always_cxl_phrases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let rust_always_cxl: Vec<&str> = avoid::DICT_CXL
        .iter()
        .copied()
        .filter(|w| !history::DICT_STOP.contains(w))
        .collect();
    assert_eq!(
        rust_always_cxl, schema_always_cxl,
        "DICT_CXL minus DICT_STOP must match always_cxl_phrases"
    );
}
