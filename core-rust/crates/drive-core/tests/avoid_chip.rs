//! Driving-time avoid chip (04 §4.3, 03 §3.1 / §3.2; docs f32ae0e): trigger → budget and
//! deadline check → `start{avoid_chip}` → `ok` not committed → `dc_avoid_offer_eval` →
//! `AvoidOffer` on its `HorizonEntry` → consent `user_tap{offer_id, avoid}` → `commit`, or
//! `dismiss{expired}` at the deadline. Gate 7 (07 §2.4) and Gate 4 / 7 (07 §2.5) cases are
//! driven by the real trigger, not by the `request_search` hook.
use contracts::facts::{
    AvoidDeclineReason, AvoidOfferEvalRequest, AvoidOfferEvalResult, CandidateFacts,
};
use contracts::horizon::{AvoidOffer, HorizonStatus, ManeuverHorizon};
use contracts::route::{RouteBundle, RouteCommitRequest};
use contracts::tick::{
    DismissReason, Fix, PlannerCmd, SearchPriority, TickInput, TickOutput, UiCmd,
};
use contracts::RouteKey;
use drive_core::avoid::AvoidState;
use drive_core::{Session, SessionConfig};
use serde_json::{json, Value};

// ---- helpers ---------------------------------------------------------------------------

/// Main line of the Gate 7 fixtures: 4 km east at 15 m/s; left 1.5 km, right 2.5 km (all
/// Evidence unknown → right_signal_unknown, an avoid target), left 3.5 km, arrive 4 km.
fn main_line() -> RouteBundle {
    let p = format!(
        "{}/../../tests/fixtures/route/gate7-side-road-20m.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    serde_json::from_value(v["bundle"].clone()).unwrap()
}

fn fix_xy(b: &RouteBundle, at: u64, x_cm: i64, y_cm: i64) -> Fix {
    let ch = &b.legs[0].chunks[0];
    Fix {
        at_mono_ms: at,
        lat_e7: ch.origin_e7.lat + (y_cm * 65_536 / i64::from(ch.k_lat_q16)) as i32,
        lon_e7: ch.origin_e7.lon + (x_cm * 65_536 / i64::from(ch.k_lon_q16)) as i32,
        acc_cm: 500,
        bearing_cdeg: Some(9_000),
        speed_cm_s: Some(1_500),
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        fix_stale_ms: 1_000_000,
        avoid_chip_lookahead: true,
        ..Default::default()
    }
}

fn session_with(b: &RouteBundle, cfg: SessionConfig) -> Session {
    let mut s = Session::new(cfg);
    s.route_commit(&RouteCommitRequest {
        req_id: None,
        expected_route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    s
}

fn key(s: &Session) -> RouteKey {
    s.route().unwrap().route_key.clone()
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

fn finished(at: u64, req_id: &str, key: &RouteKey, outcome: &str) -> Value {
    json!({
        "at_mono_ms": at, "kind": "search_finished", "req_id": req_id,
        "route_key": key, "graph_version": key.graph_version, "outcome": outcome,
        "facts_ref": "facts-1", "base_facts_ref": "base-1", "engine_ms": 300
    })
}

fn tap(at: u64, target_id: &str, action: &str) -> Value {
    json!({"at_mono_ms": at, "kind": "user_tap", "target_id": target_id, "action": action})
}

fn facts(graph_version: &str, eta_ms: u64) -> CandidateFacts {
    serde_json::from_value(json!({
        "candidate_id": "c-avoid", "label": "through_anchor", "plan_request_id": "drive",
        "graph_version": graph_version, "fingerprint": "fp", "legal": true, "complete": true,
        "eta_ms": eta_ms, "length_m": 4300,
        "counts": {"right_unprotected": 0, "right_signal_unknown_protection": 0,
                   "right_signal_unknown": 0, "right_protected": 0, "right_entry": 0, "merges": 0},
        "narrow_m": {"confirmed": 0, "inferred": 0},
        "unknown_width_residential_m": 0, "unknown_length_m": 0,
        "junctions": [], "through_anchors": []
    }))
    .unwrap()
}

/// `base_facts`: the current route from the search start, with the one target right turn
/// (`facts` = the alternative, none). Same yardstick as the alternative (04 §4.3 (b)).
fn base_facts(graph_version: &str, eta_ms: u64) -> CandidateFacts {
    let mut f = facts(graph_version, eta_ms);
    f.candidate_id = "c-base".into();
    f.counts.right_unprotected = 1;
    f
}

fn starts(o: &TickOutput, p: SearchPriority) -> Vec<String> {
    o.planner_cmds
        .iter()
        .filter_map(|c| match c {
            PlannerCmd::Start {
                req_id, priority, ..
            } if *priority == p => Some(req_id.clone()),
            _ => None,
        })
        .collect()
}

fn commits(o: &TickOutput) -> Vec<(String, RouteKey)> {
    o.planner_cmds
        .iter()
        .filter_map(|c| match c {
            PlannerCmd::Commit {
                req_id, route_key, ..
            } => Some((req_id.clone(), route_key.clone())),
            _ => None,
        })
        .collect()
}

fn offers(h: &ManeuverHorizon) -> Vec<&AvoidOffer> {
    h.upcoming
        .iter()
        .filter_map(|e| e.avoid_offer.as_ref())
        .collect()
}

fn logged(o: &TickOutput, what: &str) -> bool {
    o.log.iter().any(|l| l.what == what)
}

fn deadline(s: &Session) -> u64 {
    s.avoid_state().target().unwrap().deadline_mono_ms
}

fn eval(s: &mut Session, now: u64, f: CandidateFacts) -> AvoidOfferEvalResult {
    let req_id = match s.avoid_state() {
        AvoidState::Ready { req_id, .. } => req_id.clone(),
        _ => "r1".into(),
    };
    let base = base_facts(&f.graph_version, 0);
    s.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id,
        now_mono_ms: now,
        alt_facts: f,
        base_facts: base,
    })
}

/// Tracking at 100 m: the right turn at 2.5 km (index 1, within the 3 ahead) starts r1.
/// Returns the session, the first frame and the base ETA (remaining ETA at the start).
fn started(cfg: SessionConfig) -> (Session, RouteBundle, ManeuverHorizon) {
    let b = main_line();
    let mut s = session_with(&b, cfg);
    let o = tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)), vec![]);
    assert_eq!(starts(&o, SearchPriority::AvoidChip), vec!["r1".to_owned()]);
    (s, b, o.horizon.unwrap())
}

/// r1 started and returned ok, then evaluated with an alternative 60 s slower.
fn offered() -> (Session, RouteBundle, AvoidOffer) {
    let (mut s, b, _) = started(config());
    let k = key(&s);
    tick(&mut s, 1_500, None, vec![finished(1_400, "r1", &k, "ok")]);
    let base = s.avoid_state().target().unwrap().base_eta_ms;
    let AvoidOfferEvalResult::Offered { avoid_offer } =
        eval(&mut s, 1_600, facts(&k.graph_version, base + 60_000))
    else {
        panic!("not offered");
    };
    (s, b, avoid_offer)
}

// ---- trigger, budget and deadline --------------------------------------------------------

/// Off by default (04 §4.3 item 6: until the stop latency is measured).
#[test]
fn look_ahead_is_off_by_default() {
    let b = main_line();
    let mut s = session_with(
        &b,
        SessionConfig {
            fix_stale_ms: 1_000_000,
            ..Default::default()
        },
    );
    let o = tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)), vec![]);
    assert_eq!(o.horizon.as_ref().unwrap().status, HorizonStatus::Tracking);
    assert!(o.planner_cmds.is_empty());
    assert_eq!(*s.avoid_state(), AvoidState::Idle);
}

/// The trigger starts one `avoid_chip` search (no fix, no candidate) for the right turn;
/// its deadline is the branch's `earliest` − 20 s (from the sample), not `typical`.
#[test]
fn trigger_starts_an_avoid_chip_search_with_an_earliest_deadline() {
    let (s, _, h) = started(config());
    let e = h.upcoming[1].eta_range_ms.unwrap();
    assert_eq!(h.upcoming[1].maneuver.id.ordinal, 1);
    assert_eq!(u64::from(e.earliest), u64::from(e.typical) * 850 / 1_000);
    let t = s.avoid_state().target().unwrap();
    assert_eq!(t.maneuver_id, h.upcoming[1].maneuver.id);
    assert_eq!(t.deadline_mono_ms, 1_000 + u64::from(e.earliest) - 20_000);
    assert_eq!(t.avoided.unprotected_right, 1);
    assert_eq!(t.base_eta_ms, u64::from(s.remaining_eta_ms(1_000).unwrap()));
    assert!(matches!(s.avoid_state(), AvoidState::Searching { req_id, .. } if req_id == "r1"));
}

/// "締切＋計算予算 以上手前" (04 §4.3 item 1): `now + budget <= deadline` starts, one ms
/// more does not (and that maneuver is not tried again on the same route).
#[test]
fn start_condition_boundary_budget_and_deadline() {
    let (s, _, _) = started(config());
    let d = deadline(&s);
    let budget = d - 1_000;
    let (s, b, _) = started(SessionConfig {
        avoid_budget_ms: budget,
        ..config()
    });
    assert_eq!(deadline(&s), d);
    let mut s = session_with(
        &b,
        SessionConfig {
            avoid_budget_ms: budget + 1,
            ..config()
        },
    );
    let o = tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)), vec![]);
    assert!(starts(&o, SearchPriority::AvoidChip).is_empty());
    assert!(logged(&o, "avoid_too_close"));
    let o = tick(&mut s, 2_000, Some(fix_xy(&b, 2_000, 11_500, 0)), vec![]);
    assert!(
        starts(&o, SearchPriority::AvoidChip).is_empty(),
        "tried once"
    );
}

/// Too close to the branch for the 20 s deadline: no search.
#[test]
fn no_search_inside_the_deadline() {
    let b = main_line();
    let mut s = session_with(&b, config());
    // 2.5 km − 2.2 km = 300 m: typical 20 s, earliest 17 s < 20 s + 1.5 s
    let o = tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 220_000, 0)), vec![]);
    assert_eq!(o.horizon.as_ref().unwrap().status, HorizonStatus::Tracking);
    assert!(starts(&o, SearchPriority::AvoidChip).is_empty());
}

/// "3 手先（または到達約 120 秒前）": a target beyond the third maneuver starts once it is
/// among the 3 ahead, or earlier once its `earliest` is within 120 s.
#[test]
fn trigger_three_ahead_or_within_120_s() {
    let mut b = main_line();
    // the right turn at 2.5 km → ordinary left; the arrival at 4 km (4th maneuver) enters
    // a confirmed 3.0 m road: the only target
    let m = &mut b.legs[0].maneuvers;
    assert_eq!(m.len(), 4);
    m[1].arrow = contracts::horizon::Arrow::Left;
    m[3].departure_width_mm =
        serde_json::from_value(json!({"kind": "confirmed", "value": 3_000, "source_ref": "t"}))
            .unwrap();
    // 3 ahead: at 100 m the target is 4th, earliest 221 s > 120 s → nothing
    let mut s = session_with(&b, config());
    let o = tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)), vec![]);
    assert!(starts(&o, SearchPriority::AvoidChip).is_empty());
    // at 1.75 km: first left not passed (one sample beyond it); 2.25 km → earliest 127.5 s
    let o = tick(&mut s, 41_000, Some(fix_xy(&b, 41_000, 175_000, 0)), vec![]);
    assert_eq!(
        o.horizon.clone().unwrap().upcoming[0].maneuver.id.ordinal,
        0
    );
    assert!(starts(&o, SearchPriority::AvoidChip).is_empty());
    // at 1.85 km: the first left is passed, the target is 3rd (earliest 121.8 s > 120 s)
    let o = tick(&mut s, 42_000, Some(fix_xy(&b, 42_000, 185_000, 0)), vec![]);
    let h = o.horizon.clone().unwrap();
    assert_eq!(h.upcoming[0].maneuver.id.ordinal, 1);
    assert!(h.upcoming[2].eta_range_ms.unwrap().earliest > 120_000);
    assert_eq!(starts(&o, SearchPriority::AvoidChip), vec!["r1".to_owned()]);
    assert_eq!(s.avoid_state().target().unwrap().maneuver_id.ordinal, 3);
    assert_eq!(
        s.avoid_state().target().unwrap().avoided.unprotected_right,
        0
    );
    // 120 s: at 2.1 km with the first left not passed the target is still 4th, but its
    // earliest is 107.7 s
    let mut s = session_with(&b, config());
    tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)), vec![]);
    let o = tick(&mut s, 41_000, Some(fix_xy(&b, 41_000, 210_000, 0)), vec![]);
    assert_eq!(
        o.horizon.clone().unwrap().upcoming[0].maneuver.id.ordinal,
        0
    );
    assert_eq!(starts(&o, SearchPriority::AvoidChip), vec!["r1".to_owned()]);
    assert_eq!(s.avoid_state().target().unwrap().maneuver_id.ordinal, 3);
}

// ---- result, evaluation, offer -----------------------------------------------------------

/// `ok` is not committed; `dc_avoid_offer_eval` builds the offer (docs shape, offer_id
/// included) and the next frame carries it on the right turn's entry.
#[test]
fn ok_is_not_committed_and_the_offer_rides_on_its_entry() {
    let (mut s, b, _) = started(config());
    let k = key(&s);
    let o = tick(&mut s, 1_500, None, vec![finished(1_400, "r1", &k, "ok")]);
    assert!(commits(&o).is_empty(), "no commit before consent");
    assert!(logged(&o, "avoid_result_ready"));
    assert!(matches!(s.avoid_state(), AvoidState::Ready { .. }));
    // Kotlin is told to evaluate it with both CandidateFacts (03 §3.1, docs 2a8a727)
    assert_eq!(
        o.planner_cmds,
        vec![PlannerCmd::EvaluateOffer {
            req_id: "r1".into(),
            facts_ref: "facts-1".into(),
            base_facts_ref: "base-1".into(),
        }]
    );
    let v = serde_json::to_value(&o.planner_cmds[0]).unwrap();
    assert_eq!(
        v,
        json!({"cmd": "evaluate_offer", "req_id": "r1", "facts_ref": "facts-1",
               "base_facts_ref": "base-1"})
    );
    let d = deadline(&s);
    // another req_id is not the waiting result: declined, the result keeps waiting
    let base = s.avoid_state().target().unwrap().base_eta_ms;
    let r = s.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id: "r9".into(),
        now_mono_ms: 1_550,
        alt_facts: facts(&k.graph_version, base),
        base_facts: base_facts(&k.graph_version, base),
    });
    assert_eq!(
        r,
        AvoidOfferEvalResult::Declined {
            reason: AvoidDeclineReason::NoPendingResult
        }
    );
    assert!(matches!(s.avoid_state(), AvoidState::Ready { .. }));
    let base = s.avoid_state().target().unwrap().base_eta_ms;
    let r = eval(&mut s, 1_600, facts(&k.graph_version, base + 60_000));
    let AvoidOfferEvalResult::Offered { avoid_offer: offer } = r.clone() else {
        panic!("{r:?}")
    };
    assert_eq!(offer.offer_id, "avoid-r1");
    assert_eq!(offer.alt_route_id, "r1");
    assert_eq!(offer.delta_eta_ms, 60_000);
    assert_eq!(offer.decision_deadline_monotonic_ms, d);
    assert_eq!(offer.summary.delta_eta_ms, 60_000);
    assert_eq!(offer.summary.avoided.unprotected_right, 1);
    assert_eq!(offer.summary.avoided.narrow_m, 0);
    // JSON: {"result": "offered", "avoid_offer": {...}}
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"], "offered");
    assert_eq!(v["avoid_offer"]["offer_id"], "avoid-r1");
    let back: AvoidOffer = serde_json::from_value(v["avoid_offer"].clone()).unwrap();
    assert_eq!(back, offer);
    let o = tick(&mut s, 1_700, Some(fix_xy(&b, 1_700, 11_000, 0)), vec![]);
    let h = o.horizon.clone().unwrap();
    assert_eq!(h.upcoming[1].avoid_offer.as_ref(), Some(&offer));
    assert_eq!(offers(&h).len(), 1);
    assert!(o.next_wake_mono_ms.unwrap() <= d);
    // a second evaluation has nothing waiting and leaves the offer alone
    let r = eval(&mut s, 1_800, facts(&k.graph_version, base));
    assert_eq!(
        r,
        AvoidOfferEvalResult::Declined {
            reason: AvoidDeclineReason::NoPendingResult
        }
    );
    assert!(s.avoid_state().offer().is_some());
}

/// An `ok` without `facts_ref` or without `base_facts_ref` emits no `evaluate_offer` and
/// makes no offer (04 §4.3 (b) cannot be checked without `base_facts`); logged.
#[test]
fn ok_without_both_refs_makes_no_offer() {
    for (drop, why) in [
        ("base_facts_ref", "no_base_facts_ref"),
        ("facts_ref", "no_facts_ref"),
    ] {
        let (mut s, _, _) = started(config());
        let k = key(&s);
        let mut fin = finished(1_400, "r1", &k, "ok");
        fin.as_object_mut().unwrap().remove(drop);
        let o = tick(&mut s, 1_500, None, vec![fin]);
        assert!(o.planner_cmds.is_empty(), "{drop}: {:?}", o.planner_cmds);
        assert!(
            o.log
                .iter()
                .any(|l| l.what == "avoid_attempt_ended" && l.detail.as_deref() == Some(why)),
            "{drop}"
        );
        assert_eq!(*s.avoid_state(), AvoidState::Idle);
        let r = eval(&mut s, 1_600, facts(&k.graph_version, 0));
        assert_eq!(
            r,
            AvoidOfferEvalResult::Declined {
                reason: AvoidDeclineReason::NoPendingResult
            }
        );
    }
}

/// Declines (03 §3.2 / 03 §3.1): detour cap (base = remaining ETA at the start, short:
/// min(180 s, base × 0.35)), graph version, illegal / incomplete, deadline, not tracking.
#[test]
fn evaluation_declines() {
    let decline = |f: &dyn Fn(&RouteKey, u64) -> CandidateFacts, now: u64| {
        let (mut s, _, _) = started(config());
        let k = key(&s);
        tick(&mut s, 1_500, None, vec![finished(1_400, "r1", &k, "ok")]);
        let base = s.avoid_state().target().unwrap().base_eta_ms;
        let now = if now == 0 { deadline(&s) } else { now };
        let r = eval(&mut s, now, f(&k, base));
        if matches!(r, AvoidOfferEvalResult::Declined { .. }) {
            assert_eq!(*s.avoid_state(), AvoidState::Idle, "result dropped");
        }
        r
    };
    let d = |reason| AvoidOfferEvalResult::Declined { reason };
    // base ≈ 266 s → cap = min(180 s, 93.1 s)
    let cap = |base: u64| (base * 350 / 1_000).min(180_000);
    assert_eq!(
        decline(
            &|k, base| facts(&k.graph_version, base + cap(base) + 1),
            1_600
        ),
        d(AvoidDeclineReason::OverDetourCap)
    );
    assert!(matches!(
        decline(&|k, base| facts(&k.graph_version, base + cap(base)), 1_600),
        AvoidOfferEvalResult::Offered { .. }
    ));
    assert_eq!(
        decline(&|_, base| facts("other-graph", base), 1_600),
        d(AvoidDeclineReason::GraphMismatch)
    );
    assert_eq!(
        decline(
            &|k, base| {
                let mut f = facts(&k.graph_version, base);
                f.complete = false;
                f
            },
            1_600
        ),
        d(AvoidDeclineReason::NotLegalOrComplete)
    );
    // at the deadline (now = deadline): too late
    assert_eq!(
        decline(&|k, base| facts(&k.graph_version, base), 0),
        d(AvoidDeclineReason::DeadlinePassed)
    );
    // nothing waiting
    let (mut s, _, _) = started(config());
    let k = key(&s);
    assert_eq!(
        eval(&mut s, 1_100, facts(&k.graph_version, 0)),
        d(AvoidDeclineReason::NoPendingResult)
    );
    assert!(
        matches!(s.avoid_state(), AvoidState::Searching { .. }),
        "kept"
    );
}

/// An `error` / `cancelled` result ends the attempt; a ready result while not tracking is
/// not used either (03 §3.1).
#[test]
fn unusable_results_end_the_attempt() {
    for outcome in ["error", "cancelled"] {
        let (mut s, _, _) = started(config());
        let k = key(&s);
        let o = tick(
            &mut s,
            1_500,
            None,
            vec![finished(1_400, "r1", &k, outcome)],
        );
        assert!(logged(&o, "avoid_attempt_ended"), "{outcome}");
        assert_eq!(*s.avoid_state(), AvoidState::Idle);
    }
    // result for an older route key: discarded_stale_route_key
    let (mut s, _, _) = started(config());
    let mut k = key(&s);
    k.route_revision = contracts::U64Str(99);
    let o = tick(&mut s, 1_500, None, vec![finished(1_400, "r1", &k, "ok")]);
    assert!(logged(&o, "discarded_stale_route_key"));
    assert_eq!(*s.avoid_state(), AvoidState::Idle);
}

// ---- consent and expiry ------------------------------------------------------------------

/// Consent before the deadline → `commit{r1, route_key at the start}` once; the chip leaves
/// the horizon; `dc_route_commit` with that key takes the alternative (revision + 1).
#[test]
fn consent_before_the_deadline_commits() {
    let (mut s, b, offer) = offered();
    let k = key(&s);
    // other targets / actions do not consent
    let o = tick(&mut s, 1_700, None, vec![tap(1_650, "override-1", "avoid")]);
    assert!(commits(&o).is_empty());
    let o = tick(
        &mut s,
        1_750,
        None,
        vec![tap(1_700, &offer.offer_id, "confirm")],
    );
    assert!(commits(&o).is_empty());
    assert!(logged(&o, "avoid_consent_refused"));
    let o = tick(
        &mut s,
        1_800,
        Some(fix_xy(&b, 1_800, 11_200, 0)),
        vec![tap(1_790, &offer.offer_id, "avoid")],
    );
    assert_eq!(commits(&o), vec![("r1".to_owned(), k.clone())]);
    assert!(offers(o.horizon.as_ref().unwrap()).is_empty());
    assert_eq!(*s.avoid_state(), AvoidState::Idle);
    // a second tap does nothing
    let o = tick(
        &mut s,
        1_900,
        None,
        vec![tap(1_850, &offer.offer_id, "avoid")],
    );
    assert!(commits(&o).is_empty());
    // Kotlin: dc_route_commit(expected_route_key = commit.route_key, alternative body)
    let mut alt = b.clone();
    alt.route_key.route_revision = contracts::U64Str(0);
    let r = s
        .route_commit(&RouteCommitRequest {
            req_id: Some("r1".into()),
            expected_route_key: Some(k.clone()),
            bundle: alt,
        })
        .unwrap();
    assert_eq!(r.route_key.route_revision.0, k.route_revision.0 + 1);
}

/// Gate 4 / 7 "表示後に締切超過" (07 §2.5): at the deadline the chip is removed with
/// `dismiss{offer_id, expired}` and a later tap changes nothing.
#[test]
fn deadline_withdraws_the_offer_and_a_late_tap_is_refused() {
    let (mut s, b, offer) = offered();
    let d = offer.decision_deadline_monotonic_ms;
    let o = tick(&mut s, d - 1, Some(fix_xy(&b, d - 1, 12_000, 0)), vec![]);
    assert_eq!(o.next_wake_mono_ms, Some(d));
    assert!(o.ui_cmds.is_empty());
    let o = tick(&mut s, d, None, vec![]);
    assert_eq!(
        o.ui_cmds,
        vec![UiCmd::Dismiss {
            target_id: offer.offer_id.clone(),
            reason: DismissReason::Expired
        }]
    );
    assert!(offers(o.horizon.as_ref().unwrap()).is_empty());
    let o = tick(
        &mut s,
        d + 10,
        None,
        vec![tap(d + 5, &offer.offer_id, "avoid")],
    );
    assert!(commits(&o).is_empty());
    // a tap stamped at the deadline in the same tick as the expiry is refused too
    let (mut s, _, offer) = offered();
    let d = offer.decision_deadline_monotonic_ms;
    let o = tick(&mut s, d, None, vec![tap(d, &offer.offer_id, "avoid")]);
    assert!(commits(&o).is_empty());
    assert!(logged(&o, "avoid_consent_refused"));
    assert!(matches!(
        &o.ui_cmds[..],
        [UiCmd::Dismiss {
            reason: DismissReason::Expired,
            ..
        }]
    ));
}

/// Gate 4 / 7 "承認直前に逸脱" (07 §2.5): leaving the route removes the chip
/// (`dismiss{route_changed}`) and the tap that follows changes nothing; only the reroute
/// is started.
#[test]
fn deviation_just_before_consent_changes_nothing() {
    let (mut s, b, offer) = offered();
    let mut last = None;
    for i in 0..3 {
        let at = 2_000 + i * 1_000;
        last = Some(tick(
            &mut s,
            at,
            Some(fix_xy(&b, at, 12_000 + i as i64 * 1_500, 5_000)),
            vec![],
        ));
    }
    let o = last.unwrap();
    assert_eq!(starts(&o, SearchPriority::Reroute).len(), 1);
    assert!(o.ui_cmds.contains(&UiCmd::Dismiss {
        target_id: offer.offer_id.clone(),
        reason: DismissReason::RouteChanged
    }));
    let o = tick(
        &mut s,
        4_100,
        None,
        vec![tap(4_050, &offer.offer_id, "avoid")],
    );
    assert!(commits(&o).is_empty());
    assert_eq!(*s.avoid_state(), AvoidState::Idle);
}

// ---- Gate 7 (07 §2.4) --------------------------------------------------------------------

/// Gate 7 common test data with the real trigger: t0 start{r1, avoid_chip} → t1 off-route
/// → cancel{r1} → t2 search_finished{r1, ok} discarded, start{r2, reroute} → t3
/// search_finished{r2, ok} → commit{r2}. Running ≤ 1; nothing from r1 is shown.
#[test]
fn gate7_avoid_chip_cancelled_by_a_reroute() {
    let (mut s, b, _) = started(config());
    let k = key(&s);
    let mut outs = Vec::new();
    // t1: three valid off-route fixes (50 m north)
    for i in 0..3u64 {
        let at = 2_000 + i * 1_000;
        outs.push(tick(
            &mut s,
            at,
            Some(fix_xy(&b, at, 12_000 + i as i64 * 1_500, 5_000)),
            vec![],
        ));
    }
    let t1 = outs.last().unwrap();
    assert_eq!(
        t1.planner_cmds,
        vec![PlannerCmd::Cancel {
            req_id: "r1".into()
        }]
    );
    assert!(
        starts(t1, SearchPriority::Reroute).is_empty(),
        "slot still held by r1"
    );
    // t2: r1 ok after cancel → discarded_late, r2 reroute starts
    let t2 = tick(&mut s, 4_500, None, vec![finished(4_400, "r1", &k, "ok")]);
    assert!(logged(&t2, "discarded_late"));
    assert_eq!(starts(&t2, SearchPriority::Reroute), vec!["r2".to_owned()]);
    assert_eq!(*s.avoid_state(), AvoidState::Idle);
    assert_eq!(
        eval(&mut s, 4_600, facts(&k.graph_version, 1)),
        AvoidOfferEvalResult::Declined {
            reason: AvoidDeclineReason::NoPendingResult
        }
    );
    // t3: r2 ok → commit
    let t3 = tick(&mut s, 5_500, None, vec![finished(5_400, "r2", &k, "ok")]);
    assert_eq!(commits(&t3), vec![("r2".to_owned(), k.clone())]);
    outs.push(t2);
    outs.push(t3);
    for o in &outs {
        assert!(
            o.horizon.as_ref().is_none_or(|h| offers(h).is_empty()),
            "r1 never shown"
        );
        assert!(o.ui_cmds.is_empty());
        assert!(o.voice_cmds.is_empty());
    }
}

/// Gate 7 derived case for the avoid chip: the watchdog passes (short: 5 s) → engine_stuck,
/// status stays tracking; no new avoid chip starts until the real search_finished frees
/// the slot (03 §3.1); its ok is discarded_after_stuck; then the next target starts.
#[test]
fn gate7_stuck_avoid_chip_blocks_new_chips_until_the_slot_frees() {
    let mut b = main_line();
    // second target: the left at 3.5 km enters a confirmed 3.0 m road (narrow)
    b.legs[0].maneuvers[2].departure_width_mm =
        serde_json::from_value(json!({"kind": "confirmed", "value": 3_000, "source_ref": "t"}))
            .unwrap();
    let mut s = session_with(&b, config());
    let o = tick(&mut s, 1_000, Some(fix_xy(&b, 1_000, 10_000, 0)), vec![]);
    assert_eq!(starts(&o, SearchPriority::AvoidChip), vec!["r1".to_owned()]);
    let k = key(&s);
    // remaining ≈ 266 s < 30 min → short → watchdog 5 s from the start
    let o = tick(&mut s, 6_000, Some(fix_xy(&b, 6_000, 17_500, 0)), vec![]);
    assert!(logged(&o, "engine_stuck"));
    assert_eq!(o.horizon.as_ref().unwrap().status, HorizonStatus::Tracking);
    assert!(o.planner_cmds.is_empty(), "no new avoid chip while stuck");
    let o = tick(&mut s, 7_000, Some(fix_xy(&b, 7_000, 19_000, 0)), vec![]);
    assert!(o.planner_cmds.is_empty());
    // the call returns: ok discarded, slot freed → the narrow target starts
    let o = tick(&mut s, 8_000, None, vec![finished(7_900, "r1", &k, "ok")]);
    assert!(logged(&o, "discarded_after_stuck"));
    assert!(commits(&o).is_empty());
    assert_eq!(starts(&o, SearchPriority::AvoidChip), vec!["r2".to_owned()]);
    let t = s.avoid_state().target().unwrap();
    assert_eq!(t.maneuver_id.ordinal, 2);
    assert_eq!(t.avoided.unprotected_right, 0);
}

/// A new route (here: the committed alternative) forgets the old chip and lets the
/// maneuvers of the new route be tried.
#[test]
fn route_commit_resets_the_chip() {
    let (mut s, b, offer) = offered();
    let k = key(&s);
    let mut alt = b.clone();
    alt.route_key.route_revision = contracts::U64Str(0);
    s.issue_commit_for_test("r9", k.clone());
    s.route_commit(&RouteCommitRequest {
        req_id: Some("r9".into()),
        expected_route_key: Some(k),
        bundle: alt,
    })
    .unwrap();
    assert_eq!(*s.avoid_state(), AvoidState::Idle);
    let o = tick(&mut s, 2_000, Some(fix_xy(&b, 2_000, 12_000, 0)), vec![]);
    assert!(o.ui_cmds.contains(&UiCmd::Dismiss {
        target_id: offer.offer_id,
        reason: DismissReason::RouteChanged
    }));
    assert_eq!(
        starts(&o, SearchPriority::AvoidChip).len(),
        1,
        "new route, new attempt"
    );
}
