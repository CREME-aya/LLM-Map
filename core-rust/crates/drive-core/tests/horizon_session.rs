//! ManeuverHorizon from dc_tick (04 §2 / §3): frame_seq only on change, 3 entries, status
//! transitions, the commit condition and engine_stuck without UiCmd (03 §3.1).
use contracts::horizon::{Arrow, HorizonStatus};
use contracts::route::{RouteBundle, RouteCommitRequest};
use contracts::tick::{Fix, PlannerCmd, SearchPriority, TickInput, TickOutput};
use contracts::RouteKey;
use drive_core::{Session, SessionConfig};
use serde_json::Value;

/// The main-line route of the Gate 7 fixtures: 4 km east, left at 1.5 km, right at 2.5 km,
/// left at 3.5 km, arrive at 4 km.
fn main_line() -> RouteBundle {
    let p = format!(
        "{}/../../tests/fixtures/route/gate7-side-road-20m.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    serde_json::from_value(v["bundle"].clone()).unwrap()
}

/// A fix `x_cm` east of the start and `y_cm` north (1e-7 deg ≈ 1.1132 cm north,
/// ≈ 0.912 cm east at 35°; good enough for tests that only need "on" / "far off").
fn fix_xy(b: &RouteBundle, at: u64, x_cm: i64, y_cm: i64) -> Fix {
    let c = &b.legs[0].chunks[0];
    let dlon = x_cm * 65_536 / i64::from(c.k_lon_q16);
    let dlat = y_cm * 65_536 / i64::from(c.k_lat_q16);
    Fix {
        at_mono_ms: at,
        lat_e7: c.origin_e7.lat + dlat as i32,
        lon_e7: c.origin_e7.lon + dlon as i32,
        acc_cm: 500,
        bearing_cdeg: Some(9_000),
        speed_cm_s: Some(1_500),
    }
}

fn tick(s: &mut Session, now: u64, fix: Option<Fix>) -> TickOutput {
    s.tick(TickInput {
        now_mono_ms: now,
        fix,
        events: vec![],
    })
}

fn committed() -> (Session, RouteBundle) {
    let mut s = Session::new(SessionConfig::default());
    let b = main_line();
    s.route_commit(&RouteCommitRequest {
        route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    (s, b)
}

fn finished(req: &str, rk: &RouteKey, at: u64) -> TickInput {
    serde_json::from_value(serde_json::json!({"now_mono_ms": at, "events": [
        {"at_mono_ms": at, "kind": "search_finished", "req_id": req, "route_key": rk,
         "graph_version": rk.graph_version, "outcome": "ok", "engine_ms": 1}]}))
    .unwrap()
}

#[test]
fn frames_only_on_change_with_three_upcoming() {
    let (mut s, b) = committed();
    // committed, no fix: uncertain, the first 3 maneuvers without from_vehicle_m
    let h = tick(&mut s, 0, None).horizon.unwrap();
    assert_eq!((h.frame_seq, h.status), (1, HorizonStatus::Uncertain));
    assert_eq!(h.upcoming.len(), 3);
    assert_eq!(h.upcoming[0].from_vehicle_m, None);
    assert_eq!(h.upcoming[1].seg_from_prev_m, Some(1_000));
    assert_eq!(h.route_key, b.route_key);
    // nothing changed → no frame
    assert!(tick(&mut s, 500, None).horizon.is_none());
    // first fix at 100 m: tracking, distances from the vehicle
    let h = tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)))
        .horizon
        .unwrap();
    assert_eq!((h.frame_seq, h.status), (2, HorizonStatus::Tracking));
    let arrows: Vec<Arrow> = h.upcoming.iter().map(|e| e.maneuver.arrow).collect();
    assert_eq!(arrows, vec![Arrow::Left, Arrow::Right, Arrow::Left]);
    assert_eq!(h.upcoming[0].from_vehicle_m, Some(1_400));
    assert_eq!(h.upcoming[2].seg_from_prev_m, Some(1_000));
    assert!(h.matched_progress_cm.is_some());
    assert_eq!(h.valid_until_monotonic_ms, 1_000 + 2_500);
    // same position again: same content → no frame (frame_seq unchanged)
    assert!(tick(&mut s, 2_000, Some(fix_xy(&b, 2_000, 10_000, 0)))
        .horizon
        .is_none());
    assert_eq!(s.frame_seq(), 2);
    // past the first maneuver (1.5 km): right, left, arrive
    let h = tick(&mut s, 3_000, Some(fix_xy(&b, 3_000, 160_000, 0)))
        .horizon
        .unwrap();
    assert_eq!(h.frame_seq, 3);
    let arrows: Vec<Arrow> = h.upcoming.iter().map(|e| e.maneuver.arrow).collect();
    assert_eq!(arrows, vec![Arrow::Right, Arrow::Left, Arrow::Arrive]);
    // a short backward jitter never revives the passed maneuver
    let h = tick(&mut s, 4_000, Some(fix_xy(&b, 4_000, 159_000, 0)));
    assert!(h.horizon.is_none(), "progress kept, content unchanged");
    // destination: finished, upcoming empty, no off-route afterwards
    tick(&mut s, 5_000, Some(fix_xy(&b, 5_000, 380_000, 0)));
    let h = tick(&mut s, 6_000, Some(fix_xy(&b, 6_000, 400_500, 0)))
        .horizon
        .unwrap();
    assert_eq!(h.status, HorizonStatus::Finished);
    assert!(h.upcoming.is_empty());
    for t in 7..12 {
        let o = tick(
            &mut s,
            t * 1_000,
            Some(fix_xy(&b, t * 1_000, 400_000, 100_000)),
        );
        assert!(o.planner_cmds.is_empty());
    }
    assert_eq!(s.horizon_status(12_000), Some(HorizonStatus::Finished));
}

/// Off-route → reroute start with the fix (rerouting) → ok → commit{req_id, start key}.
#[test]
fn reroute_from_fix_then_commit() {
    let (mut s, b) = committed();
    tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)));
    let mut outs = Vec::new();
    for (i, t) in [2_000u64, 3_000, 4_000].into_iter().enumerate() {
        outs.push(tick(
            &mut s,
            t,
            Some(fix_xy(&b, t, 11_000 + 1_000 * i as i64, 10_000)),
        ));
    }
    assert!(outs[0].planner_cmds.is_empty() && outs[1].planner_cmds.is_empty());
    match &outs[2].planner_cmds[..] {
        [PlannerCmd::Start {
            req_id,
            priority: SearchPriority::Reroute,
            fix: Some(f),
            route_key,
            ..
        }] => {
            assert_eq!(req_id, "r1");
            assert_eq!(f.at_mono_ms, 4_000);
            assert_eq!(*route_key, b.route_key);
        }
        other => panic!("{other:?}"),
    }
    assert!(outs[2].ui_cmds.is_empty());
    assert_eq!(
        outs[2].horizon.as_ref().unwrap().status,
        HorizonStatus::Rerouting
    );
    // more off fixes while it runs: no second reroute
    let o = tick(&mut s, 5_000, Some(fix_xy(&b, 5_000, 14_000, 10_000)));
    assert!(o.planner_cmds.is_empty());
    // ok → commit with the key the search started with
    let o = s.tick(finished("r1", &b.route_key, 6_000));
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::Commit {
            req_id: "r1".into(),
            route_key: b.route_key.clone()
        }]
    );
    // slot free, still off: status off_route until the new route is committed
    assert_eq!(s.horizon_status(6_000), Some(HorizonStatus::OffRoute));
    // Kotlin commits the new route: horizon of the new route, uncertain until a fix
    let mut v = serde_json::to_value(&b).unwrap();
    v["route_key"]["route_revision"] = "8".into();
    for m in v["legs"][0]["maneuvers"].as_array_mut().unwrap() {
        m["id"]["route_key"]["route_revision"] = "8".into();
    }
    let b2: RouteBundle = serde_json::from_value(v).unwrap();
    s.route_commit(&RouteCommitRequest {
        route_key: Some(b.route_key.clone()),
        bundle: b2.clone(),
    })
    .unwrap();
    let h = tick(&mut s, 6_500, None).horizon.unwrap();
    assert_eq!(h.route_key, b2.route_key);
    assert_eq!(h.status, HorizonStatus::Uncertain);
    let h = tick(&mut s, 7_000, Some(fix_xy(&b2, 7_000, 20_000, 0)))
        .horizon
        .unwrap();
    assert_eq!(h.status, HorizonStatus::Tracking);
}

/// "status=tracking" for adoption: an ok while the position is uncertain is discarded.
#[test]
fn ok_while_uncertain_is_not_committed() {
    let (mut s, b) = committed();
    tick(&mut s, 0, Some(fix_xy(&b, 0, 10_000, 0)));
    s.request_search(100, SearchPriority::ExplicitPlan).unwrap();
    // no fix for 3 s → uncertain
    assert_eq!(s.horizon_status(3_000), Some(HorizonStatus::Uncertain));
    let o = s.tick(finished("r1", &b.route_key, 3_000));
    assert!(o.planner_cmds.is_empty());
    assert!(o.log.iter().any(|l| l.what == "discarded_not_tracking"));
    // while tracking the same ok is committed
    let (mut s, b) = committed();
    tick(&mut s, 0, Some(fix_xy(&b, 0, 10_000, 0)));
    s.request_search(100, SearchPriority::ExplicitPlan).unwrap();
    tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 11_000, 0)));
    let o = s.tick(finished("r1", &b.route_key, 1_500));
    assert!(matches!(&o.planner_cmds[..], [PlannerCmd::Commit { .. }]));
}

/// engine_stuck: no UiCmd. A stuck reroute stays `rerouting`; a stuck avoid chip leaves the
/// status unchanged (tracking) and is only logged.
#[test]
fn engine_stuck_keeps_the_status() {
    // reroute
    let (mut s, b) = committed();
    tick(&mut s, 0, Some(fix_xy(&b, 0, 10_000, 0)));
    for t in [1_000u64, 2_000, 3_000] {
        tick(&mut s, t, Some(fix_xy(&b, t, 10_000, 10_000)));
    }
    assert_eq!(s.horizon_status(3_000), Some(HorizonStatus::Rerouting));
    // long class (ETA invalidated): watchdog 3000 + 10000
    let o = tick(&mut s, 13_000, None);
    assert!(o
        .log
        .iter()
        .any(|l| l.what == "engine_stuck" && l.detail.as_deref() == Some("priority=reroute")));
    assert!(o.ui_cmds.is_empty());
    assert!(o.horizon.is_none(), "status unchanged → no new frame");
    assert_eq!(s.horizon_status(13_000), Some(HorizonStatus::Rerouting));
    // avoid chip while tracking
    let (mut s, b) = committed();
    tick(&mut s, 0, Some(fix_xy(&b, 0, 10_000, 0)));
    s.request_search(0, SearchPriority::AvoidChip).unwrap();
    let mut stuck = None;
    for t in 1..=12u64 {
        let o = tick(
            &mut s,
            t * 1_000,
            Some(fix_xy(&b, t * 1_000, 10_000 + 1_500 * t as i64, 0)),
        );
        assert!(o.ui_cmds.is_empty());
        if o.log.iter().any(|l| l.what == "engine_stuck") {
            stuck = Some(t);
            assert!(o
                .log
                .iter()
                .any(|l| l.detail.as_deref() == Some("priority=avoid_chip")));
        }
        assert_eq!(s.horizon_status(t * 1_000), Some(HorizonStatus::Tracking));
    }
    assert!(stuck.is_some());
}

/// Out-of-order fixes are ignored; the freshness limit is in next_wake_mono_ms.
#[test]
fn out_of_order_fix_and_stale_wake() {
    let (mut s, b) = committed();
    let o = tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)));
    assert_eq!(o.next_wake_mono_ms, Some(1_000 + 2_501));
    let o = tick(&mut s, 1_200, Some(fix_xy(&b, 900, 50_000, 0)));
    assert!(o.log.iter().any(|l| l.what == "fix_out_of_order"));
    assert!(o.horizon.is_none());
    let o = tick(&mut s, 3_501, None);
    assert_eq!(o.horizon.unwrap().status, HorizonStatus::Uncertain);
    assert_eq!(o.next_wake_mono_ms, None);
}

#[test]
fn config_thresholds_and_defaults() {
    let c = SessionConfig::default();
    let a = c.align();
    assert_eq!(
        (
            a.lateral_tol_cm,
            a.heading_tol_deg,
            a.heading_min_speed_cm_s,
            a.confirm_fixes,
            a.max_acc_cm,
            a.fix_stale_ms
        ),
        (3_000, 60, 140, 3, 3_000, 2_500)
    );
    // max_acc_cm defaults to lateral_tol_cm
    let s = Session::from_json(br#"{"lateral_tol_cm":2000}"#).unwrap();
    assert_eq!(s.aligner().config().max_acc_cm, 2_000);
    let s = Session::from_json(br#"{"lateral_tol_cm":2000,"max_acc_cm":5000}"#).unwrap();
    assert_eq!(s.aligner().config().max_acc_cm, 5_000);
    assert!(Session::from_json(br#"{"confirm_fixes":0}"#).is_err());
    assert!(Session::from_json(br#"{"heading_tol_deg":181}"#).is_err());
    assert!(Session::from_json(br#"{"lateral_tol_cm":1.5}"#).is_err());
}
