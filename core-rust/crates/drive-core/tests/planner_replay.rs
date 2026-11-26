//! Deterministic replay of the Gate 7 common test data (core-rust/tests/fixtures/tick).
use contracts::route::{RouteBundle, RouteCommitRequest};
use contracts::tick::{Fix, PlannerCmd, SearchPriority, TickInput, TickOutput};
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

/// tests/fixtures/route/straight-3km.json with `route_key` and a total travel time of
/// `eta_ms` (edge times rescaled). Stands in for the C++ route body.
fn straight_bundle(route_key: &RouteKey, eta_ms: u32) -> RouteBundle {
    let p = format!(
        "{}/../../tests/fixtures/route/straight-3km.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    let mut b: RouteBundle = serde_json::from_value(v["bundle"].clone()).unwrap();
    let len = u64::from(b.length_cm);
    b.route_key = route_key.clone();
    b.eta_ms = eta_ms;
    for leg in &mut b.legs {
        let start = leg.along_route_start_cm;
        let mut along = Vec::new();
        for c in &leg.chunks {
            along.extend_from_slice(&c.seg_along_cm);
        }
        for e in &mut leg.edges {
            let end = u64::from(start + along[e.to_pt as usize]);
            e.cum_time_ms = (u64::from(eta_ms) * end / len) as u32;
        }
        for m in &mut leg.maneuvers {
            m.id.route_key = route_key.clone();
        }
    }
    b
}

/// A fix on the start point of `b` (matched at progress 0 → remaining ETA = eta).
fn start_fix(b: &RouteBundle, at_mono_ms: u64) -> Fix {
    let o = b.legs[0].chunks[0].origin_e7;
    Fix {
        at_mono_ms,
        lat_e7: o.lat,
        lon_e7: o.lon,
        acc_cm: 500,
        bearing_cdeg: None,
        speed_cm_s: None,
    }
}

/// Commits `b` (first route or replacing the current one).
fn commit(s: &mut Session, b: &RouteBundle) {
    let guard = s.route().map(|r| r.route_key.clone());
    if let Some(k) = &guard {
        s.issue_commit_for_test("t-commit", k.clone());
    }
    s.route_commit(&RouteCommitRequest {
        req_id: guard.as_ref().map(|_| "t-commit".to_owned()),
        expected_route_key: guard,
        bundle: b.clone(),
    })
    .unwrap();
}

/// Tick with only a fix.
fn tick_fix(s: &mut Session, now: u64, fix: Fix) -> TickOutput {
    s.tick(TickInput {
        now_mono_ms: now,
        fix: Some(fix),
        events: vec![],
    })
}

fn fixture_route_key(name: &str) -> RouteKey {
    serde_json::from_value(fixture(name)["route_key"].clone()).unwrap()
}

/// Runs a fixture and returns (outputs per step, max concurrently running searches).
fn run(name: &str) -> (Session, Vec<TickOutput>, usize) {
    let (s, o, m, _) = run_with_wakes(name);
    (s, o, m)
}

/// Same, plus the planner's own wake time after each step (watchdog / comparison budget,
/// without the alignment freshness limit that `next_wake_mono_ms` also includes).
fn run_with_wakes(name: &str) -> (Session, Vec<TickOutput>, usize, Vec<Option<u64>>) {
    let fx = fixture(name);
    let cfg: SessionConfig = match fx.get("config") {
        Some(c) => serde_json::from_value(c.clone()).unwrap(),
        None => SessionConfig::default(),
    };
    let mut s = Session::new(cfg);
    let mut wakes = Vec::new();
    // Fixtures with their own `route_commit` (bundle) steps start without a route.
    let has_bundle = fx["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|st| st.get("bundle").is_some());
    if !has_bundle {
        let rk: RouteKey = serde_json::from_value(fx["route_key"].clone()).unwrap();
        s.set_current_route_key(rk);
    }
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
            // The current route is replaced (stands in for another dc_route_commit).
            "set_route" => {
                s.set_current_route_key(serde_json::from_value(step["route_key"].clone()).unwrap());
                TickOutput::default()
            }
            // Kotlin's dc_route_commit: with a `bundle` the full commit, otherwise only the
            // route_key re-check of a `commit`.
            "route_commit" => {
                let want = step["expect_error"].as_str();
                // A request serde rejects (e.g. bundle route_revision != "0") is E_SCHEMA,
                // as dc_route_commit reports it.
                let parsed = serde_json::from_value::<RouteCommitRequest>(serde_json::json!({
                    "req_id": step["req_id"], "expected_route_key": step["expected_route_key"],
                    "bundle": step["bundle"]}));
                let got = if let (Some(_), Err(_)) = (step.get("bundle"), &parsed) {
                    Some("E_SCHEMA")
                } else if let (Some(_), Ok(req)) = (step.get("bundle"), &parsed) {
                    match s.route_commit(req) {
                        Ok(r) => {
                            // the key Rust assigned (rev current + 1), when the fixture says
                            if let Some(k) = step.get("expect_route_key") {
                                let k: RouteKey = serde_json::from_value(k.clone()).unwrap();
                                assert_eq!(r.route_key, k, "{name}: step {step}");
                                assert_eq!(s.route().unwrap().route_key, k);
                                assert!(s
                                    .route()
                                    .unwrap()
                                    .maneuvers
                                    .iter()
                                    .all(|m| m.id.route_key == k));
                            }
                            None
                        }
                        Err(e) => Some(e.code.as_str()),
                    }
                } else {
                    let key: RouteKey =
                        serde_json::from_value(step["expected_route_key"].clone()).unwrap();
                    s.check_route_commit(&key).err().map(|e| e.code.as_str())
                };
                assert_eq!(got, want, "{name}: step {step}");
                TickOutput::default()
            }
            // Stands in for a `commit{req_id, route_key}` the fixture does not drive.
            "issue_commit" => {
                s.issue_commit_for_test(
                    step["req_id"].as_str().unwrap(),
                    serde_json::from_value(step["route_key"].clone()).unwrap(),
                );
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
                _ => None,
            })
            .collect();
        let want = step["expect_starts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(got, want, "{name}: step {step}");
        // Full planner_cmds of the step when the fixture lists them (`expect_cmds`).
        if let Some(w) = step.get("expect_watchdog") {
            assert_eq!(
                s.planner().watchdog_deadline(),
                w.as_u64(),
                "{name}: step {step}"
            );
        }
        if let Some(cmds) = step.get("expect_cmds") {
            let want: Vec<PlannerCmd> = serde_json::from_value(cmds.clone()).unwrap();
            assert_eq!(out.planner_cmds, want, "{name}: step {step}");
        }
        let planner_wake = s.planner().next_wake();
        if let (Some(w), Some(p)) = (out.next_wake_mono_ms, planner_wake) {
            assert!(w <= p, "{name}: next_wake includes the planner deadline");
        }
        wakes.push(planner_wake);
        outs.push(out);
    }
    (s, outs, max_running, wakes)
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
    // t1: cancel r1, no start yet. "経路を確認中" is horizon.status (off_route /
    // rerouting), never a UiCmd (show_checking_route is abolished).
    assert_eq!(
        o[1].planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    assert!(o[1].ui_cmds.is_empty());
    // t2: r1 ok after cancel → discarded, slot freed → start r2
    assert!(whats(&o[2]).contains(&"discarded_late"));
    assert!(
        !o[2]
            .planner_cmds
            .iter()
            .any(|c| matches!(c, PlannerCmd::Commit { .. })),
        "no r1-derived commit"
    );
    assert!(
        matches!(&o[2].planner_cmds[..], [PlannerCmd::Start { req_id, priority: SearchPriority::Reroute, .. }] if req_id == "r2")
    );
    // t3: r2 (reroute) ok with the current route_key → commit with the start-time key
    assert_eq!(
        o[3].planner_cmds,
        vec![PlannerCmd::Commit {
            req_id: "r2".into(),
            route_key: fixture_route_key("gate7-cancel-then-reroute.json"),
            undo_req_id: None,
        }]
    );
    assert!(o[3].ui_cmds.is_empty());
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
    assert!(o[2].ui_cmds.is_empty(), "no UiCmd for engine_stuck");
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
            .planner_cmds
            .iter()
            .any(|c| matches!(c, PlannerCmd::Commit { .. })),
        "no r1-derived commit"
    );
    assert!(
        matches!(&o[4].planner_cmds[..], [PlannerCmd::Start { req_id, priority: SearchPriority::Reroute, .. }] if req_id == "r2")
    );
    assert_eq!(o[4].next_wake_mono_ms, Some(14_900 + 10_000));
    // r2 (reroute) ok with the current route_key → commit with the start-time key
    assert_eq!(
        o[5].planner_cmds,
        vec![PlannerCmd::Commit {
            req_id: "r2".into(),
            route_key: fixture_route_key("gate7-timeout-engine-stuck.json"),
            undo_req_id: None,
        }]
    );
    assert!(o[5].ui_cmds.is_empty());
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
    // Steps 1 / 2 commit the route (eta 29:59) and match its start; `w` = planner wake
    // (watchdog / comparison budget) after each step.
    let (s, o, max_running, w) = run_with_wakes("gate7-short-engine-stuck.json");
    assert_eq!(max_running, 1);
    // plan at 0 with ETA estimate 29:59 → short; comparison budget 0 + 3000
    assert!(o[0].log.iter().any(|l| l.what == "plan_started"
        && l.detail
            .as_deref()
            .unwrap()
            .ends_with("distance_class=short")));
    assert_eq!(w[0], Some(3_000));
    // r1 started at 1000: watchdog 1000 + 5000, budget 3000 comes first
    assert_eq!(w[3], Some(3_000));
    assert!(whats(&o[5]).contains(&"comparison_budget_expired"));
    assert_eq!(w[5], Some(6_000));
    // 5999: not yet stuck
    assert!(!whats(&o[6]).contains(&"engine_stuck"));
    // 6000: watchdog limit reached → engine_stuck, slot still occupied, no UiCmd
    assert!(whats(&o[7]).contains(&"engine_stuck"));
    assert!(o[7].ui_cmds.is_empty());
    assert_eq!(w[7], None);
    // reroute requested while stuck: one cancel, no start (slot occupied)
    assert_eq!(
        o[8].planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    // much later (past both limits, engine_reset seen): still no start, no wake request
    assert!(o[9].planner_cmds.is_empty());
    assert_eq!(w[9], None);
    // r1 returns → discarded_after_stuck, r2 starts short (remaining ETA 29:59 still valid)
    assert!(whats(&o[10]).contains(&"discarded_after_stuck"));
    assert!(
        matches!(&o[10].planner_cmds[..], [PlannerCmd::Start { req_id, priority: SearchPriority::Reroute, .. }] if req_id == "r2")
    );
    assert_eq!(w[10], Some(30_000 + 5_000));
    assert!(matches!(s.planner().slot(), Slot::Running { .. }));
}

fn test_rk() -> RouteKey {
    RouteKey {
        trip_id: "t".into(),
        trip_revision: contracts::U64Str(1),
        route_revision: contracts::U64Str(1),
        graph_version: "g".into(),
    }
}

fn session_with_route() -> Session {
    let mut s = Session::new(SessionConfig::default());
    s.set_current_route_key(test_rk());
    s
}

/// `test_rk()` as a bundle carries it: route_revision "0" (Rust assigns 1 on the first
/// commit, so the committed route is `test_rk()`).
fn bundle_rk() -> RouteKey {
    RouteKey {
        route_revision: contracts::U64Str(0),
        ..test_rk()
    }
}

/// A committed route of `test_rk()` whose remaining ETA at `now = 0` is `eta_s` (matched
/// at its start), or unknown (`None`: committed, no fix yet → uncertain).
fn session_tracking(eta_s: Option<u64>) -> Session {
    let mut s = Session::new(SessionConfig::default());
    let b = straight_bundle(&bundle_rk(), (eta_s.unwrap_or(600) * 1000) as u32);
    commit(&mut s, &b);
    assert_eq!(s.route().unwrap().route_key, test_rk());
    if eta_s.is_some() {
        tick_fix(&mut s, 0, start_fix(&b, 0));
    }
    assert_eq!(s.remaining_eta_ms(0).map(|ms| u64::from(ms) / 1000), eta_s);
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
        // search outside the plan: watchdog from the remaining ETA of the alignment
        let mut s = session_tracking(eta);
        s.begin_plan(0, "p", eta); // plan: comparison budget
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
    let mut s = session_tracking(Some(600));
    assert_eq!(s.begin_plan(0, "p1", Some(600)).0, DistanceClass::Short);
    // a later (longer) estimate for the same plan does not reclassify it nor restart the budget
    let (c, o) = s.begin_plan(500, "p1", Some(7_200));
    assert_eq!(c, DistanceClass::Short);
    assert!(!whats(&o).contains(&"plan_started"));
    assert_eq!(s.planner().comparison_deadline(), Some(3_000));
    tick_fix(
        &mut s,
        600,
        start_fix(&straight_bundle(&test_rk(), 600_000), 600),
    );
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
    assert!(o.ui_cmds.is_empty(), "no show_checking_route");
}

/// Gate 7: distance class of searches outside a plan (team decision 2026-11-25), fixed at
/// each start from the valid remaining ETA on the current route; the plan's class is not
/// inherited. See tests/fixtures/tick/gate7-adhoc-distance-class.json.
#[test]
fn adhoc_distance_class_from_remaining_eta_at_start() {
    let (s, o, max_running) = run("gate7-adhoc-distance-class.json");
    assert_eq!(max_running, 1);
    // r1 (29:59 → short) / r2 (30:00 → long): checked per step (expect_starts /
    // expect_watchdog 1000 + 5000 and 3000 + 10000).
    // plan p1 is short, but off-route (3 fixes 1 km away) invalidates the remaining ETA and
    // the alignment itself starts the reroute: long (watchdog 10 s, no comparison budget;
    // next wake = min(watchdog 6000 + 10000, plan p1 budget 5000 + 3000)).
    let last = o.last().unwrap();
    assert_eq!(last.next_wake_mono_ms, Some(8_000));
    let started = last
        .log
        .iter()
        .find(|l| l.what == "search_started")
        .unwrap();
    assert_eq!(started.detail.as_deref(), Some("distance_class=long"));
    assert!(whats(last).contains(&"off_route_confirmed"));
    match &last.planner_cmds[..] {
        [PlannerCmd::Start {
            fix: Some(f),
            priority: SearchPriority::Reroute,
            ..
        }] => assert_eq!(f.at_mono_ms, 6_000, "the confirming fix is the new origin"),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.planner().watchdog_deadline(), Some(6_000 + 10_000));
    assert_eq!(s.remaining_eta_ms(6_000), None);
}

#[test]
fn adhoc_class_is_fixed_at_start_not_at_request() {
    use contracts::tick::DistanceClass;
    let mut s = session_tracking(Some(600));
    s.request_search(0, SearchPriority::AvoidChip).unwrap(); // r1 short
    s.request_search(10, SearchPriority::Reroute).unwrap(); // cancel r1, r2 waits
                                                            // the fix of t=0 goes stale (2.5 s) before r2 starts → remaining ETA invalidated
    let o = s.tick(
        serde_json::from_value(serde_json::json!({"now_mono_ms":2_501,"events":[
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
    let b = straight_bundle(&test_rk(), 600_000);
    tick_fix(&mut s, 3_000, start_fix(&b, 3_000));
    assert_eq!(s.remaining_eta_ms(3_000), Some(600_000));
    assert_eq!(s.planner().watchdog_deadline(), Some(100 + 10_000));
}

#[test]
fn remaining_eta_of_another_route_is_not_used() {
    use contracts::tick::DistanceClass;
    let mut s = session_tracking(Some(600));
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

/// Gate 7: commit is issued with the start-time route_key; when the route is replaced
/// before dc_route_commit, the late route is refused (E_STALE) and never shown.
#[test]
fn commit_then_route_replaced_is_stale_and_not_shown() {
    let (s, o, max_running) = run("gate7-commit-then-route-replaced.json");
    assert_eq!(max_running, 1);
    assert!(whats(&o[1]).contains(&"commit_issued"));
    // the replacement route stays current; the late route did not replace it
    assert!(s
        .check_route_commit(&fixture_route_key("gate7-commit-then-route-replaced.json"))
        .is_err());
    // the tick after the refused commit: no horizon, no voice from the late route
    assert_eq!(o[4].horizon, None);
    assert!(o[4].voice_cmds.is_empty());
    assert!(o[4].ui_cmds.is_empty());
    // avoid chip ok: no commit, no display
    assert!(whats(&o[6]).contains(&"avoid_result_ready"));
    assert!(o[6].ui_cmds.is_empty());
}

#[test]
fn commit_only_for_reroute_and_adhoc_explicit_plan() {
    use SearchPriority::*;
    let rk = || RouteKey {
        trip_id: "t".into(),
        trip_revision: contracts::U64Str(1),
        route_revision: contracts::U64Str(1),
        graph_version: "g".into(),
    };
    for (priority, commits) in [
        (Reroute, true),
        (ExplicitPlan, true),
        (AvoidChip, false),
        (Comparison, false),
    ] {
        let mut s = session_with_route();
        s.request_search(0, priority).unwrap();
        let o = s.tick(
            serde_json::from_value(serde_json::json!({"now_mono_ms":10,"events":[
            {"at_mono_ms":10,"kind":"search_finished","req_id":"r1","route_key":rk(),
             "graph_version":"g","outcome":"ok","facts_ref":"f1","engine_ms":10}]}))
            .unwrap(),
        );
        let want = if commits {
            vec![PlannerCmd::Commit {
                req_id: "r1".into(),
                route_key: rk(),
                undo_req_id: None,
            }]
        } else {
            vec![]
        };
        assert_eq!(o.planner_cmds, want, "{priority:?}");
        assert!(o.ui_cmds.is_empty(), "{priority:?}");
        // the guard accepts the start-time key while the route is unchanged
        assert!(s.check_route_commit(&rk()).is_ok());
    }
}

#[test]
fn no_commit_for_error_cancelled_or_stale() {
    let rk = |rev: u64| RouteKey {
        trip_id: "t".into(),
        trip_revision: contracts::U64Str(1),
        route_revision: contracts::U64Str(rev),
        graph_version: "g".into(),
    };
    let fin = |outcome: &str, rev: u64| -> TickInput {
        serde_json::from_value(serde_json::json!({"now_mono_ms":10,"events":[
            {"at_mono_ms":10,"kind":"search_finished","req_id":"r1","route_key":rk(rev),
             "graph_version":"g","outcome":outcome,"engine_ms":10}]}))
        .unwrap()
    };
    // error
    let mut s = session_with_route();
    s.request_search(0, SearchPriority::Reroute).unwrap();
    assert!(s.tick(fin("error", 1)).planner_cmds.is_empty());
    // ok with another route_key than the one the search started with
    let mut s = session_with_route();
    s.request_search(0, SearchPriority::Reroute).unwrap();
    assert!(s.tick(fin("ok", 2)).planner_cmds.is_empty());
    // the current route changed while the search ran
    let mut s = session_with_route();
    s.request_search(0, SearchPriority::Reroute).unwrap();
    s.set_current_route_key(rk(2));
    assert!(s.tick(fin("ok", 1)).planner_cmds.is_empty());
}

fn status_of(s: &Session, now: u64) -> Option<contracts::horizon::HorizonStatus> {
    s.horizon_status(now)
}

/// Gate 7 / 04 §3: a reroute ok returned while uncertain (ambiguous parallel carriageways)
/// is held and committed once tracking, with the start-time route_key; Kotlin's
/// dc_route_commit (bundle rev "0") then succeeds and Rust assigns rev 2. See
/// tests/fixtures/tick/gate7-reroute-held-uncertain.json.
#[test]
fn reroute_held_while_uncertain_commits_on_tracking() {
    use contracts::horizon::HorizonStatus;
    let (s, o, max_running) = run("gate7-reroute-held-uncertain.json");
    assert_eq!(max_running, 1);
    // [0] commit A, [1] tracking, [2..=4] off → r1, [5] ambiguous, [6] r1 ok, [7] wait,
    // [8] tracking → commit, [9] route_commit B, [10] tick on B
    assert!(whats(&o[4]).contains(&"off_route_confirmed"));
    assert!(whats(&o[5]).contains(&"align_ambiguous"));
    assert!(whats(&o[6]).contains(&"commit_held_uncertain"));
    assert!(!whats(&o[6]).contains(&"discarded_not_tracking"));
    // the held reroute counts as the reroute: no second start while held
    assert!(o[5..8].iter().all(|x| x.planner_cmds.is_empty()));
    assert!(o[6].next_wake_mono_ms.is_some_and(|w| w <= 16_001));
    let l = o[8].log.iter().find(|l| l.what == "commit_issued").unwrap();
    assert_eq!(l.detail.as_deref(), Some("from_hold"));
    assert_eq!(s.route().unwrap().route_key.route_revision.0, 2);
    assert_eq!(
        o[10].horizon.as_ref().unwrap().route_key.route_revision.0,
        2
    );
    assert_eq!(status_of(&s, 8_000), Some(HorizonStatus::Tracking));
}

/// Gate 7 / 04 §3: the hold ends after commit_hold_ms (10 s): discarded_hold_expired at
/// 16001, nothing committed afterwards. See gate7-reroute-hold-expired.json.
#[test]
fn reroute_hold_expires_after_10s() {
    let (s, o, max_running) = run("gate7-reroute-hold-expired.json");
    assert_eq!(max_running, 1);
    assert!(whats(&o[6]).contains(&"commit_held_uncertain"));
    assert!(!whats(&o[7]).contains(&"discarded_hold_expired"));
    assert!(whats(&o[8]).contains(&"discarded_hold_expired"));
    assert!(s.planner().held().is_none());
    assert!(o[9]
        .planner_cmds
        .iter()
        .all(|c| !matches!(c, PlannerCmd::Commit { .. })));
}

/// Gate 7 (commit versioning, team decision 2026-11-26): bundles carry route_revision "0"
/// (else E_SCHEMA) and Rust assigns current + 1 (first route: 1); a bundle trip_revision /
/// graph_version differing from the current route, and guard mismatches (route_key, or a
/// req_id Rust did not issue / already settled), are E_STALE.
/// See gate7-commit-route-revision.json.
#[test]
fn commit_assigns_route_revision_current_plus_one() {
    let (s, o, _) = run("gate7-commit-route-revision.json");
    assert_eq!(s.route().unwrap().route_key.route_revision.0, 2);
    // the tick right after the refused commits still shows rev 1
    assert_eq!(
        o[10].horizon.as_ref().unwrap().route_key.route_revision.0,
        1
    );
    let h = o[13].horizon.as_ref().unwrap();
    assert_eq!(h.route_key.route_revision.0, 2);
    assert!(h
        .upcoming
        .iter()
        .all(|e| e.maneuver.id.route_key == h.route_key));
}
