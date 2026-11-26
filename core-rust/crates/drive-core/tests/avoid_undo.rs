//! Gate 7: "戻して" of the driving-time avoid chip (03 §3.1, team decision 2026-11-26;
//! docs 0271804 / 0aaeee6). The divergence point is computed once, in the `dc_route_commit`
//! that loads the consented avoid route, from the position when the avoid search started.
//! `user_tap{offer_id, undo}` / `CXL` within 5 s of consent and before the divergence point
//! → `commit{req_id: undo-<req_id>, route_key: current}`; otherwise `NO_EFFECT` /
//! `NO_PENDING_TARGET` and one short `speak{template_id: undo_unavailable}`.
//!
//! Scenario: the original route runs A (edge 1000) → B (1001, 1002) → C (1003..1009), 4 km
//! east. The avoid search starts at 500 m (in 1001); the avoid route starts there, follows
//! B (1001 partial, 1002) and turns to D (2003..) at 1.2 km. While searching the car moves
//! on along B, past the avoid route's first edge (1001 ends at 800 m).
use contracts::facts::{AvoidOfferEvalRequest, AvoidOfferEvalResult, CandidateFacts};
use contracts::horizon::{AvoidOffer, HorizonStatus};
use contracts::override_types::{RejectCode, ValidateOverrideRequest, ValidateOverrideResult};
use contracts::route::{RouteBundle, RouteCommitRequest};
use contracts::tick::{Fix, PlannerCmd, SearchPriority, TickInput, TickOutput, VoiceCmd};
use contracts::{ErrorCode, RouteKey, U64Str};
use drive_core::avoid::{UndoState, UNDO_UNAVAILABLE_TEMPLATE_ID};
use drive_core::{Session, SessionConfig};
use serde_json::{json, Value};

// ---- helpers ---------------------------------------------------------------------------

fn main_line() -> RouteBundle {
    let p = format!(
        "{}/../../tests/fixtures/route/gate7-side-road-20m.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    serde_json::from_value(v["bundle"].clone()).unwrap()
}

/// Shape index of the search start (x = 500 m) and the first D edge index on the original.
const START_PT: u32 = 5;
const START_X: i64 = 50_000;
const FIRST_D_EDGE: usize = 3;

/// The avoid route: the main line cut at shape point 5 (500 m, the search-start position;
/// it starts mid-edge 1001) with every edge from index 3 renamed (D). `route_revision` "0".
fn avoid_line(orig: &RouteBundle) -> RouteBundle {
    let mut b = orig.clone();
    let base = START_X as u32;
    let leg = &mut b.legs[0];
    let ch = &mut leg.chunks[0];
    let n = START_PT as usize;
    ch.points_cm.drain(..n);
    ch.bearing_cdeg.drain(..n);
    ch.seg_along_cm.drain(..n);
    for a in &mut ch.seg_along_cm {
        *a -= base;
    }
    let along = ch.seg_along_cm.clone();
    // time at the start: interpolated in edge 1001 (40001..80000 cm, 26667..53333 ms)
    let t0 = 26_667 + (base - 40_001) * (53_333 - 26_667) / (80_000 - 40_001);
    leg.edges.retain(|e| e.to_pt > START_PT);
    for (i, e) in leg.edges.iter_mut().enumerate() {
        e.from_pt = e.from_pt.saturating_sub(START_PT);
        e.to_pt -= START_PT;
        e.length_cm = along[e.to_pt as usize] - along[e.from_pt as usize];
        e.cum_time_ms -= t0;
        if i + 1 >= FIRST_D_EDGE {
            e.edge_id = U64Str(e.edge_id.0 + 1_000);
        }
    }
    for m in &mut leg.maneuvers {
        m.anchor.leg_shape_index -= START_PT;
        m.anchor.along_route_cm -= base;
    }
    leg.length_cm -= base;
    b.length_cm -= base;
    b.eta_ms -= t0;
    b
}

/// A fix whose projected x is exactly `x_cm` on the main line (`round_q16(dlon × k)`).
fn fix_x(b: &RouteBundle, at: u64, x_cm: i64) -> Fix {
    let ch = &b.legs[0].chunks[0];
    let k = i64::from(ch.k_lon_q16);
    let guess = x_cm * 65_536 / k;
    let dlon = (guess - 3..=guess + 3)
        .find(|d| (d * k + 32_768) >> 16 == x_cm)
        .unwrap_or(guess);
    Fix {
        at_mono_ms: at,
        lat_e7: ch.origin_e7.lat,
        lon_e7: ch.origin_e7.lon + dlon as i32,
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

fn tick(s: &mut Session, now: u64, fix: Option<Fix>, events: Vec<Value>) -> TickOutput {
    let input: TickInput = serde_json::from_value(json!({
        "now_mono_ms": now,
        "fix": fix,
        "events": events,
    }))
    .unwrap();
    s.tick(input)
}

fn key(s: &Session) -> RouteKey {
    s.route().unwrap().route_key.clone()
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

fn tap(at: u64, target_id: &str, action: &str) -> Value {
    json!({"at_mono_ms": at, "kind": "user_tap", "target_id": target_id, "action": action})
}

fn commits(o: &TickOutput) -> Vec<(String, RouteKey, Option<String>)> {
    o.planner_cmds
        .iter()
        .filter_map(|c| match c {
            PlannerCmd::Commit {
                req_id,
                route_key,
                undo_req_id,
            } => Some((req_id.clone(), route_key.clone(), undo_req_id.clone())),
            _ => None,
        })
        .collect()
}

fn undo_speaks(o: &TickOutput) -> usize {
    o.voice_cmds
        .iter()
        .filter(|v| {
            matches!(v, VoiceCmd::Speak { template_id, .. }
                if template_id == UNDO_UNAVAILABLE_TEMPLATE_ID)
        })
        .count()
}

fn refused(o: &TickOutput, code: &str) -> bool {
    o.log
        .iter()
        .any(|l| l.what == "avoid_undo_refused" && l.detail.as_deref() == Some(code))
}

/// Consent time (tap) in every scenario.
const CONSENT_AT: u64 = 3_000;

struct Consented {
    s: Session,
    orig: RouteBundle,
    offer: AvoidOffer,
    orig_key: RouteKey,
    consent: TickOutput,
}

/// On `orig`: tracking at 500 m starts r1; while it runs the car moves to 850 m (past
/// edge 1001); ok → offer (`consented_on`: consent at 3 s).
fn offered_on(orig: RouteBundle) -> (Session, RouteBundle, AvoidOffer, RouteKey) {
    let mut s = Session::new(config());
    s.route_commit(&RouteCommitRequest {
        req_id: None,
        expected_route_key: None,
        bundle: orig.clone(),
    })
    .unwrap();
    let k = key(&s);
    let o = tick(&mut s, 1_000, Some(fix_x(&orig, 1_000, START_X)), vec![]);
    assert!(o.planner_cmds.iter().any(|c| matches!(c,
        PlannerCmd::Start { req_id, priority: SearchPriority::AvoidChip, .. } if req_id == "r1")));
    assert_eq!(
        s.avoid_state().target().unwrap().start_progress_cm,
        START_X as u32
    );
    let fin = json!({
        "at_mono_ms": 1_400, "kind": "search_finished", "req_id": "r1", "route_key": k,
        "graph_version": k.graph_version, "outcome": "ok", "facts_ref": "f1", "engine_ms": 300
    });
    tick(&mut s, 1_500, Some(fix_x(&orig, 1_500, 85_000)), vec![fin]);
    let base = s.avoid_state().target().unwrap().base_eta_ms;
    let AvoidOfferEvalResult::Offered { avoid_offer } =
        s.avoid_offer_eval(&AvoidOfferEvalRequest {
            now_mono_ms: 1_600,
            alt_facts: facts(&k.graph_version, base + 30_000),
        })
    else {
        panic!("not offered");
    };
    (s, orig, avoid_offer, k)
}

fn consented_on(orig: RouteBundle) -> Consented {
    let (mut s, orig, avoid_offer, k) = offered_on(orig);
    let consent = tick(
        &mut s,
        CONSENT_AT,
        None,
        vec![tap(CONSENT_AT, &avoid_offer.offer_id, "avoid")],
    );
    Consented {
        s,
        orig,
        offer: avoid_offer,
        orig_key: k,
        consent,
    }
}

/// Kotlin runs the consent commit: the avoid route goes in (expected = the original key).
fn load(c: &mut Consented, avoid: RouteBundle) -> RouteKey {
    c.s.route_commit(&RouteCommitRequest {
        req_id: Some("r1".into()),
        expected_route_key: Some(c.orig_key.clone()),
        bundle: avoid,
    })
    .unwrap()
    .route_key
}

/// Consented and the avoid route loaded; then a fix at original x `x_cm` (avoid along =
/// x − 500 m) at 4 s.
fn on_avoid_route_at(x_cm: i64) -> (Consented, RouteKey) {
    let mut c = consented_on(main_line());
    let avoid = avoid_line(&c.orig);
    let ak = load(&mut c, avoid);
    let o = tick(&mut c.s, 4_000, Some(fix_x(&c.orig, 4_000, x_cm)), vec![]);
    assert_eq!(o.horizon.unwrap().status, HorizonStatus::Tracking);
    assert_eq!(
        c.s.aligner().progress_cm(),
        Some((x_cm - START_X) as u32),
        "exact avoid-route position"
    );
    (c, ak)
}

fn divergence(c: &Consented) -> Option<u32> {
    match c.s.undo_state() {
        UndoState::Window(w) => w.divergence_cm,
        other => panic!("no undo window: {other:?}"),
    }
}

/// The first D edge starts at original 1200.01 m = avoid along 70_001 cm.
const DIVERGENCE_CM: u32 = 120_001 - START_X as u32;

// ---- consent and the divergence point ----------------------------------------------------

/// The consent commit names the undo id; the divergence point is computed from the
/// search-start position (500 m) although the car was already past edge 1001 (850 m).
#[test]
fn consent_commit_carries_undo_id_and_divergence_uses_the_search_start() {
    let mut c = consented_on(main_line());
    assert_eq!(
        commits(&c.consent),
        vec![(
            "r1".to_owned(),
            c.orig_key.clone(),
            Some("undo-r1".to_owned())
        )]
    );
    assert!(matches!(c.s.undo_state(), UndoState::Consented(_)));
    let avoid = avoid_line(&c.orig);
    load(&mut c, avoid);
    assert_eq!(divergence(&c), Some(DIVERGENCE_CM));
}

/// The consent commit serializes `undo_req_id`; other commits do not carry it.
#[test]
fn commit_json_carries_undo_req_id_only_on_consent() {
    let c = consented_on(main_line());
    let v = serde_json::to_value(&c.consent.planner_cmds).unwrap();
    assert_eq!(v[0]["cmd"], "commit");
    assert_eq!(v[0]["undo_req_id"], "undo-r1");
    let plain = PlannerCmd::Commit {
        req_id: "r2".into(),
        route_key: c.orig_key.clone(),
        undo_req_id: None,
    };
    assert!(serde_json::to_value(plain)
        .unwrap()
        .get("undo_req_id")
        .is_none());
}

// ---- Gate 7 cases ------------------------------------------------------------------------

/// Past the avoid route's first edge (avoid along 40_000 > 30_000) but before the
/// divergence point (70_001): undone. commit{req_id: undo-r1, route_key: the current
/// (avoid) route}, no refusal phrase.
#[test]
fn undo_before_the_divergence_point_commits_the_previous_route() {
    let (mut c, ak) = on_avoid_route_at(90_000);
    let o = tick(
        &mut c.s,
        4_500,
        None,
        vec![tap(4_500, &c.offer.offer_id, "undo")],
    );
    assert_eq!(commits(&o), vec![("undo-r1".to_owned(), ak.clone(), None)]);
    assert_eq!(undo_speaks(&o), 0);
    assert!(o.log.iter().any(|l| l.what == "avoid_undo_commit"));
    assert_eq!(*c.s.undo_state(), UndoState::None);
    // a second undo has nothing to undo
    let o = tick(
        &mut c.s,
        4_600,
        None,
        vec![tap(4_600, &c.offer.offer_id, "undo")],
    );
    assert!(commits(&o).is_empty());
    assert!(refused(&o, "NO_PENDING_TARGET"));
}

/// Kotlin's held previous body still carries the revision it was loaded with ("1"):
/// passed as is it is refused with E_SCHEMA (nothing replaced); set back to "0" it goes in
/// as current + 1 (docs 0aaeee6) and alignment starts over on it.
#[test]
fn undo_route_must_be_passed_with_revision_zero() {
    let (mut c, ak) = on_avoid_route_at(90_000);
    assert_eq!(c.orig_key.route_revision, U64Str(1));
    assert_eq!(ak.route_revision, U64Str(2));
    let o = tick(
        &mut c.s,
        4_500,
        None,
        vec![tap(4_500, &c.offer.offer_id, "undo")],
    );
    let (req_id, guard, _) = commits(&o).remove(0);
    assert_eq!(req_id, "undo-r1");
    // the held body as it was current: route_revision "1"
    let mut held = c.orig.clone();
    held.route_key = c.orig_key.clone();
    let e =
        c.s.route_commit(&RouteCommitRequest {
            req_id: Some(req_id.clone()),
            expected_route_key: Some(guard.clone()),
            bundle: held.clone(),
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Schema);
    assert_eq!(key(&c.s), ak, "nothing replaced");
    // revision back to "0": current (2) + 1
    held.route_key.route_revision = U64Str(0);
    let r =
        c.s.route_commit(&RouteCommitRequest {
            req_id: Some(req_id.clone()),
            expected_route_key: Some(guard),
            bundle: held,
        })
        .unwrap();
    assert_eq!(r.route_key.route_revision, U64Str(3));
    assert_eq!(key(&c.s), r.route_key);
    assert_eq!(c.s.horizon_status(4_600), Some(HorizonStatus::Uncertain));
    assert_eq!(*c.s.undo_state(), UndoState::None);
}

/// Exactly at the divergence point: not undone (NO_EFFECT) and one refusal phrase.
#[test]
fn undo_at_the_divergence_point_is_no_effect() {
    let (mut c, _) = on_avoid_route_at(START_X + i64::from(DIVERGENCE_CM));
    let o = tick(
        &mut c.s,
        4_500,
        None,
        vec![tap(4_500, &c.offer.offer_id, "undo")],
    );
    assert!(commits(&o).is_empty());
    assert!(refused(&o, "NO_EFFECT"));
    assert_eq!(undo_speaks(&o), 1);
    // one cm before it is still undone
    let (mut c, _) = on_avoid_route_at(START_X + i64::from(DIVERGENCE_CM) - 1);
    let o = tick(
        &mut c.s,
        4_500,
        None,
        vec![tap(4_500, &c.offer.offer_id, "undo")],
    );
    assert_eq!(commits(&o).len(), 1);
}

/// Past the divergence point: NO_EFFECT.
#[test]
fn undo_past_the_divergence_point_is_no_effect() {
    let (mut c, _) = on_avoid_route_at(140_000);
    let o = tick(
        &mut c.s,
        4_500,
        None,
        vec![tap(4_500, &c.offer.offer_id, "undo")],
    );
    assert!(commits(&o).is_empty());
    assert!(refused(&o, "NO_EFFECT"));
    assert_eq!(undo_speaks(&o), 1);
}

/// Position uncertain (the avoid route was loaded, no fix matched yet): NO_EFFECT.
#[test]
fn undo_while_uncertain_is_no_effect() {
    let mut c = consented_on(main_line());
    let avoid = avoid_line(&c.orig);
    load(&mut c, avoid);
    let o = tick(
        &mut c.s,
        4_000,
        None,
        vec![tap(4_000, &c.offer.offer_id, "undo")],
    );
    assert_eq!(o.horizon.as_ref().unwrap().status, HorizonStatus::Uncertain);
    assert!(commits(&o).is_empty());
    assert!(refused(&o, "NO_EFFECT"));
    assert_eq!(undo_speaks(&o), 1);
}

/// More than 5 s after consent: NO_PENDING_TARGET; exactly 5 s is still inside.
#[test]
fn undo_after_five_seconds_is_no_pending_target() {
    let (mut c, _) = on_avoid_route_at(90_000);
    let late = CONSENT_AT + 5_001;
    let o = tick(
        &mut c.s,
        late,
        None,
        vec![tap(late, &c.offer.offer_id, "undo")],
    );
    assert!(commits(&o).is_empty());
    assert!(refused(&o, "NO_PENDING_TARGET"));
    assert_eq!(undo_speaks(&o), 1);
    let (mut c, _) = on_avoid_route_at(90_000);
    let edge = CONSENT_AT + 5_000;
    let o = tick(
        &mut c.s,
        edge,
        None,
        vec![tap(edge, &c.offer.offer_id, "undo")],
    );
    assert_eq!(commits(&o).len(), 1);
}

/// The avoid route's first edge is not on the original route ahead of the search start:
/// divergence unknown → NO_EFFECT.
#[test]
fn divergence_not_found_is_no_effect() {
    let mut c = consented_on(main_line());
    let mut avoid = avoid_line(&c.orig);
    avoid.legs[0].edges[0].edge_id = U64Str(9_999);
    load(&mut c, avoid);
    assert_eq!(divergence(&c), None);
    tick(&mut c.s, 4_000, Some(fix_x(&c.orig, 4_000, 90_000)), vec![]);
    let o = tick(
        &mut c.s,
        4_500,
        None,
        vec![tap(4_500, &c.offer.offer_id, "undo")],
    );
    assert!(commits(&o).is_empty());
    assert!(refused(&o, "NO_EFFECT"));
    assert_eq!(undo_speaks(&o), 1);
}

/// The first edge appears twice ahead of the search start (a looping route): not
/// determined → unknown → NO_EFFECT. An occurrence behind the start does not count.
#[test]
fn divergence_ambiguous_is_no_effect_but_behind_the_start_is_ignored() {
    let mut looped = main_line();
    looped.legs[0].edges[8].edge_id = U64Str(1_001);
    let mut c = consented_on(looped);
    let avoid = avoid_line(&main_line());
    load(&mut c, avoid);
    assert_eq!(divergence(&c), None);
    tick(&mut c.s, 4_000, Some(fix_x(&c.orig, 4_000, 90_000)), vec![]);
    let o = tick(
        &mut c.s,
        4_500,
        None,
        vec![tap(4_500, &c.offer.offer_id, "undo")],
    );
    assert!(refused(&o, "NO_EFFECT"));

    let mut behind = main_line();
    behind.legs[0].edges[0].edge_id = U64Str(1_001);
    let mut c = consented_on(behind);
    let avoid = avoid_line(&main_line());
    load(&mut c, avoid);
    assert_eq!(divergence(&c), Some(DIVERGENCE_CM));
}

fn cancels(o: &TickOutput) -> Vec<String> {
    o.planner_cmds
        .iter()
        .filter_map(|c| match c {
            PlannerCmd::CancelCommit { req_id } => Some(req_id.clone()),
            _ => None,
        })
        .collect()
}

/// Kotlin's `dc_route_commit` of the avoid route (`req_id` r1), as a raw result.
fn load_raw(c: &mut Consented) -> Result<RouteKey, ErrorCode> {
    let avoid = avoid_line(&c.orig);
    c.s.route_commit(&RouteCommitRequest {
        req_id: Some("r1".into()),
        expected_route_key: Some(c.orig_key.clone()),
        bundle: avoid,
    })
    .map(|r| r.route_key)
    .map_err(|e| e.code)
}

/// Undo first (docs 836a370): consent given, the avoid route not loaded yet → the consent
/// commit is withdrawn with `cancel_commit{r1}` (a success: no refusal phrase); the
/// `dc_route_commit` for r1 arriving afterwards is `E_STALE` and nothing is replaced.
/// No consent at all: NO_PENDING_TARGET.
#[test]
fn undo_before_the_avoid_route_is_loaded_cancels_the_commit() {
    let mut c = consented_on(main_line());
    assert_eq!(c.s.issued_commits().collect::<Vec<_>>(), vec!["r1"]);
    let o = tick(
        &mut c.s,
        3_500,
        None,
        vec![tap(3_500, &c.offer.offer_id, "undo")],
    );
    assert!(commits(&o).is_empty());
    assert_eq!(cancels(&o), vec!["r1".to_owned()]);
    assert_eq!(undo_speaks(&o), 0);
    assert!(!o.log.iter().any(|l| l.what == "avoid_undo_refused"));
    assert!(o.log.iter().any(|l| l.what == "avoid_commit_cancelled"));
    assert_eq!(*c.s.undo_state(), UndoState::None);
    assert_eq!(c.s.issued_commits().count(), 0);
    assert_eq!(load_raw(&mut c), Err(ErrorCode::Stale));
    assert_eq!(key(&c.s), c.orig_key, "nothing replaced");
    // a second undo: nothing pending any more
    let o = tick(
        &mut c.s,
        3_600,
        None,
        vec![tap(3_600, &c.offer.offer_id, "undo")],
    );
    assert!(cancels(&o).is_empty());
    assert!(refused(&o, "NO_PENDING_TARGET"));

    let mut s = Session::new(config());
    s.route_commit(&RouteCommitRequest {
        req_id: None,
        expected_route_key: None,
        bundle: main_line(),
    })
    .unwrap();
    let o = tick(&mut s, 1_000, None, vec![tap(1_000, "avoid-r1", "undo")]);
    assert!(refused(&o, "NO_PENDING_TARGET"));
    assert_eq!(undo_speaks(&o), 1);
}

/// The other order (the actor is serial, so the order decides deterministically): the
/// avoid route's `dc_route_commit` first → the undo is the normal one (`commit{undo-r1}`),
/// never `cancel_commit`.
#[test]
fn route_commit_first_then_undo_is_the_normal_undo() {
    let mut c = consented_on(main_line());
    let ak = load_raw(&mut c).unwrap();
    tick(&mut c.s, 3_400, Some(fix_x(&c.orig, 3_400, 90_000)), vec![]);
    let o = tick(
        &mut c.s,
        3_500,
        None,
        vec![tap(3_500, &c.offer.offer_id, "undo")],
    );
    assert!(cancels(&o).is_empty());
    assert_eq!(commits(&o), vec![("undo-r1".to_owned(), ak, None)]);
}

/// Consent and undo in the same tick: the output carries `commit{r1}` then
/// `cancel_commit{r1}`, and nothing is left waiting.
#[test]
fn consent_and_undo_in_one_tick_leave_nothing_waiting() {
    let (mut s, _, offer, k) = offered_on(main_line());
    let o = tick(
        &mut s,
        CONSENT_AT,
        None,
        vec![
            tap(CONSENT_AT, &offer.offer_id, "avoid"),
            tap(CONSENT_AT, &offer.offer_id, "undo"),
        ],
    );
    assert_eq!(
        o.planner_cmds,
        vec![
            PlannerCmd::Commit {
                req_id: "r1".into(),
                route_key: k,
                undo_req_id: Some("undo-r1".into()),
            },
            PlannerCmd::CancelCommit {
                req_id: "r1".into()
            },
        ]
    );
    assert_eq!(s.issued_commits().count(), 0);
}

// ---- CXL ---------------------------------------------------------------------------------

fn cxl(s: &mut Session, at: u64) -> ValidateOverrideResult {
    s.validate_override(ValidateOverrideRequest {
        code_line: "CXL".into(),
        contract: json!({}),
        utterance_id: "u1".into(),
        utterance_mono_ms: at,
        ref_frame_seq: 0,
        pending_offer_id: None,
    })
    .unwrap()
}

/// `CXL` before the divergence point: `undone`; the commit comes with the next tick.
#[test]
fn cxl_undoes_through_the_next_tick() {
    let (mut c, ak) = on_avoid_route_at(90_000);
    assert_eq!(
        cxl(&mut c.s, 4_500),
        ValidateOverrideResult::Undone {
            req_id: "undo-r1".into()
        }
    );
    let o = tick(&mut c.s, 4_600, None, vec![]);
    assert_eq!(commits(&o), vec![("undo-r1".to_owned(), ak, None)]);
    assert_eq!(undo_speaks(&o), 0);
}

/// `CXL` before the avoid route is loaded: `undone{r1}` (the consent commit's req_id);
/// `cancel_commit{r1}` with the next tick, no refusal phrase; the late route is `E_STALE`
/// even before that tick.
#[test]
fn cxl_before_the_avoid_route_is_loaded_cancels_the_commit() {
    let mut c = consented_on(main_line());
    assert_eq!(
        cxl(&mut c.s, 3_500),
        ValidateOverrideResult::Undone {
            req_id: "r1".into()
        }
    );
    assert_eq!(load_raw(&mut c), Err(ErrorCode::Stale));
    let o = tick(&mut c.s, 3_600, None, vec![]);
    assert_eq!(cancels(&o), vec!["r1".to_owned()]);
    assert!(commits(&o).is_empty());
    assert_eq!(undo_speaks(&o), 0);
    assert_eq!(key(&c.s), c.orig_key);
}

/// `CXL` refused: the reject code comes back and the phrase goes out with the next tick.
#[test]
fn cxl_refusals_return_the_code_and_speak_once() {
    let (mut c, _) = on_avoid_route_at(140_000);
    assert_eq!(
        cxl(&mut c.s, 4_500),
        ValidateOverrideResult::Rejected {
            code: RejectCode::NoEffect
        }
    );
    let o = tick(&mut c.s, 4_600, None, vec![]);
    assert!(commits(&o).is_empty());
    assert_eq!(undo_speaks(&o), 1);

    let (mut c, _) = on_avoid_route_at(90_000);
    assert_eq!(
        cxl(&mut c.s, CONSENT_AT + 5_001),
        ValidateOverrideResult::Rejected {
            code: RejectCode::NoPendingTarget
        }
    );
    let o = tick(&mut c.s, CONSENT_AT + 5_100, None, vec![]);
    assert_eq!(undo_speaks(&o), 1);
}
