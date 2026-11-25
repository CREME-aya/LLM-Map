//! Deterministic replay of the Gate 7 common test data (core-rust/tests/fixtures/tick).
use contracts::tick::{PlannerCmd, SearchPriority, TickInput, TickOutput, UiCmd};
use contracts::RouteKey;
use drive_core::planner::Slot;
use drive_core::{Session, SessionConfig};
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let p = format!(
        "{}/../../tests/fixtures/tick/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap()
}

/// Runs a fixture and returns (outputs per step, max concurrently running searches).
fn run(name: &str) -> (Session, Vec<TickOutput>, usize) {
    let fx = fixture(name);
    let mut s = Session::new(SessionConfig::default());
    let rk: RouteKey = serde_json::from_value(fx["route_key"].clone()).unwrap();
    s.set_current_route_key(rk);
    let mut outs = Vec::new();
    let mut running = 0usize;
    let mut max_running = 0usize;
    for step in fx["steps"].as_array().unwrap() {
        let out = match step["step"].as_str().unwrap() {
            "request" => {
                let pr: SearchPriority = serde_json::from_value(step["priority"].clone()).unwrap();
                s.request_search(step["now_mono_ms"].as_u64().unwrap(), pr)
                    .unwrap()
            }
            "plan" => {
                s.begin_plan(
                    step["now_mono_ms"].as_u64().unwrap(),
                    step["plan_request_id"].as_str().unwrap(),
                    step["eta_estimate_s"].as_u64(),
                )
                .1
            }
            // Remaining ETA on the current route (null = unknown / invalidated by off-route).
            "route_eta" => {
                s.set_remaining_eta(step["remaining_eta_s"].as_u64());
                TickOutput::default()
            }
            "tick" => s.tick(serde_json::from_value::<TickInput>(step["input"].clone()).unwrap()),
            o => panic!("{o}"),
        };
        // Count physically running searches: a start without a matching terminal event.
        let finished = step["input"]["events"].as_array().map_or(0, |e| {
            e.iter().filter(|e| e["kind"] == "search_finished").count()
        });
        running -= finished.min(running);
        running += out
            .planner_cmds
            .iter()
            .filter(|c| matches!(c, PlannerCmd::Start { .. }))
            .count();
        max_running = max_running.max(running);
        // Every start carries the class the planner fixed (fixture `expect_starts`).
        let got: Vec<Value> = out
            .planner_cmds
            .iter()
            .filter_map(|c| match c {
                PlannerCmd::Start {
                    req_id,
                    priority,
                    distance_class,
                    ..
                } => Some(serde_json::json!({
                    "req_id": req_id, "priority": priority, "distance_class": distance_class
                })),
                PlannerCmd::Cancel { .. } | PlannerCmd::Rank { .. } => None,
            })
            .collect();
        let want = step["expect_starts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(got, want, "{name}: step {step}");
        outs.push(out);
    }
    (s, outs, max_running)
}

fn whats(o: &TickOutput) -> Vec<&str> {
    o.log.iter().map(|l| l.what.as_str()).collect()
}

#[test]
fn cancel_then_reroute() {
    let (s, o, max_running) = run("gate7-cancel-then-reroute.json");
    assert_eq!(max_running, 1, "at most one search at a time");
    // t0: start r1
    assert!(
        matches!(&o[0].planner_cmds[..], [PlannerCmd::Start { req_id, priority: SearchPriority::AvoidChip, .. }] if req_id == "r1")
    );
    // t1: cancel r1 + "checking route", no start yet
    assert_eq!(
        o[1].planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    assert_eq!(o[1].ui_cmds, vec![UiCmd::ShowCheckingRoute {}]);
    // t2: r1 ok after cancel → discarded, slot freed → start r2
    assert!(whats(&o[2]).contains(&"discarded_late"));
    assert!(
        !o[2]
            .ui_cmds
            .iter()
            .any(|u| matches!(u, UiCmd::SearchResultReady { .. })),
        "no r1-derived display"
    );
    assert!(
        matches!(&o[2].planner_cmds[..], [PlannerCmd::Start { req_id, priority: SearchPriority::Reroute, .. }] if req_id == "r2")
    );
    // t3: r2 adopted
    assert_eq!(
        o[3].ui_cmds,
        vec![UiCmd::SearchResultReady {
            req_id: "r2".into(),
            facts_ref: Some("facts-r2".into())
        }]
    );
    assert_eq!(*s.planner().slot(), Slot::Idle);
    assert_eq!(o[3].next_wake_mono_ms, None);
}

#[test]
fn timeout_engine_stuck_keeps_slot_until_native_returns() {
    let (s, o, max_running) = run("gate7-timeout-engine-stuck.json");
    assert_eq!(max_running, 1);
    // deadline = started_at(1000) + 10_000 → wake requested at 11000
    assert_eq!(o[1].next_wake_mono_ms, Some(11_000));
    // Rust detects the deadline from now_mono_ms (no timeout event from Kotlin)
    assert!(whats(&o[2]).contains(&"engine_stuck"));
    assert_eq!(o[2].ui_cmds, vec![UiCmd::ShowCheckingRoute {}]);
    assert!(o[2].planner_cmds.is_empty());
    assert_eq!(
        o[2].next_wake_mono_ms, None,
        "stuck: only search_finished frees the slot"
    );
    // engine_reset alone never resumes searching
    assert!(whats(&o[3]).contains(&"engine_reset"));
    assert!(o[3].planner_cmds.is_empty());
    // native call physically returns ok → result discarded, slot freed → r2 starts
    assert!(whats(&o[4]).contains(&"discarded_after_stuck"));
    assert!(!whats(&o[4]).contains(&"discarded_late"));
    assert!(
        !o[4]
            .ui_cmds
            .iter()
            .any(|u| matches!(u, UiCmd::SearchResultReady { .. })),
        "no r1-derived display"
    );
    assert!(
        matches!(&o[4].planner_cmds[..], [PlannerCmd::Start { req_id, priority: SearchPriority::Reroute, .. }] if req_id == "r2")
    );
    assert_eq!(o[4].next_wake_mono_ms, Some(14_900 + 10_000));
    // r2 adopted
    assert_eq!(
        o[5].ui_cmds,
        vec![UiCmd::SearchResultReady {
            req_id: "r2".into(),
            facts_ref: Some("facts-r2".into())
        }]
    );
    assert_eq!(*s.planner().slot(), Slot::Idle);
}

#[test]
fn ok_after_stuck_without_cancel_is_discarded_after_stuck() {
    let mut s = Session::new(SessionConfig::default());
    let rk = serde_json::json!({"trip_id":"t","trip_revision":"1","route_revision":"1","graph_version":"g"});
    s.set_current_route_key(serde_json::from_value(rk.clone()).unwrap());
    s.request_search(0, SearchPriority::Reroute).unwrap();
    let o = s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms":9_999})).unwrap());
    assert!(!whats(&o).contains(&"engine_stuck"));
    let o = s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms":10_000})).unwrap());
    assert!(whats(&o).contains(&"engine_stuck"));
    assert!(matches!(
        s.planner().slot(),
        Slot::EngineStuck {
            cancel_sent: false,
            ..
        }
    ));
    let o = s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms":12_000,"events":[
        {"at_mono_ms":11_000,"kind":"search_finished","req_id":"r1","route_key":rk,"graph_version":"g","outcome":"ok","engine_ms":11_000}]})).unwrap());
    assert!(whats(&o).contains(&"discarded_after_stuck"));
    assert!(o.ui_cmds.is_empty());
    assert_eq!(*s.planner().slot(), Slot::Idle);
}

#[test]
fn reroute_while_stuck_sends_one_cancel_and_waits_for_real_return() {
    let mut s = Session::new(SessionConfig::default());
    let rk = serde_json::json!({"trip_id":"t","trip_revision":"1","route_revision":"1","graph_version":"g"});
    s.set_current_route_key(serde_json::from_value(rk.clone()).unwrap());
    s.request_search(0, SearchPriority::AvoidChip).unwrap();
    s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms":10_000})).unwrap());
    let o = s.request_search(10_100, SearchPriority::Reroute).unwrap();
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    // a second reroute request does not repeat the cancel nor start anything
    let o = s.request_search(10_200, SearchPriority::Reroute).unwrap();
    assert!(o.planner_cmds.is_empty());
    let o = s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms":13_000,"events":[
        {"at_mono_ms":12_900,"kind":"search_finished","req_id":"r1","route_key":rk,"graph_version":"g","outcome":"cancelled","engine_ms":12_900}]})).unwrap());
    assert!(
        matches!(&o.planner_cmds[..], [PlannerCmd::Start { req_id, priority: SearchPriority::Reroute, .. }] if req_id == "r2")
    );
}

#[test]
fn replay_is_byte_identical() {
    let a: Vec<Vec<u8>> = run("gate7-cancel-then-reroute.json")
        .1
        .iter()
        .map(|o| serde_json::to_vec(o).unwrap())
        .collect();
    let b: Vec<Vec<u8>> = run("gate7-cancel-then-reroute.json")
        .1
        .iter()
        .map(|o| serde_json::to_vec(o).unwrap())
        .collect();
    assert_eq!(a, b);
}

#[test]
fn ok_for_stale_route_key_is_not_adopted() {
    let mut s = Session::new(SessionConfig::default());
    let rk = |rev: &str| RouteKey {
        trip_id: "t".into(),
        trip_revision: contracts::U64Str(1),
        route_revision: contracts::U64Str(rev.parse().unwrap()),
        graph_version: "g".into(),
    };
    s.set_current_route_key(rk("1"));
    s.request_search(0, SearchPriority::Reroute).unwrap();
    s.set_current_route_key(rk("2")); // route changed meanwhile
    let t: TickInput = serde_json::from_value(serde_json::json!({"now_mono_ms":10,"events":[
        {"at_mono_ms":9,"kind":"search_finished","req_id":"r1","route_key":rk("1"),"graph_version":"g","outcome":"ok","engine_ms":1}]})).unwrap();
    let o = s.tick(t);
    assert!(o.ui_cmds.is_empty());
    assert!(whats(&o).contains(&"discarded_stale_route_key"));
}

#[test]
fn unknown_req_and_lower_priority_do_not_start_second_search() {
    let mut s = Session::new(SessionConfig::default());
    s.set_current_route_key(RouteKey {
        trip_id: "t".into(),
        trip_revision: contracts::U64Str(1),
        route_revision: contracts::U64Str(1),
        graph_version: "g".into(),
    });
    assert_eq!(
        s.request_search(0, SearchPriority::Reroute)
            .unwrap()
            .planner_cmds
            .len(),
        1
    );
    // lower priority while reroute runs: queued, no cancel, no start
    assert!(s
        .request_search(1, SearchPriority::AvoidChip)
        .unwrap()
        .planner_cmds
        .is_empty());
    let o = s.tick(
        serde_json::from_str(
            r#"{"now_mono_ms":5,"events":[{"at_mono_ms":5,"kind":"engine_reset"}]}"#,
        )
        .unwrap(),
    );
    assert!(o.planner_cmds.is_empty());
}

#[test]
fn short_plan_watchdog_5s_keeps_slot_until_search_finished() {
    let (s, o, max_running) = run("gate7-short-engine-stuck.json");
    assert_eq!(max_running, 1);
    // plan at 0 with ETA estimate 29:59 → short; comparison budget 0 + 3000
    assert!(o[0].log.iter().any(|l| l.what == "plan_started"
        && l.detail
            .as_deref()
            .unwrap()
            .ends_with("distance_class=short")));
    assert_eq!(o[0].next_wake_mono_ms, Some(3_000));
    // r1 started at 1000: watchdog 1000 + 5000, budget 3000 comes first
    assert_eq!(o[2].next_wake_mono_ms, Some(3_000));
    assert!(whats(&o[4]).contains(&"comparison_budget_expired"));
    assert_eq!(o[4].next_wake_mono_ms, Some(6_000));
    // 5999: not yet stuck
    assert!(!whats(&o[5]).contains(&"engine_stuck"));
    // 6000: watchdog limit reached → engine_stuck, slot still occupied
    assert!(whats(&o[6]).contains(&"engine_stuck"));
    assert_eq!(o[6].next_wake_mono_ms, None);
    // reroute requested while stuck: one cancel, no start (slot occupied)
    assert_eq!(
        o[7].planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    // much later (past both limits, engine_reset seen): still no start, no wake request
    assert!(o[8].planner_cmds.is_empty());
    assert_eq!(o[8].next_wake_mono_ms, None);
    // r1 returns → discarded_after_stuck, r2 starts short (remaining ETA 29:59 still valid)
    assert!(whats(&o[9]).contains(&"discarded_after_stuck"));
    assert!(
        matches!(&o[9].planner_cmds[..], [PlannerCmd::Start { req_id, priority: SearchPriority::Reroute, .. }] if req_id == "r2")
    );
    assert_eq!(o[9].next_wake_mono_ms, Some(30_000 + 5_000));
    assert!(matches!(s.planner().slot(), Slot::Running { .. }));
}

fn session_with_route() -> Session {
    let mut s = Session::new(SessionConfig::default());
    s.set_current_route_key(RouteKey {
        trip_id: "t".into(),
        trip_revision: contracts::U64Str(1),
        route_revision: contracts::U64Str(1),
        graph_version: "g".into(),
    });
    s
}

#[test]
fn distance_class_boundaries() {
    use contracts::tick::DistanceClass;
    for (eta, want) in [
        (Some(29 * 60 + 59), DistanceClass::Short),
        (Some(30 * 60), DistanceClass::Long),
        (None, DistanceClass::Long),
        (Some(0), DistanceClass::Short),
    ] {
        assert_eq!(DistanceClass::from_eta_estimate_s(eta), want, "{eta:?}");
        let mut s = session_with_route();
        assert_eq!(s.begin_plan(0, "p", eta).0, want, "{eta:?}");
    }
}

#[test]
fn watchdog_limit_follows_distance_class_from_each_start() {
    for (eta, watchdog, budget) in [
        (Some(1_799), 5_000, 3_000),
        (Some(1_800), 10_000, 8_000),
        (None, 10_000, 8_000),
    ] {
        let mut s = session_with_route();
        s.begin_plan(0, "p", eta); // plan: comparison budget
        s.set_remaining_eta(eta); // search outside the plan: watchdog from remaining ETA
        s.request_search(100, SearchPriority::Comparison).unwrap();
        assert_eq!(s.planner().watchdog_deadline(), Some(100 + watchdog));
        assert_eq!(s.planner().comparison_deadline(), Some(budget));
        let o = s.tick(
            serde_json::from_value(serde_json::json!({"now_mono_ms": 100 + watchdog - 1})).unwrap(),
        );
        assert!(!whats(&o).contains(&"engine_stuck"), "{eta:?}");
        let o = s.tick(
            serde_json::from_value(serde_json::json!({"now_mono_ms": 100 + watchdog})).unwrap(),
        );
        assert!(whats(&o).contains(&"engine_stuck"), "{eta:?}");
        assert!(matches!(s.planner().slot(), Slot::EngineStuck { .. }));
    }
}

#[test]
fn distance_class_is_fixed_for_the_same_plan_request() {
    use contracts::tick::DistanceClass;
    let mut s = session_with_route();
    assert_eq!(s.begin_plan(0, "p1", Some(600)).0, DistanceClass::Short);
    // a later (longer) estimate for the same plan does not reclassify it nor restart the budget
    let (c, o) = s.begin_plan(500, "p1", Some(7_200));
    assert_eq!(c, DistanceClass::Short);
    assert!(!whats(&o).contains(&"plan_started"));
    assert_eq!(s.planner().comparison_deadline(), Some(3_000));
    s.set_remaining_eta(Some(600));
    s.request_search(600, SearchPriority::Comparison).unwrap();
    assert_eq!(s.planner().watchdog_deadline(), Some(5_600));
    // a new plan_request is classified anew, but the running search keeps its class
    assert_eq!(s.begin_plan(700, "p2", Some(7_200)).0, DistanceClass::Long);
    assert_eq!(s.planner().watchdog_deadline(), Some(5_600));
    assert_eq!(s.planner().comparison_deadline(), Some(8_700));
}

#[test]
fn comparison_budget_counts_from_plan_start_and_fires_once() {
    let mut s = session_with_route();
    s.begin_plan(1_000, "p", Some(100));
    let o = s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms": 3_999})).unwrap());
    assert_eq!(o.next_wake_mono_ms, Some(4_000));
    assert!(!whats(&o).contains(&"comparison_budget_expired"));
    let o = s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms": 4_000})).unwrap());
    assert!(whats(&o).contains(&"comparison_budget_expired"));
    assert_eq!(o.next_wake_mono_ms, None);
    let o = s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms": 5_000})).unwrap());
    assert!(!whats(&o).contains(&"comparison_budget_expired"));
}

#[test]
fn priority_order_reroute_explicit_plan_avoid_chip_comparison() {
    use SearchPriority::*;
    assert!(Reroute > ExplicitPlan && ExplicitPlan > AvoidChip && AvoidChip > Comparison);
    // explicit_plan preempts avoid_chip (cancel, but no "checking route": not a reroute)
    let mut s = session_with_route();
    s.request_search(0, AvoidChip).unwrap();
    let o = s.request_search(1, ExplicitPlan).unwrap();
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    assert!(o.ui_cmds.is_empty());
    // comparison never preempts anything higher
    let mut s = session_with_route();
    s.request_search(0, AvoidChip).unwrap();
    assert!(s
        .request_search(1, Comparison)
        .unwrap()
        .planner_cmds
        .is_empty());
    let mut s = session_with_route();
    s.request_search(0, ExplicitPlan).unwrap();
    assert!(s
        .request_search(1, AvoidChip)
        .unwrap()
        .planner_cmds
        .is_empty());
    // reroute preempts comparison
    let mut s = session_with_route();
    s.request_search(0, Comparison).unwrap();
    let o = s.request_search(1, Reroute).unwrap();
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    assert_eq!(o.ui_cmds, vec![UiCmd::ShowCheckingRoute {}]);
}

/// Gate 7: distance class of searches outside a plan (team decision 2026-11-25), fixed at
/// each start from the valid remaining ETA on the current route; the plan's class is not
/// inherited. See tests/fixtures/tick/gate7-adhoc-distance-class.json.
#[test]
fn adhoc_distance_class_from_remaining_eta_at_start() {
    let (s, o, max_running) = run("gate7-adhoc-distance-class.json");
    assert_eq!(max_running, 1);
    // r1 (29:59 → short): watchdog 1000 + 5000; r2 (30:00 → long): 3000 + 10000
    assert_eq!(o[1].next_wake_mono_ms, Some(6_000));
    assert_eq!(o[4].next_wake_mono_ms, Some(13_000));
    // plan p1 is short, but the reroute after off-route (ETA invalidated) is long:
    // its watchdog is 10 s, and no comparison budget applies to it
    // (next wake = min(watchdog 6000 + 10000, plan p1 budget 5000 + 3000))
    assert_eq!(o[9].next_wake_mono_ms, Some(8_000));
    let o = o[9].clone();
    let started = o.log.iter().find(|l| l.what == "search_started").unwrap();
    assert_eq!(started.detail.as_deref(), Some("distance_class=long"));
    assert_eq!(s.planner().watchdog_deadline(), Some(6_000 + 10_000));
}

#[test]
fn adhoc_class_is_fixed_at_start_not_at_request() {
    use contracts::tick::DistanceClass;
    let mut s = session_with_route();
    s.set_remaining_eta(Some(600));
    s.request_search(0, SearchPriority::AvoidChip).unwrap(); // r1 short
    s.request_search(10, SearchPriority::Reroute).unwrap(); // cancel r1, r2 waits
    s.set_remaining_eta(None); // off-route before r2 starts
    let o = s.tick(
        serde_json::from_value(serde_json::json!({"now_mono_ms":100,"events":[
        {"at_mono_ms":100,"kind":"search_finished","req_id":"r1",
         "route_key":{"trip_id":"t","trip_revision":"1","route_revision":"1","graph_version":"g"},
         "graph_version":"g","outcome":"cancelled","engine_ms":100}]}))
        .unwrap(),
    );
    assert!(matches!(
        &o.planner_cmds[..],
        [PlannerCmd::Start { req_id, distance_class: DistanceClass::Long, .. }] if req_id == "r2"
    ));
    // the class stays for the whole call even if a short ETA arrives meanwhile
    s.set_remaining_eta(Some(60));
    assert_eq!(s.planner().watchdog_deadline(), Some(100 + 10_000));
}

#[test]
fn remaining_eta_of_another_route_is_not_used() {
    use contracts::tick::DistanceClass;
    let mut s = session_with_route();
    s.set_remaining_eta(Some(600));
    s.set_current_route_key(RouteKey {
        trip_id: "t".into(),
        trip_revision: contracts::U64Str(1),
        route_revision: contracts::U64Str(2),
        graph_version: "g".into(),
    });
    let o = s.request_search(0, SearchPriority::Reroute).unwrap();
    assert!(matches!(
        &o.planner_cmds[..],
        [PlannerCmd::Start {
            distance_class: DistanceClass::Long,
            ..
        }]
    ));
}
