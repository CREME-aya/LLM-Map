//! Deterministic replay of a plan's candidate searches: `Session::plan_candidates` →
//! one `start` at a time → `rank` (all_done | budget_expired). Team decision 2026-11-25.
use contracts::plan::{CandidateLabel, CostingVariant, TripPlanSnapshot};
use contracts::tick::{PlannerCmd, RankReason, SearchPriority, TickInput, TickOutput};
use contracts::RouteKey;
use drive_core::candidates::candidate_requests;
use drive_core::planner::Slot;
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
