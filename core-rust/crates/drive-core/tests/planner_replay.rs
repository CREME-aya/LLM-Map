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
    assert_eq!(o[1].ui_cmds, vec![UiCmd::ShowCheckingRoute]);
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
            comparison_ref: Some("cmp-r2".into())
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
    assert_eq!(o[1].next_wake_mono_ms.map(|m| m.0), Some(11_000));
    assert!(whats(&o[2]).contains(&"engine_stuck"));
    assert!(o[2].planner_cmds.is_empty());
    // engine_reset alone never resumes searching
    assert!(whats(&o[3]).contains(&"engine_reset"));
    assert!(o[3].planner_cmds.is_empty());
    // native call physically returns → slot freed → r2 starts
    assert!(matches!(&o[4].planner_cmds[..], [PlannerCmd::Start { req_id, .. }] if req_id == "r2"));
    assert!(matches!(s.planner().slot(), Slot::Running { .. }));
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
    let t: TickInput = serde_json::from_value(serde_json::json!({"now_mono_ms":"10","events":[
        {"at_mono_ms":"9","kind":"search_finished","req_id":"r1","route_key":rk("1"),"graph_version":"g","outcome":"ok","engine_ms":1}]})).unwrap();
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
            r#"{"now_mono_ms":"5","events":[{"at_mono_ms":"5","kind":"engine_reset"}]}"#,
        )
        .unwrap(),
    );
    assert!(o.planner_cmds.is_empty());
}
