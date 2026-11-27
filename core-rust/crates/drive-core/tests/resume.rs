//! `dc_session_new` `resume: { route_bundle, route_key }` tests (PR #23, docs 04 §3.2):
//! - Reloads the route immediately without incrementing `route_revision` or requiring `req_id`
//! - Starts alignment in `uncertain`, then returns to `tracking` on an on-route `Fix`
//! - Preserves `route_key` (including `route_revision`) and `ManeuverId.route_key` in `ManeuverHorizon`
//! - Subsequent normal `dc_route_commit` increments `route_revision` to `resumed + 1`
//! - Does not carry over P3 spoken history / `avoid_tried` / held results / plan state
//! - Rejects invalid resume payloads (`route_revision: "0"`, key mismatch, unknown fields)
use contracts::horizon::HorizonStatus;
use contracts::route::{RouteBundle, RouteCommitRequest};
use contracts::tick::{Fix, PlannerCmd, SearchPriority, TickInput, TickOutput};
use contracts::{ErrorCode, RouteKey, U64Str};
use drive_core::Session;
use serde_json::{json, Value};

fn main_line() -> RouteBundle {
    let p = format!(
        "{}/../../tests/fixtures/route/gate7-side-road-20m.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    serde_json::from_value(v["bundle"].clone()).unwrap()
}

fn fix_xy(b: &RouteBundle, at: u64, x_cm: i64, y_cm: i64) -> Fix {
    let c = &b.legs[0].chunks[0];
    Fix {
        at_mono_ms: at,
        lat_e7: c.origin_e7.lat + (y_cm * 65_536 / i64::from(c.k_lat_q16)) as i32,
        lon_e7: c.origin_e7.lon + (x_cm * 65_536 / i64::from(c.k_lon_q16)) as i32,
        acc_cm: 500,
        bearing_cdeg: Some(9_000),
        speed_cm_s: Some(1_500),
    }
}

fn tick(s: &mut Session, now: u64, fix: Option<Fix>, events: Vec<Value>) -> TickOutput {
    let input: TickInput = serde_json::from_value(json!({
        "now_mono_ms": now,
        "fix": fix,
        "events": events,
    }))
    .unwrap();
    s.tick(input)
}

#[test]
fn resume_preserves_route_key_starts_uncertain_then_tracks_and_next_commit_increments() {
    let orig = main_line();
    let resumed_rk = RouteKey {
        route_revision: U64Str(5),
        ..orig.route_key.clone()
    };

    // Case 1: route_bundle carries route_revision "0" (raw C++ bundle) while resume.route_key has "5"
    let cfg = json!({
        "fix_stale_ms": 1_000_000,
        "resume": {
            "route_bundle": orig,
            "route_key": resumed_rk,
        }
    });
    let mut s = Session::from_json(&serde_json::to_vec(&cfg).unwrap()).unwrap();

    // Immediately after resume: route is loaded with the exact route_key (rev 5, NOT 1 or 6),
    // and clean initial state (no plan, no held result, no spoken P3, no issued commits)
    assert_eq!(s.route().unwrap().route_key, resumed_rk);
    assert_eq!(s.horizon_status(1_000), Some(HorizonStatus::Uncertain));
    assert!(s.planner().plan().is_none());
    assert!(s.planner().held().is_none());
    assert_eq!(s.issued_commits().count(), 0);
    assert!(!s.history_consent());
    assert_eq!(s.history_spoken_count(), 0);

    // First tick without fix: horizon status is `uncertain`, route_key and ManeuverId.route_key stay rev 5
    let h1 = tick(&mut s, 1_000, None, vec![]).horizon.unwrap();
    assert_eq!(h1.status, HorizonStatus::Uncertain);
    assert_eq!(h1.route_key, resumed_rk);
    assert!(!h1.upcoming.is_empty());
    for entry in &h1.upcoming {
        assert_eq!(entry.maneuver.id.route_key, resumed_rk);
    }

    // On-route fix -> returns to `tracking` with the exact same ManeuverId.route_key (rev 5)
    let h2 = tick(&mut s, 2_000, Some(fix_xy(&orig, 2_000, 10_000, 0)), vec![])
        .horizon
        .unwrap();
    assert_eq!(h2.status, HorizonStatus::Tracking);
    assert_eq!(h2.route_key, resumed_rk);
    for entry in &h2.upcoming {
        assert_eq!(entry.maneuver.id.route_key, resumed_rk);
    }

    // Off-route -> reroute -> ok -> normal dc_route_commit increments revision to 5 + 1 = 6
    let mut reroute_req_id = None;
    for (i, t) in [3_000u64, 4_000, 5_000].into_iter().enumerate() {
        let o = tick(
            &mut s,
            t,
            Some(fix_xy(&orig, t, 11_000 + 1_000 * i as i64, 10_000)),
            vec![],
        );
        for cmd in o.planner_cmds {
            if let PlannerCmd::Start {
                req_id,
                priority: SearchPriority::Reroute,
                ..
            } = cmd
            {
                reroute_req_id = Some(req_id);
            }
        }
    }
    let req_id = reroute_req_id.expect("reroute started");
    let o_fin = tick(
        &mut s,
        6_000,
        None,
        vec![json!({
            "at_mono_ms": 6_000,
            "kind": "search_finished",
            "req_id": req_id,
            "route_key": resumed_rk,
            "graph_version": resumed_rk.graph_version,
            "outcome": "ok",
            "engine_ms": 100
        })],
    );
    assert_eq!(
        o_fin.planner_cmds,
        vec![PlannerCmd::Commit {
            req_id: req_id.clone(),
            route_key: resumed_rk.clone(),
            undo_req_id: None,
        }]
    );
    let committed = s
        .route_commit(&RouteCommitRequest {
            req_id,
            expected_route_key: Some(resumed_rk),
            bundle: orig,
        })
        .unwrap();
    assert_eq!(committed.route_key.route_revision, U64Str(6));
}

#[test]
fn resume_accepts_already_stamped_bundle_and_rejects_invalid_payloads() {
    let orig = main_line();
    let resumed_rk = RouteKey {
        route_revision: U64Str(7),
        ..orig.route_key.clone()
    };

    // Case 2: route_bundle already stamped with route_revision "7" matching resume.route_key
    let mut stamped = serde_json::to_value(&orig).unwrap();
    stamped["route_key"]["route_revision"] = json!("7");
    for m in stamped["legs"][0]["maneuvers"].as_array_mut().unwrap() {
        m["id"]["route_key"]["route_revision"] = json!("7");
    }
    let cfg = json!({
        "resume": {
            "route_bundle": stamped,
            "route_key": resumed_rk,
        }
    });
    let mut s = Session::from_json(&serde_json::to_vec(&cfg).unwrap()).unwrap();
    assert_eq!(s.route().unwrap().route_key.route_revision, U64Str(7));
    let h = tick(&mut s, 100, None, vec![]).horizon.unwrap();
    assert_eq!(h.route_key.route_revision, U64Str(7));
    assert_eq!(
        h.upcoming[0].maneuver.id.route_key.route_revision,
        U64Str(7)
    );

    // Invalid: resume.route_key.route_revision == "0" -> E_SCHEMA
    let zero_rev = json!({
        "resume": {
            "route_bundle": orig,
            "route_key": orig.route_key,
        }
    });
    assert_eq!(
        Session::from_json(&serde_json::to_vec(&zero_rev).unwrap())
            .unwrap_err()
            .code,
        ErrorCode::Schema
    );

    // Invalid: bundle route_revision "3" != resume.route_key "7" -> E_SCHEMA
    let mut wrong_rev = serde_json::to_value(&orig).unwrap();
    wrong_rev["route_key"]["route_revision"] = json!("3");
    for m in wrong_rev["legs"][0]["maneuvers"].as_array_mut().unwrap() {
        m["id"]["route_key"]["route_revision"] = json!("3");
    }
    let bad_rev_cfg = json!({
        "resume": {
            "route_bundle": wrong_rev,
            "route_key": resumed_rk,
        }
    });
    assert_eq!(
        Session::from_json(&serde_json::to_vec(&bad_rev_cfg).unwrap())
            .unwrap_err()
            .code,
        ErrorCode::Schema
    );

    // Invalid: mismatched trip_id -> E_SCHEMA
    let mut wrong_trip = resumed_rk.clone();
    wrong_trip.trip_id = "other-trip".into();
    let bad_trip_cfg = json!({
        "resume": {
            "route_bundle": orig,
            "route_key": wrong_trip,
        }
    });
    assert_eq!(
        Session::from_json(&serde_json::to_vec(&bad_trip_cfg).unwrap())
            .unwrap_err()
            .code,
        ErrorCode::Schema
    );

    // Invalid: unknown field inside resume -> E_SCHEMA
    let extra_cfg = json!({
        "resume": {
            "route_bundle": orig,
            "route_key": resumed_rk,
            "extra": 1
        }
    });
    assert_eq!(
        Session::from_json(&serde_json::to_vec(&extra_cfg).unwrap())
            .unwrap_err()
            .code,
        ErrorCode::Schema
    );
}
