//! L3 override end-to-end flow (PR #20, docs 03 §4.1 / §4.2, 07 §1):
//! `dc_validate_override` rejection boundaries (`STALE_REVISION`, `EXPIRED`, `UNKNOWN_REF`,
//! `NO_EFFECT`, `jp=last` without route change) → `start{priority: override, fix}` →
//! `evaluate_offer` → `dc_avoid_offer_eval` (same override weights, >= 30,000 ms score
//! improvement, detour cap, `jp` edge-pair check) → `ui_cmds: show_confirm` → 30 s deadline
//! `dismiss{expired}` / `cancelled` / `superseded` / `route_changed` → consent via dict "はい"
//! or `user_tap{confirm}` → `commit{req_id, route_key, undo_req_id}` (allowed while driving) →
//! divergence check in `dc_route_commit` → 5 s `undo` / `cancel_commit`.
use contracts::facts::{
    AvoidDeclineReason, AvoidOfferEvalRequest, AvoidOfferEvalResult, CandidateFacts, FactJunction,
};
use contracts::horizon::{Arrow, Evidence, HorizonStatus};
use contracts::override_types::{
    Detour, Level, RejectCode, ValidateOverrideRequest, ValidateOverrideResult, Weights,
};
use contracts::route::{RouteBundle, RouteCommitRequest};
use contracts::tick::{
    DismissReason, Fix, PlannerCmd, SearchPriority, TickInput, TickOutput, UiCmd, VoiceCmd,
    VoicePriority,
};
use contracts::{ErrorCode, RouteKey, U64Str};
use drive_core::avoid::OVERRIDE_NO_ROUTE_TEMPLATE_ID;
use drive_core::session::OverrideState;
use drive_core::{Session, SessionConfig};
use serde_json::{json, Value};

// ---- helpers ---------------------------------------------------------------------------

fn main_line() -> RouteBundle {
    let p = format!(
        "{}/../../tests/fixtures/route/gate7-side-road-20m.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    let mut b: RouteBundle = serde_json::from_value(v["bundle"].clone()).unwrap();
    // Mark maneuver 1 (2.5 km right turn) as confirmed right_unprotected so refs.next /
    // refs.last populate per horizon.rs.
    let ev = |val: &str| -> Evidence {
        serde_json::from_value(json!({"kind": "confirmed", "value": val, "source_ref": "test"}))
            .unwrap()
    };
    let m1 = &mut b.legs[0].maneuvers[1];
    m1.crossing = ev("across_oncoming");
    m1.approach_signal = ev("absent");
    b
}

/// Alternate route starting at x = 500 m (shape point 5, inside edge 1001) and diverging
/// at edge index 3 (x = 1200 m on original, 700 m on alt).
const START_PT: u32 = 5;
const START_X: i64 = 50_000;
const DIV_ORIG_X: i64 = 120_000;
const FIRST_ALT_EDGE: usize = 3;

fn alt_line(orig: &RouteBundle) -> RouteBundle {
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
    let t0 = 26_667 + (base - 40_001) * (53_333 - 26_667) / (80_000 - 40_001);
    leg.edges.retain(|e| e.to_pt > START_PT);
    for (i, e) in leg.edges.iter_mut().enumerate() {
        e.from_pt = e.from_pt.saturating_sub(START_PT);
        e.to_pt -= START_PT;
        e.length_cm = along[e.to_pt as usize] - along[e.from_pt as usize];
        e.cum_time_ms -= t0;
        if i + 1 >= FIRST_ALT_EDGE {
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

fn fix_xy(b: &RouteBundle, at: u64, x_cm: i64, y_cm: i64) -> Fix {
    let ch = &b.legs[0].chunks[0];
    let mut f = fix_x(b, at, x_cm);
    f.lat_e7 = ch.origin_e7.lat + (y_cm * 65_536 / i64::from(ch.k_lat_q16)) as i32;
    f
}

fn session_on(b: &RouteBundle) -> (Session, RouteKey) {
    let mut s = Session::new(SessionConfig {
        fix_stale_ms: 1_000_000,
        ..Default::default()
    });
    s.record_plan_ok_for_test("p-init", b.route_key.clone());
    let res = s
        .route_commit(&RouteCommitRequest {
            req_id: "p-init".into(),
            expected_route_key: None,
            bundle: b.clone(),
        })
        .unwrap();
    (s, res.route_key)
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

fn ovr_req(
    code_line: &str,
    utterance_mono_ms: u64,
    ref_frame_seq: u64,
    contract: Value,
) -> ValidateOverrideRequest {
    ValidateOverrideRequest {
        code_line: code_line.to_owned(),
        contract,
        utterance_id: format!("u-{utterance_mono_ms}"),
        utterance_mono_ms,
        ref_frame_seq,
        pending_offer_id: None,
    }
}

fn make_facts(graph_version: &str, eta_ms: u64, right_unprotected: u64) -> CandidateFacts {
    serde_json::from_value(json!({
        "candidate_id": "c-alt",
        "label": "costing_variant",
        "plan_request_id": "drive",
        "graph_version": graph_version,
        "fingerprint": "fp-alt",
        "legal": true,
        "complete": true,
        "eta_ms": eta_ms,
        "length_m": 4000,
        "counts": {
            "right_unprotected": right_unprotected,
            "right_signal_unknown_protection": 0,
            "right_signal_unknown": 0,
            "right_protected": 0,
            "right_entry": 0,
            "merges": 0
        },
        "narrow_m": {"confirmed": 0, "inferred": 0},
        "unknown_width_residential_m": 0,
        "unknown_length_m": 0,
        "junctions": [],
        "through_anchors": []
    }))
    .unwrap()
}

fn search_finished_ev(at: u64, req_id: &str, rk: &RouteKey) -> Value {
    json!({
        "at_mono_ms": at,
        "kind": "search_finished",
        "req_id": req_id,
        "route_key": rk,
        "graph_version": rk.graph_version,
        "outcome": "ok",
        "facts_ref": "alt-facts-1",
        "base_facts_ref": "base-facts-1",
        "engine_ms": 200
    })
}

fn has_no_route_voice(o: &TickOutput) -> bool {
    o.voice_cmds.iter().any(|v| {
        matches!(
            v,
            VoiceCmd::Speak {
                priority: VoicePriority::P2,
                template_id,
                ..
            } if template_id == OVERRIDE_NO_ROUTE_TEMPLATE_ID
        )
    })
}

/// Starts a tracking session at x = 500 m, applies `code_line` at t = 10_000, starts the
/// override search, finishes it, and returns `(session, orig_bundle, route_key, req_id, override_id)`.
fn ready_override(code_line: &str) -> (Session, RouteBundle, RouteKey, String, String) {
    let orig = main_line();
    let (mut s, rk) = session_on(&orig);
    let o1 = tick(&mut s, 10_000, Some(fix_x(&orig, 10_000, START_X)), vec![]);
    let frame_seq = o1.horizon.unwrap().frame_seq;
    let res = s
        .validate_override(ovr_req(
            code_line,
            10_000,
            frame_seq,
            json!({"route_key": rk}),
        ))
        .unwrap();
    let ValidateOverrideResult::Applied {
        override_id,
        expires_at_mono_ms,
        ..
    } = res
    else {
        panic!("expected Applied, got {res:?}");
    };
    assert_eq!(expires_at_mono_ms, 40_000);
    // Next tick emits exactly 1 start{priority: override, fix: Some(...)}
    let o2 = tick(&mut s, 10_100, None, vec![]);
    let req_id = match &o2.planner_cmds[..] {
        [PlannerCmd::Start {
            req_id,
            priority: SearchPriority::Override,
            fix: Some(f),
            route_key,
            ..
        }] => {
            assert_eq!(*route_key, rk);
            assert_eq!(f.at_mono_ms, 10_000);
            req_id.clone()
        }
        other => panic!("expected 1 override start with fix, got {other:?}"),
    };
    // search_finished -> evaluate_offer
    let o3 = tick(
        &mut s,
        10_500,
        None,
        vec![search_finished_ev(10_500, &req_id, &rk)],
    );
    assert_eq!(
        o3.planner_cmds,
        vec![PlannerCmd::EvaluateOffer {
            req_id: req_id.clone(),
            facts_ref: "alt-facts-1".into(),
            base_facts_ref: "base-facts-1".into(),
        }]
    );
    (s, orig, rk, req_id, override_id)
}

// ---- 1. Rejection boundaries: STALE_REVISION / EXPIRED / UNKNOWN_REF / NO_EFFECT -------

#[test]
fn stale_revision_route_key_and_jp_refs_vs_weights_only() {
    let orig = main_line();
    let (mut s, rk) = session_on(&orig);
    let o1 = tick(&mut s, 10_000, Some(fix_x(&orig, 10_000, START_X)), vec![]);
    let seq1 = o1.horizon.as_ref().unwrap().frame_seq;
    assert!(o1.horizon.as_ref().unwrap().refs.next.is_some());

    // Wrong route_key -> STALE_REVISION even for weights-only OVR
    let mut wrong_rk = rk.clone();
    wrong_rk.route_revision = U64Str(99);
    let r = s
        .validate_override(ovr_req(
            "OVR rc=H",
            10_000,
            seq1,
            json!({"route_key": wrong_rk}),
        ))
        .unwrap();
    assert_eq!(
        r,
        ValidateOverrideResult::Rejected {
            code: RejectCode::StaleRevision
        }
    );

    // Advance fix slightly (same upcoming difficult junction, frame_seq advances seq1 -> seq2)
    let o2 = tick(
        &mut s,
        11_000,
        Some(fix_x(&orig, 11_000, START_X + 2_000)),
        vec![],
    );
    let seq2 = o2.horizon.as_ref().unwrap().frame_seq;
    assert!(seq2 > seq1);

    // Weight-only OVR with stale frame_seq (even 0!) and matching route_key -> Applied!
    let r = s
        .validate_override(ovr_req("OVR rc=H", 11_000, 0, json!({"route_key": rk})))
        .unwrap();
    assert!(matches!(r, ValidateOverrideResult::Applied { .. }), "{r:?}");

    // jp=next with old seq1 where refs.next STILL points to the SAME junction -> Applied!
    let r = s
        .validate_override(ovr_req(
            "OVR jp=next",
            11_000,
            seq1,
            json!({"route_key": rk}),
        ))
        .unwrap();
    assert!(matches!(r, ValidateOverrideResult::Applied { .. }), "{r:?}");

    // Pass the right turn at 2.5 km (two consecutive fixes beyond 250_000 cm) -> refs.next disappears
    tick(&mut s, 12_000, Some(fix_x(&orig, 12_000, 260_000)), vec![]);
    let o3 = tick(&mut s, 13_000, Some(fix_x(&orig, 13_000, 265_000)), vec![]);
    let h3 = o3.horizon.unwrap();
    assert!(h3.refs.next.is_none());
    let seq3 = h3.frame_seq;

    // jp=next referencing seq1 (when j0-1 was next, now None) -> STALE_REVISION!
    let r = s
        .validate_override(ovr_req(
            "OVR jp=next",
            13_000,
            seq1,
            json!({"route_key": rk}),
        ))
        .unwrap();
    assert_eq!(
        r,
        ValidateOverrideResult::Rejected {
            code: RejectCode::StaleRevision
        }
    );

    // jp=next referencing current seq3 (no upcoming difficult junction) -> UNKNOWN_REF!
    let r = s
        .validate_override(ovr_req(
            "OVR jp=next",
            13_000,
            seq3,
            json!({"route_key": rk}),
        ))
        .unwrap();
    assert_eq!(
        r,
        ValidateOverrideResult::Rejected {
            code: RejectCode::UnknownRef
        }
    );
}

#[test]
fn expired_30s_and_jp_next_deadline_boundaries() {
    let orig = main_line();
    let (mut s, rk) = session_on(&orig);
    let o1 = tick(&mut s, 40_000, Some(fix_x(&orig, 40_000, START_X)), vec![]);
    let seq = o1.horizon.as_ref().unwrap().frame_seq;
    let next_deadline = o1
        .horizon
        .as_ref()
        .unwrap()
        .refs
        .next
        .as_ref()
        .unwrap()
        .deadline_mono_ms;

    // Exact 30 s boundary: now (40_000) == utterance (10_000) + 30_000 -> NOT expired
    let r = s
        .validate_override(ovr_req("OVR rc=H", 10_000, seq, json!({"route_key": rk})))
        .unwrap();
    assert!(matches!(r, ValidateOverrideResult::Applied { .. }), "{r:?}");

    // 1 ms past 30 s: now (40_000) > utterance (9_999) + 30_000 -> EXPIRED
    let r = s
        .validate_override(ovr_req("OVR rc=H", 9_999, seq, json!({"route_key": rk})))
        .unwrap();
    assert_eq!(
        r,
        ValidateOverrideResult::Rejected {
            code: RejectCode::Expired
        }
    );

    // jp=next past its 20 s branch deadline -> EXPIRED
    tick(&mut s, next_deadline, None, vec![]);
    let r = s
        .validate_override(ovr_req(
            "OVR jp=next",
            next_deadline,
            seq,
            json!({"route_key": rk}),
        ))
        .unwrap();
    assert_eq!(
        r,
        ValidateOverrideResult::Rejected {
            code: RejectCode::Expired
        }
    );
}

#[test]
fn no_effect_and_jp_last_without_route_change() {
    let orig = main_line();
    let (mut s, rk) = session_on(&orig);
    let o1 = tick(&mut s, 10_000, Some(fix_x(&orig, 10_000, START_X)), vec![]);
    let seq = o1.horizon.unwrap().frame_seq;

    // Expanded weights equal current (L2=1000, L3=None/Normal) and detour unchanged -> NO_EFFECT
    for line in ["OVR rc=N", "OVR nr=N mg=N", "OVR dt=S", "OVR rc=N dt=S"] {
        let r = s
            .validate_override(ovr_req(line, 10_000, seq, json!({"route_key": rk})))
            .unwrap();
        assert_eq!(
            r,
            ValidateOverrideResult::Rejected {
                code: RejectCode::NoEffect
            },
            "{line}"
        );
    }

    // jp=last alone with no passed junction: Applied with review_proposal=None, NO route search
    let r = s
        .validate_override(ovr_req(
            "OVR jp=last",
            10_000,
            seq,
            json!({"route_key": rk}),
        ))
        .unwrap();
    match r {
        ValidateOverrideResult::Applied {
            review_proposal, ..
        } => assert!(review_proposal.is_none()),
        other => panic!("expected Applied, got {other:?}"),
    }
    assert_eq!(*s.override_state(), OverrideState::Idle);
    let o_no_search = tick(&mut s, 10_100, None, vec![]);
    assert!(o_no_search.planner_cmds.is_empty());

    // Pass the right turn at 2.5 km -> refs.last is populated; jp=last produces review_proposal
    // without starting any route search
    tick(&mut s, 12_000, Some(fix_x(&orig, 12_000, 260_000)), vec![]);
    let o_passed = tick(&mut s, 13_000, Some(fix_x(&orig, 13_000, 265_000)), vec![]);
    let seq_passed = o_passed.horizon.as_ref().unwrap().frame_seq;
    assert!(o_passed.horizon.as_ref().unwrap().refs.last.is_some());
    let r = s
        .validate_override(ovr_req(
            "OVR jp=last",
            13_000,
            seq_passed,
            json!({"route_key": rk}),
        ))
        .unwrap();
    match r {
        ValidateOverrideResult::Applied {
            review_proposal, ..
        } => {
            let prop = review_proposal.expect("review_proposal for passed difficult junction");
            assert_eq!(prop["origin"], "jp_last");
            assert_eq!(prop["ref"], "j0-1");
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert_eq!(*s.override_state(), OverrideState::Idle);
}

// ---- 2. Min improvement boundary (30,000 ms exact pass vs 29,999 ms fail) --------------

#[test]
fn min_improvement_30s_exact_passes_and_under_fails() {
    // OVR rc=H: L2(1000) * L3_High(2000) / 1000 = 2000 permille.
    // 1 right_unprotected = 45_000 * 2000 / 1000 = 90_000 ms penalty.
    // base: eta = 600_000, right_unprotected = 1 -> score_ms = 690_000.
    // alt (exact 30_000 improvement): eta = 660_000, right_unprotected = 0 -> score_ms = 660_000.
    // diff = 30_000 == 30_000 -> PASS!
    let (mut s, _orig, rk, req_id, override_id) = ready_override("OVR rc=H");
    let base = make_facts(&rk.graph_version, 600_000, 1);
    let alt_exact = make_facts(&rk.graph_version, 660_000, 0);
    let eval_res = s.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id: req_id.clone(),
        now_mono_ms: 10_600,
        alt_facts: alt_exact,
        base_facts: base.clone(),
    });
    match eval_res {
        AvoidOfferEvalResult::OverrideOffered {
            confirm_screen_id,
            override_id: oid,
            score_improvement_ms,
            expires_at_mono_ms,
            ..
        } => {
            assert_eq!(oid, override_id);
            assert_eq!(score_improvement_ms, 30_000);
            assert_eq!(expires_at_mono_ms, 40_000);
            let o = tick(&mut s, 10_700, None, vec![]);
            assert_eq!(
                o.ui_cmds,
                vec![UiCmd::ShowConfirm {
                    confirm_screen_id,
                    override_id: oid,
                    expires_at_mono_ms: 40_000,
                }]
            );
            assert!(!has_no_route_voice(&o));
            // Before consent, route and active L3 weights are completely unchanged
            assert_eq!(s.route().unwrap().route_key, rk);
            assert_eq!(*s.active_l3_weights(), Weights::default());
        }
        other => panic!("expected OverrideOffered, got {other:?}"),
    }

    // alt (29_999 ms improvement): eta = 660_001 -> score_ms = 660_001, diff = 29_999 < 30_000 -> FAIL!
    let (mut s2, _orig2, rk2, req_id2, _) = ready_override("OVR rc=H");
    let alt_short = make_facts(&rk2.graph_version, 660_001, 0);
    let eval_res2 = s2.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id: req_id2,
        now_mono_ms: 10_600,
        alt_facts: alt_short,
        base_facts: base,
    });
    assert_eq!(
        eval_res2,
        AvoidOfferEvalResult::Declined {
            reason: AvoidDeclineReason::InsufficientImprovement
        }
    );
    let o2 = tick(&mut s2, 10_700, None, vec![]);
    assert!(o2.ui_cmds.is_empty());
    assert!(has_no_route_voice(&o2));
}

#[test]
fn jp_next_checks_target_edge_pair_not_passed() {
    let (mut s, orig, rk, req_id, _) = ready_override("OVR jp=next");
    let target_pair = s
        .route()
        .unwrap()
        .edge_pair_at(orig.legs[0].maneuvers[1].anchor.along_route_cm)
        .expect("target edge pair");
    let base = make_facts(&rk.graph_version, 600_000, 2);
    let mut alt_still_passes = make_facts(&rk.graph_version, 600_000, 0);
    alt_still_passes.junctions.push(FactJunction {
        ref_: "j0-1".into(),
        edge_in: U64Str(target_pair.0),
        edge_out: U64Str(target_pair.1),
        kind: "right_unprotected".into(),
        evidence: serde_json::from_value::<Evidence>(
            json!({"kind": "unknown", "reason": "missing"}),
        )
        .unwrap(),
    });
    let r = s.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id,
        now_mono_ms: 10_600,
        alt_facts: alt_still_passes,
        base_facts: base,
    });
    assert_eq!(
        r,
        AvoidOfferEvalResult::Declined {
            reason: AvoidDeclineReason::TargetNotAvoided
        }
    );
    let o = tick(&mut s, 10_700, None, vec![]);
    assert!(has_no_route_voice(&o));
}

// ---- 3. 30 s deadline expiry -> dismiss{expired} and late consent refused --------------

#[test]
fn confirm_expires_at_30s_with_dismiss_and_refuses_late_consent() {
    let (mut s, _orig, rk, req_id, override_id) = ready_override("OVR rc=H");
    let base = make_facts(&rk.graph_version, 600_000, 1);
    let alt = make_facts(&rk.graph_version, 600_000, 0);
    assert!(matches!(
        s.avoid_offer_eval(&AvoidOfferEvalRequest {
            req_id,
            now_mono_ms: 10_600,
            alt_facts: alt,
            base_facts: base,
        }),
        AvoidOfferEvalResult::OverrideOffered { .. }
    ));
    let o_show = tick(&mut s, 10_700, None, vec![]);
    assert!(o_show
        .ui_cmds
        .iter()
        .any(|u| matches!(u, UiCmd::ShowConfirm { .. })));
    assert_eq!(o_show.next_wake_mono_ms, Some(40_001));

    // At exactly 40_000 (inclusive 30 s): NOT dismissed yet
    let o_at = tick(&mut s, 40_000, None, vec![]);
    assert!(o_at.ui_cmds.is_empty());
    assert!(matches!(
        s.override_state(),
        OverrideState::Confirming { .. }
    ));

    // At 40_001 (1 ms past deadline): dismiss{expired}, late consent produces NO commit
    let o_exp = tick(
        &mut s,
        40_001,
        None,
        vec![json!({
            "at_mono_ms": 40_001,
            "kind": "user_tap",
            "target_id": override_id,
            "action": "confirm"
        })],
    );
    assert_eq!(
        o_exp.ui_cmds,
        vec![UiCmd::Dismiss {
            target_id: override_id,
            reason: DismissReason::Expired,
        }]
    );
    assert!(o_exp.planner_cmds.is_empty());
    assert_eq!(*s.override_state(), OverrideState::Idle);
    assert_eq!(s.route().unwrap().route_key, rk);
}

// ---- 4. Divergence already passed at dc_route_commit -> route unchanged + voice --------

#[test]
fn commit_after_passing_divergence_keeps_route_and_speaks_no_route() {
    let (mut s, orig, rk, req_id, override_id) = ready_override("OVR rc=H");
    let base = make_facts(&rk.graph_version, 600_000, 1);
    let alt = make_facts(&rk.graph_version, 600_000, 0);
    assert!(matches!(
        s.avoid_offer_eval(&AvoidOfferEvalRequest {
            req_id: req_id.clone(),
            now_mono_ms: 10_600,
            alt_facts: alt,
            base_facts: base,
        }),
        AvoidOfferEvalResult::OverrideOffered { .. }
    ));
    tick(&mut s, 10_700, None, vec![]);

    // Vehicle moves past the divergence point (120_000 cm) before committing
    let o_consent = tick(
        &mut s,
        12_000,
        Some(fix_x(&orig, 12_000, DIV_ORIG_X + 5_000)),
        vec![json!({
            "at_mono_ms": 12_000,
            "kind": "user_tap",
            "target_id": override_id,
            "action": "confirm"
        })],
    );
    assert!(o_consent.planner_cmds.iter().any(|c| matches!(
        c,
        PlannerCmd::Commit {
            req_id: r,
            undo_req_id: Some(_),
            ..
        } if *r == req_id
    )));

    // Kotlin calls dc_route_commit with the alternate route: Rust sees vehicle >= divergence
    let alt_bundle = alt_line(&orig);
    let err = s
        .route_commit(&RouteCommitRequest {
            req_id,
            expected_route_key: Some(rk.clone()),
            bundle: alt_bundle,
        })
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Stale);
    // Original route is preserved unchanged
    assert_eq!(s.route().unwrap().route_key, rk);
    assert_eq!(*s.active_l3_weights(), Weights::default());

    // Next tick speaks "この時間内では回避できる経路がありません"
    let o_after = tick(&mut s, 12_100, None, vec![]);
    assert!(has_no_route_voice(&o_after));
}

// ---- 5. Driving (>= 8 km/h for 3 s): override commit succeeds, not FORBIDDEN -----------

#[test]
fn override_commit_succeeds_while_driving_and_supports_undo() {
    let (mut s, orig, rk, req_id, override_id) = ready_override("OVR rc=H dt=T");
    // Drive continuously at >= 8 km/h (222 cm/s) for > 3 s while remaining BEFORE divergence (120_000 cm)
    for (t, x) in [(10_600u64, 60_000i64), (12_200, 75_000), (13_800, 90_000)] {
        tick(&mut s, t, Some(fix_x(&orig, t, x)), vec![]);
    }
    let base = make_facts(&rk.graph_version, 600_000, 1);
    let alt = make_facts(&rk.graph_version, 650_000, 0);
    assert!(matches!(
        s.avoid_offer_eval(&AvoidOfferEvalRequest {
            req_id: req_id.clone(),
            now_mono_ms: 13_900,
            alt_facts: alt,
            base_facts: base,
        }),
        AvoidOfferEvalResult::OverrideOffered { .. }
    ));
    tick(&mut s, 14_000, None, vec![]);

    // Consent via dictionary "はい" while driving
    let o_yes = tick(
        &mut s,
        14_500,
        None,
        vec![json!({
            "at_mono_ms": 14_500,
            "kind": "asr_result",
            "utterance_id": "u-yes",
            "text_class": "dict",
            "dict_norm": "はい"
        })],
    );
    let undo_id = match &o_yes.planner_cmds[..] {
        [PlannerCmd::Commit {
            req_id: r,
            route_key,
            undo_req_id: Some(u),
        }] => {
            assert_eq!(*r, req_id);
            assert_eq!(*route_key, rk);
            u.clone()
        }
        other => panic!("expected commit with undo_req_id, got {other:?}"),
    };

    // dc_route_commit succeeds while driving (NOT E_FORBIDDEN_WHILE_DRIVING!)
    let alt_bundle = alt_line(&orig);
    let committed = s
        .route_commit(&RouteCommitRequest {
            req_id,
            expected_route_key: Some(rk.clone()),
            bundle: alt_bundle,
        })
        .expect("override commit must succeed while driving");
    assert_eq!(committed.route_key.route_revision, U64Str(2));
    assert_eq!(s.active_l3_weights().crossing_right, Some(Level::High));
    assert_eq!(s.active_detour(), Detour::Tight);

    // Align on alt route BEFORE divergence (x = 95_000 -> alt along 45_000 < 70_000) and undo within 5 s
    tick(&mut s, 15_000, Some(fix_x(&orig, 15_000, 95_000)), vec![]);
    let o_undo = tick(
        &mut s,
        16_000,
        None,
        vec![json!({
            "at_mono_ms": 16_000,
            "kind": "user_tap",
            "target_id": override_id,
            "action": "undo"
        })],
    );
    assert_eq!(
        o_undo.planner_cmds,
        vec![PlannerCmd::Commit {
            req_id: undo_id.clone(),
            route_key: committed.route_key.clone(),
            undo_req_id: None,
        }]
    );
    let restored = s
        .route_commit(&RouteCommitRequest {
            req_id: undo_id,
            expected_route_key: Some(committed.route_key),
            bundle: orig,
        })
        .unwrap();
    assert_eq!(restored.route_key.route_revision, U64Str(3));
    assert_eq!(*s.active_l3_weights(), Weights::default());
    assert_eq!(s.active_detour(), Detour::Standard);
}

// ---- 6. Supersede, CXL cancel_commit, priority ordering --------------------------------

#[test]
fn override_priority_order_and_supersede_and_cancel_commit() {
    assert!(SearchPriority::Reroute > SearchPriority::ExplicitPlan);
    assert!(SearchPriority::ExplicitPlan > SearchPriority::Override);
    assert!(SearchPriority::Override > SearchPriority::AvoidChip);
    assert!(SearchPriority::AvoidChip > SearchPriority::Comparison);

    // Superseding a shown confirm dismisses the old one with DismissReason::Superseded
    let (mut s, _orig, rk, req_id, old_override_id) = ready_override("OVR rc=H");
    let base = make_facts(&rk.graph_version, 600_000, 1);
    let alt = make_facts(&rk.graph_version, 600_000, 0);
    s.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id: req_id.clone(),
        now_mono_ms: 10_600,
        alt_facts: alt,
        base_facts: base,
    });
    tick(&mut s, 10_700, None, vec![]);

    let res2 = s
        .validate_override(ovr_req("OVR nr=H", 11_000, 0, json!({"route_key": rk})))
        .unwrap();
    assert!(matches!(res2, ValidateOverrideResult::Applied { .. }));
    let o_sup = tick(&mut s, 11_100, None, vec![]);
    assert!(o_sup.ui_cmds.contains(&UiCmd::Dismiss {
        target_id: old_override_id,
        reason: DismissReason::Superseded,
    }));

    // Undo after consent before dc_route_commit -> cancel_commit{req_id}
    let (mut s2, _orig2, rk2, req_id2, oid2) = ready_override("OVR rc=H");
    let base2 = make_facts(&rk2.graph_version, 600_000, 1);
    let alt2 = make_facts(&rk2.graph_version, 600_000, 0);
    s2.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id: req_id2.clone(),
        now_mono_ms: 10_600,
        alt_facts: alt2,
        base_facts: base2,
    });
    tick(&mut s2, 10_700, None, vec![]);
    tick(
        &mut s2,
        11_000,
        None,
        vec![json!({
            "at_mono_ms": 11_000,
            "kind": "user_tap",
            "target_id": oid2,
            "action": "confirm"
        })],
    );
    let cxl_res = s2
        .validate_override(ovr_req("CXL", 12_000, 0, json!({})))
        .unwrap();
    assert_eq!(
        cxl_res,
        ValidateOverrideResult::Undone {
            req_id: req_id2.clone()
        }
    );
    let o_cxl = tick(&mut s2, 12_100, None, vec![]);
    assert_eq!(
        o_cxl.planner_cmds,
        vec![PlannerCmd::CancelCommit { req_id: req_id2 }]
    );

    // Off-route withdraws a confirming override with DismissReason::RouteChanged
    let (mut s3, orig3, rk3, req_id3, oid3) = ready_override("OVR rc=H");
    let base3 = make_facts(&rk3.graph_version, 600_000, 1);
    let alt3 = make_facts(&rk3.graph_version, 600_000, 0);
    s3.avoid_offer_eval(&AvoidOfferEvalRequest {
        req_id: req_id3,
        now_mono_ms: 10_600,
        alt_facts: alt3,
        base_facts: base3,
    });
    tick(&mut s3, 10_700, None, vec![]);
    let mut dismissed = false;
    for (i, t) in [11_000u64, 12_000, 13_000].into_iter().enumerate() {
        let o = tick(
            &mut s3,
            t,
            Some(fix_xy(&orig3, t, START_X + 1_000 * i as i64, 10_000)),
            vec![],
        );
        if o.ui_cmds.contains(&UiCmd::Dismiss {
            target_id: oid3.clone(),
            reason: DismissReason::RouteChanged,
        }) {
            dismissed = true;
        }
    }
    assert!(dismissed);
    assert_eq!(s3.horizon_status(13_000), Some(HorizonStatus::Rerouting));
    // Verify Arrow import used (suppress unused warning if needed)
    assert_eq!(orig3.legs[0].maneuvers[1].arrow, Arrow::Right);
}

// ---- 7. Codex review regressions: ASR-bound consent & jp=next junction deadline --------

#[test]
fn dict_yes_does_not_transfer_when_confirm_replaced_during_asr() {
    let (mut s, _orig, rk, req_id1, _oid1) = ready_override("OVR rc=H");
    let base = make_facts(&rk.graph_version, 600_000, 1);
    let alt = make_facts(&rk.graph_version, 600_000, 0);
    assert!(matches!(
        s.avoid_offer_eval(&AvoidOfferEvalRequest {
            req_id: req_id1,
            now_mono_ms: 10_600,
            alt_facts: alt.clone(),
            base_facts: base.clone(),
        }),
        AvoidOfferEvalResult::OverrideOffered { .. }
    ));
    tick(&mut s, 10_700, None, vec![]);

    // User starts saying "はい" while ovr-1 is on screen
    tick(
        &mut s,
        11_000,
        None,
        vec![json!({"at_mono_ms": 11_000, "kind": "asr_started", "utterance_id": "u-yes-1"})],
    );

    // Before ASR finishes, ovr-1 is superseded by ovr-2
    assert!(matches!(
        s.validate_override(ovr_req("OVR nr=H", 11_100, 0, json!({"route_key": rk}))),
        Ok(ValidateOverrideResult::Applied { .. })
    ));
    let o_start2 = tick(&mut s, 11_200, None, vec![]);
    let req_id2 = o_start2
        .planner_cmds
        .iter()
        .find_map(|c| match c {
            PlannerCmd::Start {
                req_id,
                priority: SearchPriority::Override,
                ..
            } => Some(req_id.clone()),
            _ => None,
        })
        .expect("override 2 start");
    tick(
        &mut s,
        11_400,
        None,
        vec![search_finished_ev(11_400, &req_id2, &rk)],
    );
    assert!(matches!(
        s.avoid_offer_eval(&AvoidOfferEvalRequest {
            req_id: req_id2.clone(),
            now_mono_ms: 11_500,
            alt_facts: alt,
            base_facts: base,
        }),
        AvoidOfferEvalResult::OverrideOffered { .. }
    ));
    tick(&mut s, 11_600, None, vec![]);

    // Delayed "はい" from u-yes-1 must NOT commit ovr-2
    let o_stale_yes = tick(
        &mut s,
        11_700,
        None,
        vec![json!({
            "at_mono_ms": 11_700,
            "kind": "asr_result",
            "utterance_id": "u-yes-1",
            "text_class": "dict",
            "dict_norm": "はい"
        })],
    );
    assert!(o_stale_yes.planner_cmds.is_empty());
    assert!(o_stale_yes
        .log
        .iter()
        .any(|l| l.what == "override_consent_refused"
            && l.detail.as_deref() == Some("proposal_changed")));
    assert!(matches!(
        s.override_state(),
        OverrideState::Confirming { .. }
    ));

    // A fresh utterance started while ovr-2 is shown DOES commit ovr-2
    let o_fresh_yes = tick(
        &mut s,
        12_000,
        None,
        vec![
            json!({"at_mono_ms": 11_900, "kind": "asr_started", "utterance_id": "u-yes-2"}),
            json!({
                "at_mono_ms": 12_000,
                "kind": "asr_result",
                "utterance_id": "u-yes-2",
                "text_class": "dict",
                "dict_norm": "はい"
            }),
        ],
    );
    assert!(o_fresh_yes
        .planner_cmds
        .iter()
        .any(|c| matches!(c, PlannerCmd::Commit { req_id, .. } if *req_id == req_id2)));
}

#[test]
fn jp_next_junction_deadline_expires_confirm_before_30s() {
    let orig = main_line();
    let (mut s, rk) = session_on(&orig);
    // Position at 2.0 km (500 m = ~33.3 s before the 2.5 km right turn -> earliest ~28.3 s,
    // so decision deadline is earliest - 20 s ≈ +8.3 s, well before the 30 s override expiry)
    let o1 = tick(&mut s, 10_000, Some(fix_x(&orig, 10_000, 200_000)), vec![]);
    let h1 = o1.horizon.unwrap();
    let jp_deadline = h1.refs.next.as_ref().unwrap().deadline_mono_ms;
    assert!(
        jp_deadline > 10_000 && jp_deadline < 40_000,
        "{jp_deadline}"
    );

    let ValidateOverrideResult::Applied { override_id, .. } = s
        .validate_override(ovr_req(
            "OVR jp=next",
            10_000,
            h1.frame_seq,
            json!({"route_key": rk}),
        ))
        .unwrap()
    else {
        panic!("expected Applied");
    };
    let o2 = tick(&mut s, 10_100, None, vec![]);
    let req_id = match &o2.planner_cmds[..] {
        [PlannerCmd::Start {
            req_id,
            priority: SearchPriority::Override,
            ..
        }] => req_id.clone(),
        other => panic!("unexpected cmds: {other:?}"),
    };
    tick(
        &mut s,
        10_500,
        None,
        vec![search_finished_ev(10_500, &req_id, &rk)],
    );
    let base = make_facts(&rk.graph_version, 600_000, 2);
    let alt = make_facts(&rk.graph_version, 600_000, 0);
    assert!(matches!(
        s.avoid_offer_eval(&AvoidOfferEvalRequest {
            req_id,
            now_mono_ms: 10_600,
            alt_facts: alt,
            base_facts: base,
        }),
        AvoidOfferEvalResult::OverrideOffered { .. }
    ));
    let o_show = tick(&mut s, 10_700, None, vec![]);
    assert_eq!(o_show.next_wake_mono_ms, Some(jp_deadline));

    // At jp_deadline (well before 40_000): dismissed as expired and late tap is refused
    let o_exp = tick(
        &mut s,
        jp_deadline,
        None,
        vec![json!({
            "at_mono_ms": jp_deadline,
            "kind": "user_tap",
            "target_id": override_id,
            "action": "confirm"
        })],
    );
    assert_eq!(
        o_exp.ui_cmds,
        vec![UiCmd::Dismiss {
            target_id: override_id,
            reason: DismissReason::Expired,
        }]
    );
    assert!(o_exp.planner_cmds.is_empty());
    assert_eq!(*s.override_state(), OverrideState::Idle);
}

#[test]
fn consent_with_third_off_route_fix_in_same_tick_is_refused() {
    // Both user_tap{confirm} and dict "はい" arriving in the same tick as the 3rd off-route
    // fix must be refused ("not_tracking"), never emitting a Commit for the old override.
    for use_dict_yes in [false, true] {
        let (mut s, orig, rk, req_id, oid) = ready_override("OVR rc=H");
        let base = make_facts(&rk.graph_version, 600_000, 1);
        let alt = make_facts(&rk.graph_version, 600_000, 0);
        assert!(matches!(
            s.avoid_offer_eval(&AvoidOfferEvalRequest {
                req_id,
                now_mono_ms: 10_600,
                alt_facts: alt,
                base_facts: base,
            }),
            AvoidOfferEvalResult::OverrideOffered { .. }
        ));
        tick(&mut s, 10_700, None, vec![]);

        // Two off-route fixes: still tracking (confirm_fixes == 3)
        tick(
            &mut s,
            11_000,
            Some(fix_xy(&orig, 11_000, START_X + 1_000, 10_000)),
            vec![],
        );
        tick(
            &mut s,
            12_000,
            Some(fix_xy(&orig, 12_000, START_X + 2_000, 10_000)),
            vec![],
        );
        assert_eq!(s.horizon_status(12_000), Some(HorizonStatus::Tracking));

        let evs = if use_dict_yes {
            vec![
                json!({"at_mono_ms": 12_900, "kind": "asr_started", "utterance_id": "u-off"}),
                json!({
                    "at_mono_ms": 13_000,
                    "kind": "asr_result",
                    "utterance_id": "u-off",
                    "text_class": "dict",
                    "dict_norm": "はい"
                }),
            ]
        } else {
            vec![json!({
                "at_mono_ms": 13_000,
                "kind": "user_tap",
                "target_id": oid,
                "action": "confirm"
            })]
        };
        // 3rd off-route fix arrives in the SAME tick as consent
        let o = tick(
            &mut s,
            13_000,
            Some(fix_xy(&orig, 13_000, START_X + 3_000, 10_000)),
            evs,
        );
        assert!(
            !o.planner_cmds
                .iter()
                .any(|c| matches!(c, PlannerCmd::Commit { .. })),
            "must not emit Commit on off-route tick: {:?}",
            o.planner_cmds
        );
        assert!(o.planner_cmds.iter().any(|c| matches!(
            c,
            PlannerCmd::Start {
                priority: SearchPriority::Reroute,
                ..
            }
        )));
        assert!(o
            .log
            .iter()
            .any(|l| l.what == "override_consent_refused"
                && l.detail.as_deref() == Some("not_tracking")));
        assert!(o.ui_cmds.contains(&UiCmd::Dismiss {
            target_id: oid,
            reason: DismissReason::RouteChanged,
        }));
        assert_eq!(*s.override_state(), OverrideState::Idle);
        assert_eq!(s.issued_commits().count(), 0);
    }
}

#[test]
fn override_commit_retains_divergence_and_weights_after_expired_undo() {
    // Case A: expired undo (> 5 s) while override commit is unsettled still installs new
    // L3 weights and detour when dc_route_commit arrives before divergence.
    let (mut s, orig, rk, req_id, oid) = ready_override("OVR rc=H dt=T");
    let base = make_facts(&rk.graph_version, 600_000, 1);
    let alt = make_facts(&rk.graph_version, 600_000, 0);
    assert!(matches!(
        s.avoid_offer_eval(&AvoidOfferEvalRequest {
            req_id: req_id.clone(),
            now_mono_ms: 10_600,
            alt_facts: alt.clone(),
            base_facts: base.clone(),
        }),
        AvoidOfferEvalResult::OverrideOffered { .. }
    ));
    tick(&mut s, 10_700, None, vec![]);
    tick(
        &mut s,
        12_000,
        None,
        vec![json!({
            "at_mono_ms": 12_000,
            "kind": "user_tap",
            "target_id": oid,
            "action": "confirm"
        })],
    );
    // > 5 s later: undo attempt expires and clears self.undo, leaving commit unsettled
    let expired_undo = s
        .validate_override(ovr_req("CXL", 18_000, 0, json!({})))
        .unwrap();
    assert_eq!(
        expired_undo,
        ValidateOverrideResult::Rejected {
            code: RejectCode::NoPendingTarget
        }
    );
    let alt_bundle = alt_line(&orig);
    let committed = s
        .route_commit(&RouteCommitRequest {
            req_id,
            expected_route_key: Some(rk),
            bundle: alt_bundle,
        })
        .expect("override commit before divergence must still succeed");
    assert_eq!(committed.route_key.route_revision, U64Str(2));
    assert_eq!(s.active_l3_weights().crossing_right, Some(Level::High));
    assert_eq!(s.active_detour(), Detour::Tight);

    // Case B: expired undo (> 5 s) while override commit is unsettled STILL enforces the
    // divergence-point refusal when the vehicle has already passed divergence.
    let (mut s2, orig2, rk2, req_id2, oid2) = ready_override("OVR rc=H");
    assert!(matches!(
        s2.avoid_offer_eval(&AvoidOfferEvalRequest {
            req_id: req_id2.clone(),
            now_mono_ms: 10_600,
            alt_facts: alt,
            base_facts: base,
        }),
        AvoidOfferEvalResult::OverrideOffered { .. }
    ));
    tick(&mut s2, 10_700, None, vec![]);
    tick(
        &mut s2,
        12_000,
        None,
        vec![json!({
            "at_mono_ms": 12_000,
            "kind": "user_tap",
            "target_id": oid2,
            "action": "confirm"
        })],
    );
    let expired_undo2 = s2
        .validate_override(ovr_req("CXL", 18_000, 0, json!({})))
        .unwrap();
    assert_eq!(
        expired_undo2,
        ValidateOverrideResult::Rejected {
            code: RejectCode::NoPendingTarget
        }
    );
    // Vehicle advances past divergence (x = 130_000 > 120_000) before dc_route_commit
    tick(
        &mut s2,
        18_500,
        Some(fix_x(&orig2, 18_500, 130_000)),
        vec![],
    );
    let alt_bundle2 = alt_line(&orig2);
    let err = s2
        .route_commit(&RouteCommitRequest {
            req_id: req_id2,
            expected_route_key: Some(rk2.clone()),
            bundle: alt_bundle2,
        })
        .expect_err("must still refuse override commit past divergence after expired undo");
    assert_eq!(err.code, ErrorCode::Stale);
    assert_eq!(s2.route().unwrap().route_key, rk2);
    assert_eq!(*s2.active_l3_weights(), Weights::default());
    let o_voice = tick(&mut s2, 18_600, None, vec![]);
    assert!(has_no_route_voice(&o_voice));
}
