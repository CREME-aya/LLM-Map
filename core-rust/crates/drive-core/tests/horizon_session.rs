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

/// `b` with the route key Rust assigned on commit (route and every ManeuverId), i.e. the
/// bundle as the session now holds it.
fn as_committed(s: &Session, mut b: RouteBundle) -> RouteBundle {
    let key = s.route().unwrap().route_key.clone();
    for leg in &mut b.legs {
        for m in &mut leg.maneuvers {
            m.id.route_key = key.clone();
        }
    }
    b.route_key = key;
    b
}

fn committed() -> (Session, RouteBundle) {
    let mut s = Session::new(SessionConfig::default());
    let b = main_line();
    s.route_commit(&RouteCommitRequest {
        req_id: None,
        expected_route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    let b = as_committed(&s, b);
    assert_eq!(b.route_key.route_revision.0, 1, "the first route is rev 1");
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
    // The same position on a later sample shifts the absolute P1 deadline.
    let next = tick(&mut s, 2_000, Some(fix_xy(&b, 2_000, 10_000, 0)))
        .horizon
        .unwrap();
    assert_eq!(
        next.next_p1_deadline_monotonic_ms.unwrap(),
        h.next_p1_deadline_monotonic_ms.unwrap() + 1_000
    );
    assert_eq!(next.frame_seq, 3);
    // past the first maneuver (1.5 km): right, left, arrive
    let h = tick(&mut s, 3_000, Some(fix_xy(&b, 3_000, 160_000, 0)))
        .horizon
        .unwrap();
    assert_eq!(h.frame_seq, 4);
    assert_eq!(h.upcoming[0].from_vehicle_m, Some(0));
    assert!(tick(&mut s, 3_050, None).horizon.is_none());
    let h = tick(&mut s, 3_100, Some(fix_xy(&b, 3_100, 160_000, 0)))
        .horizon
        .unwrap();
    let arrows: Vec<Arrow> = h.upcoming.iter().map(|e| e.maneuver.arrow).collect();
    assert_eq!(arrows, vec![Arrow::Right, Arrow::Left, Arrow::Arrive]);
    // a short backward jitter never revives the passed maneuver
    let h = tick(&mut s, 4_000, Some(fix_xy(&b, 4_000, 159_000, 0)));
    assert_eq!(h.horizon.unwrap().upcoming[0].maneuver.arrow, Arrow::Right);
    // destination: finished, upcoming empty, no off-route afterwards
    tick(&mut s, 5_000, Some(fix_xy(&b, 5_000, 380_000, 0)));
    let h = tick(&mut s, 6_000, Some(fix_xy(&b, 6_000, 400_500, 0)))
        .horizon
        .unwrap();
    assert_eq!(h.status, HorizonStatus::Tracking);
    let h = tick(&mut s, 6_100, Some(fix_xy(&b, 6_100, 400_500, 0)))
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
    assert_eq!(s.horizon_status(6_000), Some(HorizonStatus::Rerouting));
    let o = s.tick(finished("r1", &b.route_key, 6_000));
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::Commit {
            req_id: "r1".into(),
            route_key: b.route_key.clone(),
            undo_req_id: None,
        }]
    );
    // slot free, still off: status off_route until the new route is committed
    assert_eq!(s.horizon_status(6_000), Some(HorizonStatus::OffRoute));
    // An explicit plan can finish while off route even after the reroute slot was released.
    s.request_search(6_100, SearchPriority::ExplicitPlan)
        .unwrap();
    assert_eq!(s.horizon_status(6_200), Some(HorizonStatus::OffRoute));
    let o = s.tick(finished("r2", &b.route_key, 6_200));
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::Commit {
            req_id: "r2".into(),
            undo_req_id: None,
            route_key: b.route_key.clone()
        }]
    );
    // req_id must name a commit Rust issued with that guard: none / unknown / another
    // guard → E_STALE, nothing replaced
    let b2 = main_line();
    for (req_id, guard) in [
        (None, b.route_key.clone()),
        (Some("r3"), b.route_key.clone()),
        (Some("undo-r2"), b.route_key.clone()),
    ] {
        let e = s
            .route_commit(&RouteCommitRequest {
                req_id: req_id.map(str::to_owned),
                expected_route_key: Some(guard),
                bundle: b2.clone(),
            })
            .unwrap_err();
        assert_eq!(e.code, contracts::ErrorCode::Stale, "{req_id:?}");
        assert_eq!(s.route().unwrap().route_key, b.route_key);
    }
    // Kotlin commits the new route (r2): horizon of the new route, uncertain until a fix
    let r = s
        .route_commit(&RouteCommitRequest {
            req_id: Some("r2".into()),
            expected_route_key: Some(b.route_key.clone()),
            bundle: b2.clone(),
        })
        .unwrap();
    // r1 had the same guard; it is settled by the route change (E_STALE on both halves)
    let e = s
        .route_commit(&RouteCommitRequest {
            req_id: Some("r1".into()),
            expected_route_key: Some(r.route_key.clone()),
            bundle: main_line(),
        })
        .unwrap_err();
    assert_eq!(e.code, contracts::ErrorCode::Stale);
    assert_eq!(s.issued_commits().count(), 0);
    let b2 = as_committed(&s, b2);
    assert_eq!(r.route_key, b2.route_key);
    assert_eq!(b2.route_key.route_revision.0, 2, "current + 1");
    let h = tick(&mut s, 6_500, None).horizon.unwrap();
    assert_eq!(h.route_key, b2.route_key);
    assert_eq!(h.status, HorizonStatus::Uncertain);
    let h = tick(&mut s, 7_000, Some(fix_xy(&b2, 7_000, 20_000, 0)))
        .horizon
        .unwrap();
    assert_eq!(h.status, HorizonStatus::Tracking);
}

fn whats(o: &TickOutput) -> Vec<&str> {
    o.log.iter().map(|l| l.what.as_str()).collect()
}

fn commit_of(o: &TickOutput) -> Vec<(&str, &RouteKey)> {
    o.planner_cmds
        .iter()
        .filter_map(|c| match c {
            PlannerCmd::Commit {
                req_id, route_key, ..
            } => Some((req_id.as_str(), route_key)),
            _ => None,
        })
        .collect()
}

/// Tracking at 0, an explicit-plan search r1 from 100, no fix until 3 s → uncertain.
fn uncertain_with_search() -> (Session, RouteBundle) {
    let (mut s, b) = committed();
    tick(&mut s, 0, Some(fix_xy(&b, 0, 10_000, 0)));
    s.request_search(100, SearchPriority::ExplicitPlan).unwrap();
    assert_eq!(s.horizon_status(3_000), Some(HorizonStatus::Uncertain));
    (s, b)
}

/// 04 §3: an ok returned while `uncertain` is held (not committed, not discarded) and
/// committed once the status is back to tracking, with the start-time route_key.
#[test]
fn ok_while_uncertain_is_held_and_committed_on_tracking() {
    let (mut s, b) = uncertain_with_search();
    let o = s.tick(finished("r1", &b.route_key, 3_000));
    assert!(o.planner_cmds.is_empty());
    assert!(whats(&o).contains(&"commit_held_uncertain"));
    assert!(!whats(&o).contains(&"discarded_not_tracking"));
    assert_eq!(s.planner().held().map(|h| h.req_id.as_str()), Some("r1"));
    // the hold expiry is a wake-up time (held_at + 10 s + 1 ms)
    assert!(o.next_wake_mono_ms.is_some_and(|w| w <= 13_001));
    // still uncertain: nothing yet
    let o = tick(&mut s, 3_500, None);
    assert!(o.planner_cmds.is_empty());
    // a fix back on the route → tracking → commit{r1, route_key at the start}
    let o = tick(&mut s, 4_000, Some(fix_xy(&b, 4_000, 16_000, 0)));
    assert_eq!(s.horizon_status(4_000), Some(HorizonStatus::Tracking));
    assert_eq!(commit_of(&o), vec![("r1", &b.route_key)]);
    let l = o.log.iter().find(|l| l.what == "commit_issued").unwrap();
    assert_eq!(l.detail.as_deref(), Some("from_hold"));
    assert!(s.planner().held().is_none());
    // only once
    let o = tick(&mut s, 5_000, Some(fix_xy(&b, 5_000, 17_500, 0)));
    assert!(commit_of(&o).is_empty());
}

/// While tracking the same ok is committed right away (no hold).
#[test]
fn ok_while_tracking_is_committed_at_once() {
    let (mut s, b) = committed();
    tick(&mut s, 0, Some(fix_xy(&b, 0, 10_000, 0)));
    s.request_search(100, SearchPriority::ExplicitPlan).unwrap();
    tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 11_000, 0)));
    assert_eq!(s.horizon_status(1_500), Some(HorizonStatus::Tracking));
    let o = s.tick(finished("r1", &b.route_key, 1_500));
    assert!(matches!(&o.planner_cmds[..], [PlannerCmd::Commit { .. }]));
    assert!(s.planner().held().is_none());
}

/// The hold lasts 10 s (inclusive); past it the result is `discarded_hold_expired` and a
/// later return to tracking commits nothing.
#[test]
fn held_result_expires_after_hold_limit() {
    // exactly 10 s after the hold: still committed
    let (mut s, b) = uncertain_with_search();
    s.tick(finished("r1", &b.route_key, 3_000));
    let o = tick(&mut s, 13_000, Some(fix_xy(&b, 13_000, 20_000, 0)));
    assert_eq!(commit_of(&o), vec![("r1", &b.route_key)]);
    // 10 s + 1 ms: expired, even if the same tick brings tracking back
    let (mut s, b) = uncertain_with_search();
    s.tick(finished("r1", &b.route_key, 3_000));
    let o = tick(&mut s, 13_001, Some(fix_xy(&b, 13_001, 20_000, 0)));
    assert!(commit_of(&o).is_empty());
    assert!(whats(&o).contains(&"discarded_hold_expired"));
    // expiry without input (the wake-up tick), then tracking: nothing
    let (mut s, b) = uncertain_with_search();
    s.tick(finished("r1", &b.route_key, 3_000));
    let o = tick(&mut s, 13_001, None);
    assert!(whats(&o).contains(&"discarded_hold_expired"));
    let o = tick(&mut s, 14_000, Some(fix_xy(&b, 14_000, 20_000, 0)));
    assert!(commit_of(&o).is_empty());
}

/// The hold limit comes from SessionConfig (`commit_hold_ms`).
#[test]
fn hold_limit_is_configurable() {
    let mut s = Session::from_json(br#"{"commit_hold_ms": 2000}"#).unwrap();
    let b = main_line();
    s.route_commit(&RouteCommitRequest {
        req_id: None,
        expected_route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    let b = as_committed(&s, b);
    tick(&mut s, 0, Some(fix_xy(&b, 0, 10_000, 0)));
    s.request_search(100, SearchPriority::ExplicitPlan).unwrap();
    s.tick(finished("r1", &b.route_key, 3_000));
    let o = tick(&mut s, 5_001, Some(fix_xy(&b, 5_001, 16_000, 0)));
    assert!(commit_of(&o).is_empty());
    assert!(whats(&o).contains(&"discarded_hold_expired"));
}

/// Only one result is held: a newer ok replaces the older one.
#[test]
fn newer_ok_replaces_the_held_one() {
    let (mut s, b) = uncertain_with_search();
    s.tick(finished("r1", &b.route_key, 3_000));
    s.request_search(3_100, SearchPriority::ExplicitPlan)
        .unwrap();
    let o = s.tick(finished("r2", &b.route_key, 3_200));
    assert!(o
        .log
        .iter()
        .any(|l| l.what == "discarded_hold_replaced" && l.req_id.as_deref() == Some("r1")));
    assert_eq!(s.planner().held().map(|h| h.req_id.as_str()), Some("r2"));
    let o = tick(&mut s, 4_000, Some(fix_xy(&b, 4_000, 16_000, 0)));
    assert_eq!(commit_of(&o), vec![("r2", &b.route_key)]);
}

/// The route changed while the result was held: its route_key no longer matches, no
/// commit (the hold is dropped on the route change).
#[test]
fn held_result_for_a_replaced_route_is_not_committed() {
    let (mut s, b) = uncertain_with_search();
    s.tick(finished("r1", &b.route_key, 3_000));
    let b2 = main_line();
    // another commit against the same route (stands in for an explicit plan)
    s.issue_commit_for_test("r9", b.route_key.clone());
    s.route_commit(&RouteCommitRequest {
        req_id: Some("r9".into()),
        expected_route_key: Some(b.route_key.clone()),
        bundle: b2.clone(),
    })
    .unwrap();
    let b2 = as_committed(&s, b2);
    assert!(s.planner().held().is_none());
    let o = tick(&mut s, 4_000, Some(fix_xy(&b2, 4_000, 16_000, 0)));
    assert!(commit_of(&o).is_empty());
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

#[test]
fn group_limits_and_voice_lead_use_the_approach_edge() {
    for (distance, motorway, time_per_cm, paired) in [
        (12_000, false, 2, true),
        (12_001, false, 2, false),
        (20_000, true, 2, true),
        (20_001, true, 2, false),
        (24_000, false, 0, true),
        (24_002, false, 0, false),
    ] {
        let mut b = main_line();
        let leg = &mut b.legs[0];
        let mut cumulative_cm = 0;
        for edge in &mut leg.edges {
            cumulative_cm += edge.length_cm;
            edge.cum_time_ms = if time_per_cm == 0 {
                cumulative_cm / 2
            } else {
                cumulative_cm * time_per_cm
            };
            if motorway {
                edge.flags |= contracts::route::EDGE_FLAG_MOTORWAY;
            }
        }
        b.eta_ms = leg.edges.last().unwrap().cum_time_ms;
        // Anchor all three maneuvers on the same axis; only the first pair may group.
        for (m, along) in
            leg.maneuvers
                .iter_mut()
                .zip([100_000, 100_000 + distance, 100_000 + distance + 100])
        {
            let i = leg.chunks[0].seg_along_cm.partition_point(|a| *a <= along) - 1;
            m.anchor.leg_shape_index = i as u32;
            m.anchor.along_route_cm = along;
        }
        let mut s = Session::new(SessionConfig::default());
        s.route_commit(&RouteCommitRequest {
            req_id: None,
            expected_route_key: None,
            bundle: b.clone(),
        })
        .unwrap();
        let h = tick(&mut s, 50_000, Some(fix_xy(&b, 50_000, 0, 0)))
            .horizon
            .unwrap();
        let head = &h.upcoming[0];
        assert_eq!(
            head.group_id.is_some(),
            paired,
            "{distance}, motorway={motorway}"
        );
        if paired {
            assert_eq!(head.group_id.as_ref(), Some(&head.maneuver.id));
            assert_eq!(h.upcoming[1].group_id, head.group_id);
            assert!(h.upcoming[2].group_id.is_none());
        }
        let eta = head.eta_range_ms.unwrap();
        let lead = if motorway { 20_000 } else { 8_000 };
        assert_eq!(
            h.next_p1_deadline_monotonic_ms,
            Some(50_000 + u64::from(eta.typical) - lead)
        );
    }
}

#[test]
fn last_junction_expires_after_either_inclusive_limit() {
    let mut b = main_line();
    let m = &mut b.legs[0].maneuvers[0];
    m.arrow = Arrow::Right;
    let evidence = |value| {
        serde_json::from_value(
            serde_json::json!({"kind":"confirmed","value":value,"source_ref":"test"}),
        )
        .unwrap()
    };
    m.approach_signal = evidence("absent");
    m.crossing = evidence("across_oncoming");
    let anchor = m.anchor.along_route_cm;
    for time_limit in [true, false] {
        let mut s = Session::new(SessionConfig {
            fix_stale_ms: 200_000,
            ..Default::default()
        });
        s.route_commit(&RouteCommitRequest {
            req_id: None,
            expected_route_key: None,
            bundle: b.clone(),
        })
        .unwrap();
        for t in [1_000, 2_000] {
            let h = tick(&mut s, t, Some(fix_xy(&b, t, 150_100, 0)))
                .horizon
                .unwrap();
            assert_eq!(h.refs.last.is_some(), t == 2_000);
            if let Some(r) = h.refs.last {
                assert_eq!((r.passed_along_cm, r.passed_at_mono_ms), (anchor, 2_000));
            }
        }
        if time_limit {
            assert_eq!(tick(&mut s, 182_000, None).next_wake_mono_ms, Some(182_001));
            assert!(tick(&mut s, 182_001, None)
                .horizon
                .unwrap()
                .refs
                .last
                .is_none());
        } else {
            assert!(tick(&mut s, 3_000, Some(fix_xy(&b, 3_000, 300_000, 0)))
                .horizon
                .unwrap()
                .refs
                .last
                .is_some());
            assert!(tick(&mut s, 4_000, Some(fix_xy(&b, 4_000, 300_010, 0)))
                .horizon
                .unwrap()
                .refs
                .last
                .is_none());
        }
    }
}

#[test]
fn ok_after_arrival_is_not_committed() {
    let (mut s, b) = committed();
    tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 400_100, 0)));
    s.request_search(1_000, SearchPriority::ExplicitPlan)
        .unwrap();
    tick(&mut s, 2_000, Some(fix_xy(&b, 2_000, 400_100, 0)));
    assert_eq!(s.horizon_status(2_100), Some(HorizonStatus::Finished));
    let o = s.tick(finished("r1", &b.route_key, 2_100));
    assert!(o.planner_cmds.is_empty());
    assert!(o.log.iter().any(|l| l.what == "discarded_not_tracking"));
}

#[test]
fn roundabout_exit_still_has_a_p1_deadline_after_entrance_passage() {
    let mut b = main_line();
    let m = &mut b.legs[0].maneuvers[0];
    m.arrow = Arrow::Roundabout {
        exit_number: Some(2),
    };
    let mut exit = m.anchor;
    exit.leg_shape_index = 17;
    exit.along_route_cm = 175_000;
    m.roundabout_leave_anchor = Some(exit);
    let mut s = Session::new(SessionConfig::default());
    s.route_commit(&RouteCommitRequest {
        req_id: None,
        expected_route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    tick(&mut s, 40_000, Some(fix_xy(&b, 40_000, 155_000, 0)));
    let frame = tick(&mut s, 41_000, Some(fix_xy(&b, 41_000, 155_000, 0)))
        .horizon
        .unwrap();
    assert_eq!(frame.upcoming[0].maneuver.arrow, Arrow::Right);
    let r = s.route().unwrap();
    let exit_eta = r.time_at(exit.along_route_cm) - r.time_at(frame.matched_progress_cm.unwrap());
    assert_eq!(
        frame.next_p1_deadline_monotonic_ms,
        Some(41_000 + u64::from(exit_eta) - 8_000)
    );
}

/// (crossing, approach_signal, right_turn_protection), all confirmed.
type JunctionEvidence = (&'static str, &'static str, &'static str);

/// 04 §2 / 03 §4.1: `refs` point at difficult junctions (難所) only (`right_unprotected`
/// for now), so passing or approaching ordinary intersections creates no `JunctionRef` /
/// `PassedRef`; they still carry the P1 guidance deadline (04 §3). They start no complex
/// window either, except a confirmed protected right turn across oncoming traffic
/// (`right_protected` is a complex-window trigger, team decision 2026-11-26).
#[test]
fn ordinary_junctions_create_no_refs_but_keep_p1_deadlines() {
    let ev = |value: &str| -> contracts::horizon::Evidence {
        serde_json::from_value(
            serde_json::json!({"kind":"confirmed","value":value,"source_ref":"test"}),
        )
        .unwrap()
    };
    let cases: Vec<(&str, Arrow, Option<JunctionEvidence>)> = vec![
        ("left", Arrow::Left, None),
        ("slight_left", Arrow::SlightLeft, None),
        ("keep_left", Arrow::KeepLeft, None),
        ("fork_right", Arrow::ForkRight, None),
        ("straight_decision", Arrow::StraightDecision, None),
        (
            "roundabout",
            Arrow::Roundabout {
                exit_number: Some(2),
            },
            None,
        ),
        (
            "right_no_crossing",
            Arrow::Right,
            Some(("no_crossing", "absent", "unprotected")),
        ),
        (
            "right_signalled_protected",
            Arrow::Right,
            Some(("across_oncoming", "present", "protected")),
        ),
    ];
    for (name, arrow, evidence) in cases {
        let mut b = main_line();
        for m in &mut b.legs[0].maneuvers[..3] {
            m.arrow = arrow;
            if let Some((c, sig, p)) = evidence {
                m.crossing = ev(c);
                m.approach_signal = ev(sig);
                m.right_turn_protection = ev(p);
            }
        }
        let mut s = Session::new(SessionConfig {
            fix_stale_ms: 200_000,
            ..Default::default()
        });
        s.route_commit(&RouteCommitRequest {
            req_id: None,
            expected_route_key: None,
            bundle: b.clone(),
        })
        .unwrap();
        let mut seen = 0;
        for (t, x) in [(1_000, 140_000), (2_000, 150_100), (3_000, 150_200)] {
            if let Some(h) = tick(&mut s, t, Some(fix_xy(&b, t, x, 0))).horizon {
                seen += 1;
                assert_eq!(h.status, HorizonStatus::Tracking, "{name}");
                assert_eq!(h.refs.next, None, "{name}");
                assert_eq!(h.refs.last, None, "{name}: passing is not a PassedRef");
                assert!(h.next_p1_deadline_monotonic_ms.is_some(), "{name}");
                assert_eq!(
                    h.next_complex_window.is_some(),
                    name == "right_signalled_protected",
                    "{name}"
                );
            }
        }
        assert!(seen > 0, "{name}");
    }
}
