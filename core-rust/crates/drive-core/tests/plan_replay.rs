//! Deterministic replay of a plan's candidate searches: `Session::plan_candidates` →
//! one `start` at a time → `rank` (all_done | budget_expired). Team decision 2026-11-25.
use contracts::plan::{CandidateLabel, CostingVariant, TripPlanSnapshot};
use contracts::route::{RouteBundle, RouteCommitRequest};
use contracts::tick::{Fix, PlannerCmd, RankReason, SearchPriority, TickInput, TickOutput};
use contracts::{ErrorCode, RouteKey, U64Str};
use drive_core::candidates::candidate_requests;
use drive_core::planner::{PlanCommitState, PlanOkSearch, Slot};
use drive_core::{Session, SessionConfig};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let p = format!(
        "{}/../../tests/fixtures/tick/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap()
}

fn snapshot(max_requests: u32, vias: usize) -> TripPlanSnapshot {
    let mut v = fixture("plan-budget-expired.json")["snapshot"].clone();
    v["budget"]["max_requests"] = json!(max_requests);
    v["vias"] = json!(vec![json!({"lat": 35.0, "lon": 139.0}); vias]);
    serde_json::from_value(v).unwrap()
}

fn plan_key() -> Value {
    json!({"trip_id":"t1","trip_revision":"3","route_revision":"0","graph_version":"jp-2026.11.0"})
}

fn finished(now: u64, req_id: &str, outcome: &str, facts: Option<&str>) -> TickInput {
    let mut e = json!({"at_mono_ms": now, "kind": "search_finished", "req_id": req_id,
        "route_key": plan_key(), "graph_version": "jp-2026.11.0", "outcome": outcome, "engine_ms": 1});
    if let Some(f) = facts {
        e["facts_ref"] = json!(f);
    }
    serde_json::from_value(json!({"now_mono_ms": now, "events": [e]})).unwrap()
}

fn at(now: u64) -> TickInput {
    serde_json::from_value(json!({ "now_mono_ms": now })).unwrap()
}

fn whats(log: &[contracts::tick::LogEvent]) -> Vec<&str> {
    log.iter().map(|l| l.what.as_str()).collect()
}

fn starts(cmds: &[PlannerCmd]) -> Vec<(String, SearchPriority, Option<String>)> {
    cmds.iter()
        .filter_map(|c| match c {
            PlannerCmd::Start {
                req_id,
                priority,
                candidate_id,
                ..
            } => Some((req_id.clone(), *priority, candidate_id.clone())),
            _ => None,
        })
        .collect()
}

fn ranks(cmds: &[PlannerCmd]) -> Vec<(Vec<String>, RankReason)> {
    cmds.iter()
        .filter_map(|c| match c {
            PlannerCmd::Rank {
                facts_refs, reason, ..
            } => Some((facts_refs.clone(), *reason)),
            _ => None,
        })
        .collect()
}

#[test]
fn budget_expired_fixture_leaves_no_waiting_search() {
    let fx = fixture("plan-budget-expired.json");
    let mut s = Session::new(SessionConfig::default());
    s.set_current_route_key(serde_json::from_value::<RouteKey>(fx["route_key"].clone()).unwrap());
    let snap: TripPlanSnapshot = serde_json::from_value(fx["snapshot"].clone()).unwrap();
    let (mut running, mut max_running) = (0usize, 0usize);
    let mut logs = Vec::new();
    for (i, step) in fx["steps"].as_array().unwrap().iter().enumerate() {
        let now = step["now_mono_ms"].as_u64();
        let (cmds, log, next_wake) = match step["step"].as_str().unwrap() {
            "plan_candidates" => {
                let o = s.plan_candidates(now.unwrap(), None, &snap);
                assert_eq!(o.candidates.len(), 3);
                (o.planner_cmds, o.log, o.next_wake_mono_ms)
            }
            "request" => {
                let pr = serde_json::from_value(step["priority"].clone()).unwrap();
                let o = s.request_search(now.unwrap(), pr).unwrap();
                (o.planner_cmds, o.log, o.next_wake_mono_ms)
            }
            "tick" => {
                let o: TickOutput = s.tick(serde_json::from_value(step["input"].clone()).unwrap());
                (o.planner_cmds, o.log, o.next_wake_mono_ms)
            }
            o => panic!("{o}"),
        };
        assert_eq!(
            serde_json::to_value(&cmds).unwrap(),
            step["expect_cmds"],
            "step {i}"
        );
        let fin = step["input"]["events"].as_array().map_or(0, |e| {
            e.iter().filter(|e| e["kind"] == "search_finished").count()
        });
        running -= fin.min(running);
        running += starts(&cmds).len();
        max_running = max_running.max(running);
        logs.push((i, next_wake, log));
    }
    assert_eq!(max_running, 1, "at most one search at a time");
    // plan start → wake at the comparison budget (8000) before the watchdog (10000)
    assert_eq!(logs[0].1, Some(8_000));
    // 8000: budget expired, c3 dropped without a start; r2 cancelled but still holds the slot
    let w = whats(&logs[3].2);
    assert!(w.contains(&"comparison_budget_expired"));
    assert!(w.contains(&"plan_ranked"));
    assert_eq!(w.iter().filter(|x| **x == "plan_search_dropped").count(), 1);
    assert_eq!(s.planner().plan().unwrap().queue.len(), 0);
    // watchdog of r2 (started 2000) still drives the wake while it holds the slot
    assert_eq!(logs[3].1, Some(12_000));
    // r2's late ok only frees the slot
    assert!(whats(&logs[5].2).contains(&"discarded_late"));
    assert!(
        matches!(s.planner().slot(), Slot::Running { req } if req.priority == SearchPriority::Reroute)
    );
}

#[test]
fn all_done_ranks_every_ok_in_request_order_and_stops() {
    let mut s = Session::new(SessionConfig::default());
    let o = s.plan_candidates(0, Some(600), &snapshot(5, 0));
    assert_eq!(o.distance_class, contracts::tick::DistanceClass::Short);
    assert_eq!(
        starts(&o.planner_cmds),
        vec![("r1".into(), SearchPriority::ExplicitPlan, Some("c1".into()))]
    );
    let o = s.tick(finished(500, "r1", "ok", Some("f1")));
    assert_eq!(
        starts(&o.planner_cmds),
        vec![("r2".into(), SearchPriority::Comparison, Some("c2".into()))]
    );
    // an error does not stop the plan and contributes no facts
    let o = s.tick(finished(1_000, "r2", "error", None));
    assert_eq!(
        starts(&o.planner_cmds),
        vec![("r3".into(), SearchPriority::Comparison, Some("c3".into()))]
    );
    let o = s.tick(finished(1_500, "r3", "ok", Some("f3")));
    assert_eq!(
        ranks(&o.planner_cmds),
        vec![(vec!["f1".into(), "f3".into()], RankReason::AllDone)]
    );
    assert!(starts(&o.planner_cmds).is_empty());
    assert!(
        o.ui_cmds.is_empty(),
        "candidate results are never displayed alone"
    );
    assert_eq!(o.next_wake_mono_ms, None, "plan over: no budget wake");
    // budget time passing afterwards emits nothing more
    let o = s.tick(at(10_000));
    assert!(o.planner_cmds.is_empty());
}

#[test]
fn budget_expiry_while_baseline_runs_ranks_empty_and_cancels() {
    let mut s = Session::new(SessionConfig::default());
    s.plan_candidates(0, Some(600), &snapshot(3, 0));
    let o = s.tick(at(3_000));
    assert_eq!(
        ranks(&o.planner_cmds),
        vec![(vec![], RankReason::BudgetExpired)]
    );
    assert!(o.planner_cmds.contains(&PlannerCmd::Cancel {
        req_id: "r1".into()
    }));
    assert_eq!(
        whats(&o.log)
            .iter()
            .filter(|x| **x == "plan_search_dropped")
            .count(),
        2
    );
    assert!(matches!(s.planner().slot(), Slot::CancelRequested { .. }));
    // the running search keeps the slot: watchdog (5000) still requested
    assert_eq!(o.next_wake_mono_ms, Some(5_000));
    // the stuck call keeps the slot until its real return; no new start ever comes for p1
    let o = s.tick(at(5_000));
    assert!(whats(&o.log).contains(&"engine_stuck"));
    assert!(o.planner_cmds.is_empty(), "cancel already sent");
    let o = s.tick(finished(20_000, "r1", "ok", Some("f1")));
    assert!(whats(&o.log).contains(&"discarded_after_stuck"));
    assert!(o.planner_cmds.is_empty());
    assert_eq!(*s.planner().slot(), Slot::Idle);
}

#[test]
fn reroute_preempts_plan_search_and_plan_continues_after() {
    let mut s = Session::new(SessionConfig::default());
    s.set_current_route_key(
        serde_json::from_value(json!({"trip_id":"t1","trip_revision":"3","route_revision":"7","graph_version":"jp-2026.11.0"}))
            .unwrap(),
    );
    s.plan_candidates(0, None, &snapshot(2, 0));
    s.tick(finished(100, "r1", "ok", Some("f1"))); // r2 = c2 comparison starts
    let o = s.request_search(200, SearchPriority::Reroute).unwrap();
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r2".into()
        }]
    );
    let o = s.tick(finished(300, "r2", "cancelled", None));
    // queue empty after c2 → rank all_done, then the reroute takes the slot
    assert_eq!(
        ranks(&o.planner_cmds),
        vec![(vec!["f1".into()], RankReason::AllDone)]
    );
    assert_eq!(
        starts(&o.planner_cmds),
        vec![("r3".into(), SearchPriority::Reroute, None)]
    );
}

#[test]
fn same_plan_request_is_not_requeued_and_new_one_supersedes() {
    let mut s = Session::new(SessionConfig::default());
    s.plan_candidates(0, None, &snapshot(3, 0));
    let o = s.plan_candidates(100, Some(60), &snapshot(3, 0));
    assert!(o.planner_cmds.is_empty());
    assert_eq!(o.distance_class, contracts::tick::DistanceClass::Long);
    assert_eq!(s.planner().plan().unwrap().queue.len(), 2);
    let mut p2 = snapshot(1, 0);
    p2.plan_request_id = "p2".into();
    let o = s.plan_candidates(200, None, &p2);
    assert!(whats(&o.log).contains(&"plan_superseded"));
    // p1's running r1 is cancelled (no rank for a superseded plan); p2 waits for the slot
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    let o = s.tick(finished(300, "r1", "ok", Some("f1")));
    assert!(whats(&o.log).contains(&"discarded_late"));
    assert_eq!(
        starts(&o.planner_cmds),
        vec![("r2".into(), SearchPriority::ExplicitPlan, Some("c1".into()))]
    );
}

#[test]
fn candidate_requests_follow_caps_and_vias() {
    let c = candidate_requests(&snapshot(7, 0));
    assert_eq!(
        c.len(),
        3,
        "baseline + 2 costing variants (no through_anchor yet)"
    );
    assert_eq!(c[0].label, CandidateLabel::Baseline);
    assert_eq!(c[0].engine_alternates, 2);
    assert_eq!(c[1].costing_variant, Some(CostingVariant::PreferTrunk));
    assert_eq!(c[2].costing_variant, Some(CostingVariant::ServicePenalty));
    assert_eq!(candidate_requests(&snapshot(1, 0)).len(), 1);
    assert_eq!(candidate_requests(&snapshot(2, 1))[0].engine_alternates, 0);
}

// ---- Plan candidate commit by req_id (02 §2.1 #9, 03 §3.1) --------------------------------

fn min_bundle_for(trip_id: &str, trip_rev: u64, graph_version: &str) -> RouteBundle {
    let p = format!(
        "{}/../../../schema/fixtures/valid/route-bundle.min.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut b: RouteBundle = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    b.route_key = RouteKey {
        trip_id: trip_id.into(),
        trip_revision: U64Str(trip_rev),
        route_revision: U64Str(0),
        graph_version: graph_version.into(),
    };
    for leg in &mut b.legs {
        for m in &mut leg.maneuvers {
            m.id.route_key = b.route_key.clone();
        }
    }
    b
}

fn finished_for(now: u64, req_id: &str, trip_rev: u64, facts: &str) -> TickInput {
    let rk = json!({
        "trip_id": "t1",
        "trip_revision": trip_rev.to_string(),
        "route_revision": "0",
        "graph_version": "jp-2026.11.0"
    });
    serde_json::from_value(json!({
        "now_mono_ms": now,
        "events": [{
            "at_mono_ms": now,
            "kind": "search_finished",
            "req_id": req_id,
            "route_key": rk,
            "graph_version": "jp-2026.11.0",
            "outcome": "ok",
            "facts_ref": facts,
            "engine_ms": 10
        }]
    }))
    .unwrap()
}

fn fix_full(at: u64, speed_cm_s: Option<u32>, acc_cm: u32) -> TickInput {
    TickInput {
        now_mono_ms: at,
        fix: Some(Fix {
            at_mono_ms: at,
            lat_e7: 350_000_000,
            lon_e7: 1_390_000_000,
            acc_cm,
            bearing_cdeg: Some(9_000),
            speed_cm_s,
        }),
        events: vec![],
    }
}

fn fix_speed(at: u64, speed_cm_s: u32) -> TickInput {
    fix_full(at, Some(speed_cm_s), 300)
}

/// First route is loaded by the `req_id` of an `ok` candidate of the current plan (even
/// after `rank` closed new searches); once one candidate is committed, the plan's commit
/// window closes and another candidate of the same plan is `E_STALE`.
#[test]
fn first_route_committed_by_plan_candidate_req_id() {
    let mut s = Session::new(SessionConfig::default());
    let snap = snapshot(2, 0);
    let o = s.plan_candidates(100, Some(600), &snap);
    assert_eq!(starts(&o.planner_cmds)[0].0, "r1");
    let o = s.tick(finished_for(200, "r1", 3, "f1"));
    assert_eq!(starts(&o.planner_cmds)[0].0, "r2");
    let o = s.tick(finished_for(300, "r2", 3, "f2"));
    assert_eq!(
        ranks(&o.planner_cmds),
        vec![(vec!["f1".into(), "f2".into()], RankReason::AllDone)]
    );
    let plan = s.planner().plan().unwrap();
    assert!(plan.searches_closed, "rank closes new searches");
    assert_eq!(plan.commit_state, PlanCommitState::Open);
    assert_eq!(plan.trip_revision, Some(U64Str(3)));
    assert_eq!(
        plan.ok_searches,
        vec![
            PlanOkSearch {
                req_id: "r1".into(),
                candidate_id: "c1".into(),
                facts_ref: "f1".into(),
            },
            PlanOkSearch {
                req_id: "r2".into(),
                candidate_id: "c2".into(),
                facts_ref: "f2".into(),
            },
        ]
    );
    // Unknown req_id is E_STALE
    let b = min_bundle_for("t1", 3, "jp-2026.11.0");
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: "r99".into(),
            expected_route_key: None,
            bundle: b.clone(),
        })
        .unwrap_err()
        .code,
        ErrorCode::Stale
    );
    // Bundle trip_revision mismatching the plan is E_STALE
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: "r2".into(),
            expected_route_key: None,
            bundle: min_bundle_for("t1", 4, "jp-2026.11.0"),
        })
        .unwrap_err()
        .code,
        ErrorCode::Stale
    );
    // Commit r2 (second candidate) as the first route -> rev 1
    let r = s
        .route_commit(&RouteCommitRequest {
            req_id: "r2".into(),
            expected_route_key: None,
            bundle: b.clone(),
        })
        .unwrap();
    assert_eq!(r.route_key.trip_revision, U64Str(3));
    assert_eq!(r.route_key.route_revision, U64Str(1));
    assert_eq!(
        s.planner().plan().unwrap().commit_state,
        PlanCommitState::Closed
    );
    // Another candidate of the same plan (r1) is now E_STALE
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: "r1".into(),
            expected_route_key: Some(r.route_key),
            bundle: b,
        })
        .unwrap_err()
        .code,
        ErrorCode::Stale
    );
}

/// Mid-trip re-plan (`trip_revision` + 1): allowed only via a plan candidate whose plan
/// carries `current + 1`, and `route_revision` continues at `current + 1` without resetting.
/// Skipping a revision (+2) or mismatching the plan's `trip_revision` returns `E_STALE`.
#[test]
fn mid_trip_replan_advances_trip_revision_without_resetting_route_revision() {
    let mut s = Session::new(SessionConfig::default());
    // Initial plan p1 (trip_revision 3) -> route_revision 1
    s.plan_candidates(0, Some(600), &snapshot(1, 0));
    s.tick(finished_for(100, "r1", 3, "f1"));
    let k1 = s
        .route_commit(&RouteCommitRequest {
            req_id: "r1".into(),
            expected_route_key: None,
            bundle: min_bundle_for("t1", 3, "jp-2026.11.0"),
        })
        .unwrap()
        .route_key;
    assert_eq!((k1.trip_revision.0, k1.route_revision.0), (3, 1));

    // A reroute (issued commit) keeps trip_revision 3 and bumps route_revision to 2
    s.issue_commit_for_test("rr1", k1.clone());
    // Issued commit cannot advance trip_revision to 4
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: "rr1".into(),
            expected_route_key: Some(k1.clone()),
            bundle: min_bundle_for("t1", 4, "jp-2026.11.0"),
        })
        .unwrap_err()
        .code,
        ErrorCode::Stale
    );
    let k2 = s
        .route_commit(&RouteCommitRequest {
            req_id: "rr1".into(),
            expected_route_key: Some(k1),
            bundle: min_bundle_for("t1", 3, "jp-2026.11.0"),
        })
        .unwrap()
        .route_key;
    assert_eq!((k2.trip_revision.0, k2.route_revision.0), (3, 2));

    // Mid-trip replan p2 with trip_revision = 4 (current 3 + 1)
    let mut p2 = snapshot(1, 0);
    p2.plan_request_id = "p2".into();
    p2.trip_revision = U64Str(4);
    let o = s.plan_candidates(200, Some(600), &p2);
    let r2 = starts(&o.planner_cmds)[0].0.clone();
    s.tick(finished_for(300, &r2, 4, "f2"));
    // Bundle with trip_revision 3 does not match p2's trip_revision (4) -> E_STALE
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: r2.clone(),
            expected_route_key: Some(k2.clone()),
            bundle: min_bundle_for("t1", 3, "jp-2026.11.0"),
        })
        .unwrap_err()
        .code,
        ErrorCode::Stale
    );
    // Bundle with trip_revision 4 matches p2 and is current + 1 -> route_revision = 3 (not 1!)
    let k3 = s
        .route_commit(&RouteCommitRequest {
            req_id: r2,
            expected_route_key: Some(k2),
            bundle: min_bundle_for("t1", 4, "jp-2026.11.0"),
        })
        .unwrap()
        .route_key;
    assert_eq!((k3.trip_revision.0, k3.route_revision.0), (4, 3));

    // Replan p3 skipping trip_revision to 6 (current 4 + 2) -> E_STALE
    let mut p3 = snapshot(1, 0);
    p3.plan_request_id = "p3".into();
    p3.trip_revision = U64Str(6);
    let o = s.plan_candidates(400, Some(600), &p3);
    let r3 = starts(&o.planner_cmds)[0].0.clone();
    s.tick(finished_for(500, &r3, 6, "f3"));
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: r3,
            expected_route_key: Some(k3.clone()),
            bundle: min_bundle_for("t1", 6, "jp-2026.11.0"),
        })
        .unwrap_err()
        .code,
        ErrorCode::Stale
    );
    assert_eq!(s.route().unwrap().route_key, k3, "nothing replaced");

    // Replan p4 keeping trip_revision 4 (same as current) -> succeeds, route_revision = 4
    let mut p4 = snapshot(1, 0);
    p4.plan_request_id = "p4".into();
    p4.trip_revision = U64Str(4);
    let o = s.plan_candidates(600, Some(600), &p4);
    let r4 = starts(&o.planner_cmds)[0].0.clone();
    s.tick(finished_for(700, &r4, 4, "f4"));
    let k4 = s
        .route_commit(&RouteCommitRequest {
            req_id: r4,
            expected_route_key: Some(k3),
            bundle: min_bundle_for("t1", 4, "jp-2026.11.0"),
        })
        .unwrap()
        .route_key;
    assert_eq!((k4.trip_revision.0, k4.route_revision.0), (4, 4));
}

/// Candidates of a superseded (old) plan are rejected with `E_STALE`.
#[test]
fn old_plan_candidate_returns_e_stale() {
    let mut s = Session::new(SessionConfig::default());
    s.plan_candidates(0, Some(600), &snapshot(1, 0));
    s.tick(finished_for(100, "r1", 3, "f1"));

    // A new plan p2 supersedes p1
    let mut p2 = snapshot(1, 0);
    p2.plan_request_id = "p2".into();
    let o = s.plan_candidates(200, Some(600), &p2);
    let r2 = starts(&o.planner_cmds)[0].0.clone();
    s.tick(finished_for(300, &r2, 3, "f2"));

    let b = min_bundle_for("t1", 3, "jp-2026.11.0");
    // Candidate r1 of old plan p1 -> E_STALE
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: "r1".into(),
            expected_route_key: None,
            bundle: b.clone(),
        })
        .unwrap_err()
        .code,
        ErrorCode::Stale
    );
    assert!(s.route().is_none());
    // Candidate r2 of current plan p2 -> succeeds
    let k1 = s
        .route_commit(&RouteCommitRequest {
            req_id: r2,
            expected_route_key: None,
            bundle: b,
        })
        .unwrap()
        .route_key;
    assert_eq!(k1.route_revision, U64Str(1));
}

/// Soft-mode / drive-start rule (02 §2.1 #9, 03 §3.1, 07 §1):
/// - `>= 8 km/h` (222 cm/s) continuously for 2.9 s (2900 ms) still accepts plan-candidate commit;
/// - at 3.0 s (3000 ms) continuous, plan-candidate commit is refused with
///   `FORBIDDEN_WHILE_DRIVING` (both before the first route and during a mid-trip replan),
///   while issued commits (reroute / avoid_chip / undo) still succeed;
/// - dropping below 222 cm/s (e.g. 7 km/h), missing `speed_cm_s`, or low accuracy
///   (`acc_cm > max_acc_cm`) resets the continuous duration.
#[test]
fn plan_candidate_after_driving_returns_forbidden_while_driving() {
    // 1) 2.9 s (2900 ms) at 222 cm/s (8 km/h) -> STILL accepted
    let mut s = Session::new(SessionConfig::default());
    s.plan_candidates(0, Some(600), &snapshot(1, 0));
    s.tick(finished_for(100, "r1", 3, "f1"));
    s.tick(fix_speed(1_000, 222));
    s.tick(fix_speed(2_500, 222));
    s.tick(fix_speed(3_900, 222)); // 2900 ms continuous
    assert_eq!(
        s.planner().plan().unwrap().commit_state,
        PlanCommitState::Open
    );
    let k1 = s
        .route_commit(&RouteCommitRequest {
            req_id: "r1".into(),
            expected_route_key: None,
            bundle: min_bundle_for("t1", 3, "jp-2026.11.0"),
        })
        .unwrap()
        .route_key;
    assert_eq!(k1.route_revision, U64Str(1));

    // 2) 3.0 s (3000 ms) at 222 cm/s before first route -> FORBIDDEN_WHILE_DRIVING
    let mut s = Session::new(SessionConfig::default());
    s.plan_candidates(0, Some(600), &snapshot(1, 0));
    s.tick(finished_for(100, "r1", 3, "f1"));
    s.tick(fix_speed(1_000, 222));
    s.tick(fix_speed(3_900, 222)); // 2900 ms: still open
    assert_eq!(
        s.planner().plan().unwrap().commit_state,
        PlanCommitState::Open
    );
    s.tick(fix_speed(4_000, 222)); // 3000 ms: drive-start reached
    assert_eq!(
        s.planner().plan().unwrap().commit_state,
        PlanCommitState::ClosedDriving
    );
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: "r1".into(),
            expected_route_key: None,
            bundle: min_bundle_for("t1", 3, "jp-2026.11.0"),
        })
        .unwrap_err()
        .code,
        ErrorCode::ForbiddenWhileDriving
    );
    assert!(s.route().is_none());

    // 3) Mid-trip replan while stopped, then 3.0 s driving -> FORBIDDEN_WHILE_DRIVING,
    //    while issued commits (reroute / avoid / undo) still succeed while driving
    let mut s = Session::new(SessionConfig::default());
    s.plan_candidates(0, Some(600), &snapshot(1, 0));
    s.tick(finished_for(100, "r1", 3, "f1"));
    let k1 = s
        .route_commit(&RouteCommitRequest {
            req_id: "r1".into(),
            expected_route_key: None,
            bundle: min_bundle_for("t1", 3, "jp-2026.11.0"),
        })
        .unwrap()
        .route_key;
    let mut p2 = snapshot(1, 0);
    p2.plan_request_id = "p2".into();
    p2.trip_revision = U64Str(4);
    let o = s.plan_candidates(200, Some(600), &p2);
    let r2 = starts(&o.planner_cmds)[0].0.clone();
    s.tick(finished_for(300, &r2, 4, "f2"));
    s.tick(fix_speed(1_000, 500));
    s.tick(fix_speed(2_500, 500));
    s.tick(fix_speed(4_000, 500)); // 3000 ms continuous -> ClosedDriving
    assert_eq!(
        s.route_commit(&RouteCommitRequest {
            req_id: r2,
            expected_route_key: Some(k1.clone()),
            bundle: min_bundle_for("t1", 4, "jp-2026.11.0"),
        })
        .unwrap_err()
        .code,
        ErrorCode::ForbiddenWhileDriving
    );
    assert_eq!(s.route().unwrap().route_key, k1, "nothing replaced");
    s.issue_commit_for_test("rr-drive", k1.clone());
    let k2 = s
        .route_commit(&RouteCommitRequest {
            req_id: "rr-drive".into(),
            expected_route_key: Some(k1),
            bundle: min_bundle_for("t1", 3, "jp-2026.11.0"),
        })
        .unwrap()
        .route_key;
    assert_eq!(k2.route_revision, U64Str(2));
}

/// Dropping below 8 km/h (e.g. 7 km/h = 194 cm/s or 221 cm/s), a fix without speed, or a
/// low-accuracy fix (`acc_cm > max_acc_cm`) breaks the continuous drive-start run so the
/// 3.0 s timer restarts from scratch.
#[test]
fn drive_start_hold_resets_on_slow_missing_speed_or_low_accuracy_fix() {
    let cases: [(&str, TickInput); 4] = [
        ("7 km/h (194 cm/s)", fix_speed(3_000, 194)),
        ("just below 222 cm/s (221)", fix_speed(3_000, 221)),
        ("missing speed", fix_full(3_000, None, 300)),
        (
            "low accuracy (acc_cm > max_acc_cm)",
            fix_full(3_000, Some(500), 3_001),
        ),
    ];
    for (label, breaker) in cases {
        let mut s = Session::new(SessionConfig::default());
        s.plan_candidates(0, Some(600), &snapshot(1, 0));
        s.tick(finished_for(100, "r1", 3, "f1"));
        // 2.0 s of fast driving, then interrupted at t = 3_000
        s.tick(fix_speed(1_000, 222));
        s.tick(fix_speed(2_500, 222));
        s.tick(breaker);
        // Restart fast run at t = 4_000: at t = 6_900 (2.9 s) must STILL be Open
        s.tick(fix_speed(4_000, 222));
        s.tick(fix_speed(5_500, 222));
        s.tick(fix_speed(6_900, 222));
        assert_eq!(
            s.planner().plan().unwrap().commit_state,
            PlanCommitState::Open,
            "{label}: 2.9 s after reset should still be Open"
        );
        // At t = 7_000 (3.0 s continuous after reset) -> ClosedDriving -> FORBIDDEN_WHILE_DRIVING
        s.tick(fix_speed(7_000, 222));
        assert_eq!(
            s.planner().plan().unwrap().commit_state,
            PlanCommitState::ClosedDriving,
            "{label}: 3.0 s after reset should be ClosedDriving"
        );
        assert_eq!(
            s.route_commit(&RouteCommitRequest {
                req_id: "r1".into(),
                expected_route_key: None,
                bundle: min_bundle_for("t1", 3, "jp-2026.11.0"),
            })
            .unwrap_err()
            .code,
            ErrorCode::ForbiddenWhileDriving,
            "{label}"
        );
    }

    // SessionConfig overrides drive_start_speed_cm_s / drive_start_hold_ms via JSON
    let mut s =
        Session::from_json(br#"{"drive_start_speed_cm_s": 300, "drive_start_hold_ms": 1000}"#)
            .unwrap();
    s.plan_candidates(0, Some(600), &snapshot(1, 0));
    s.tick(finished_for(100, "r1", 3, "f1"));
    s.tick(fix_speed(1_000, 299));
    s.tick(fix_speed(5_000, 299));
    assert_eq!(
        s.planner().plan().unwrap().commit_state,
        PlanCommitState::Open
    );
    s.tick(fix_speed(6_000, 300));
    s.tick(fix_speed(6_999, 300));
    assert_eq!(
        s.planner().plan().unwrap().commit_state,
        PlanCommitState::Open
    );
    s.tick(fix_speed(7_000, 300));
    assert_eq!(
        s.planner().plan().unwrap().commit_state,
        PlanCommitState::ClosedDriving
    );
}
