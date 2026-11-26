//! Unit and integration tests for `drive_core::rank` (`dc_rank`, 03 §3.2, `8b833a1`).

use contracts::facts::{CandidateFacts, FactCounts, FactLabel, NarrowMeters};
use contracts::override_types::{Detour, Level, Weights};
use contracts::rank::{
    DetourPolicyConfig, L3LevelScale, PenaltyPolicy, RankDropReason, RankPolicy, RankProfile,
    RankRequest,
};
use contracts::ErrorCode;
use drive_core::rank::{combined_weight_permille, distance_penalty_ms, rank};

fn base_fact(id: &str, fp: &str, eta_ms: u64) -> CandidateFacts {
    CandidateFacts {
        candidate_id: id.into(),
        label: FactLabel::CostingVariant,
        plan_request_id: "p1".into(),
        graph_version: "jp-2026.11.0".into(),
        fingerprint: fp.into(),
        legal: true,
        complete: true,
        eta_ms,
        length_m: 5_000,
        counts: FactCounts {
            right_unprotected: 0,
            right_signal_unknown_protection: 0,
            right_signal_unknown: 0,
            right_protected: 0,
            right_entry: 0,
            right_entry_unknown: 0,
            merges: 0,
        },
        narrow_m: NarrowMeters {
            confirmed: 0,
            inferred: 0,
        },
        unknown_width_residential_m: 0,
        unknown_length_m: 0,
        junctions: Vec::new(),
        through_anchors: Vec::new(),
    }
}

/// Arrival right-entry penalty (+90 s) replaces the arrival maneuver's right-turn penalty
/// (not +45 s + 90 s = +135 s), and unknown destination entry side adds +45 s.
#[test]
fn right_entry_replaces_arrival_right_turn_penalty() {
    // Route with 1 normal unprotected right turn -> +45_000 ms
    let mut normal_rt = base_fact("c-normal", "fp-1", 600_000);
    normal_rt.label = FactLabel::Baseline;
    normal_rt.counts.right_unprotected = 1;

    // Route where that single crossing maneuver is the arrival right entry (mutually exclusive
    // bucket per 04 §2.2 / junction::classify) -> +90_000 ms (replaces +45_000 ms, NOT +135_000 ms)
    let mut arrival_entry = base_fact("c-entry", "fp-2", 600_000);
    arrival_entry.counts.right_entry = 1;

    // Route with 1 intermediate unprotected right turn AND 1 arrival right entry -> 45_000 + 90_000 = 135_000 ms
    let mut both = base_fact("c-both", "fp-3", 600_000);
    both.counts.right_unprotected = 1;
    both.counts.right_entry = 1;

    // Route with unknown destination entry side -> +45_000 ms
    let mut unknown_entry = base_fact("c-unk-entry", "fp-4", 600_000);
    unknown_entry.counts.right_entry_unknown = 1;

    let res = rank(&RankRequest {
        facts: vec![normal_rt, arrival_entry, both, unknown_entry],
        policy: RankPolicy::default(),
        profile: RankProfile::default(),
    })
    .unwrap();

    let by_id = |id: &str| res.ranked.iter().find(|r| r.candidate_id == id).expect(id);
    assert_eq!(by_id("c-normal").penalty_ms, 45_000);
    assert_eq!(by_id("c-entry").penalty_ms, 90_000);
    assert_eq!(by_id("c-entry").score_ms, 690_000);
    assert_eq!(by_id("c-both").penalty_ms, 135_000);
    assert_eq!(by_id("c-unk-entry").penalty_ms, 45_000);
}

/// L2 × L3 / 1000 weight combinations across all 3 axes (right_turn / narrow_road / merge),
/// with L3 low=500, normal=1000, high=2000 (03 §3.2 / 07 §1).
#[test]
fn l2_times_l3_weights_across_all_axes() {
    let scale = L3LevelScale::default();
    assert_eq!(combined_weight_permille(1_000, None, &scale), 1_000);
    assert_eq!(
        combined_weight_permille(1_000, Some(Level::Low), &scale),
        500
    );
    assert_eq!(
        combined_weight_permille(1_000, Some(Level::Normal), &scale),
        1_000
    );
    assert_eq!(
        combined_weight_permille(1_000, Some(Level::High), &scale),
        2_000
    );
    assert_eq!(
        combined_weight_permille(1_500, Some(Level::High), &scale),
        3_000
    );
    assert_eq!(
        combined_weight_permille(1_500, Some(Level::Low), &scale),
        750
    );
    assert_eq!(combined_weight_permille(750, Some(Level::Low), &scale), 375);

    let mut cand = base_fact("c1", "fp-1", 500_000);
    cand.label = FactLabel::Baseline;
    cand.counts.right_unprotected = 1; // 45_000
    cand.counts.right_signal_unknown_protection = 1; // 25_000
    cand.counts.right_signal_unknown = 1; // 45_000
    cand.counts.right_protected = 1; // 10_000
                                     // right raw = 125_000
    cand.narrow_m.confirmed = 100; // 30_000
    cand.narrow_m.inferred = 100; // +30_000 -> 60_000
    cand.unknown_width_residential_m = 200; // 30_000 (separate from narrow_m)
                                            // narrow raw = 90_000
    cand.counts.merges = 2; // 30_000

    let profile = RankProfile {
        right_turn_penalty_permille: 1_500,
        narrow_road_penalty_permille: 1_200,
        merge_penalty_permille: 800,
        max_detour_ratio_permille: None,
        l3_weights: Weights {
            crossing_right: Some(Level::High), // 1500 * 2000 / 1000 = 3000 -> 125_000 * 3 = 375_000
            narrow_road: Some(Level::Low),     // 1200 * 500 / 1000 = 600   -> 90_000 * 0.6 = 54_000
            merge: Some(Level::Normal),        // 800 * 1000 / 1000 = 800   -> 30_000 * 0.8 = 24_000
        },
        detour: Detour::Standard,
    };

    let res = rank(&RankRequest {
        facts: vec![cand],
        policy: RankPolicy::default(),
        profile,
    })
    .unwrap();

    assert_eq!(res.ranked.len(), 1);
    let r = &res.ranked[0];
    assert_eq!(r.penalty_ms, 375_000 + 54_000 + 24_000);
    assert_eq!(r.score_ms, 500_000 + 453_000);
}

/// Detour cap drops candidates exceeding the cap, but baseline is always retained.
/// Covers standard, tight (dt=T), and user max_detour_ratio tightening + 1.35 cap.
#[test]
fn detour_cap_drops_excessive_detours_but_keeps_baseline() {
    // base_eta = 600_000 ms (10 min, < 30 min short threshold):
    // standard cap = min(180_000, 600_000 * 350 / 1000 = 210_000) = 180_000 ms.
    let mut base = base_fact("c-base", "fp-base", 600_000);
    base.label = FactLabel::Baseline;
    base.counts.right_unprotected = 4; // heavy penalty (+180_000)

    let at_cap = base_fact("c-at-cap", "fp-at", 780_000); // delta = 180_000 <= 180_000 (kept, wins on score!)
    let over_cap = base_fact("c-over-cap", "fp-over", 780_001); // delta = 180_001 > 180_000 (dropped)

    let res = rank(&RankRequest {
        facts: vec![base.clone(), at_cap.clone(), over_cap.clone()],
        policy: RankPolicy::default(),
        profile: RankProfile::default(),
    })
    .unwrap();

    assert_eq!(res.baseline_id.as_deref(), Some("c-base"));
    assert_eq!(res.base_eta_ms, Some(600_000));
    let ids: Vec<&str> = res.ranked.iter().map(|r| r.candidate_id.as_str()).collect();
    // Both have score 780_000; tie-break by ETA puts c-base (600_000) ahead of c-at-cap (780_000)
    assert_eq!(ids, vec!["c-base", "c-at-cap"]);
    assert_eq!(res.dropped.len(), 1);
    assert_eq!(res.dropped[0].candidate_id, "c-over-cap");
    assert_eq!(res.dropped[0].reason, RankDropReason::OverDetourCap);

    // Tight detour (dt=T): cap = 180_000 * 500 / 1000 = 90_000 ms -> both c-at-cap & c-over-cap dropped,
    // baseline MUST still remain!
    let tight_res = rank(&RankRequest {
        facts: vec![base.clone(), at_cap.clone(), over_cap.clone()],
        policy: RankPolicy::default(),
        profile: RankProfile {
            detour: Detour::Tight,
            ..Default::default()
        },
    })
    .unwrap();
    assert_eq!(tight_res.ranked.len(), 1);
    assert_eq!(tight_res.ranked[0].candidate_id, "c-base");
    assert!(tight_res.ranked[0].is_baseline);
    assert_eq!(tight_res.dropped.len(), 2);

    // User max_detour_ratio_permille = 1100 (1.10 -> +60_000 ms) tightens cap;
    // max allowed 1350 matches policy cap; values outside 1000..=1350 are rejected with E_SCHEMA.
    let mid = base_fact("c-mid", "fp-mid", 670_000); // delta = 70_000
    let user_tight = rank(&RankRequest {
        facts: vec![base.clone(), mid.clone()],
        policy: RankPolicy::default(),
        profile: RankProfile {
            max_detour_ratio_permille: Some(1_100),
            ..Default::default()
        },
    })
    .unwrap();
    assert_eq!(user_tight.ranked.len(), 1);
    assert_eq!(user_tight.ranked[0].candidate_id, "c-base");
    assert_eq!(user_tight.dropped[0].reason, RankDropReason::OverDetourCap);

    // At 1350 (upper boundary), still accepted and does not loosen policy cap
    let user_max_valid = rank(&RankRequest {
        facts: vec![base.clone(), over_cap.clone()],
        policy: RankPolicy::default(),
        profile: RankProfile {
            max_detour_ratio_permille: Some(1_350),
            ..Default::default()
        },
    })
    .unwrap();
    assert_eq!(user_max_valid.ranked.len(), 1);
    assert_eq!(
        user_max_valid.dropped[0].reason,
        RankDropReason::OverDetourCap
    );

    // Out-of-range max_detour_ratio_permille (999 or 1351) -> E_SCHEMA
    for bad_ratio in [999, 1_351] {
        let err = rank(&RankRequest {
            facts: vec![base.clone()],
            policy: RankPolicy::default(),
            profile: RankProfile {
                max_detour_ratio_permille: Some(bad_ratio),
                ..Default::default()
            },
        })
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Schema, "ratio {bad_ratio}");
    }
}

/// All 3 tie-break stages when `score_ms` is tied (03 §3.2):
/// 1. ETA (asc) -> 2. unknown_length_m (asc) -> 3. fingerprint (lex asc).
#[test]
fn tiebreak_all_three_stages() {
    // All 4 candidates have score_ms = 645_000:
    // - c-eta-win: eta 600_000 + 1 right_unprotected (45_000) = 645_000, unknown 999, fp "fp-z"
    // - c-unk-win: eta 645_000 + 0 = 645_000, unknown 50, fp "fp-z"
    // - c-fp-a:    eta 645_000 + 0 = 645_000, unknown 200, fp "fp-a"
    // - c-fp-b:    eta 645_000 + 0 = 645_000, unknown 200, fp "fp-b"
    let mut c_eta_win = base_fact("c-eta-win", "fp-z", 600_000);
    c_eta_win.label = FactLabel::Baseline;
    c_eta_win.counts.right_unprotected = 1;
    c_eta_win.unknown_length_m = 999;

    let mut c_unk_win = base_fact("c-unk-win", "fp-y", 645_000);
    c_unk_win.unknown_length_m = 50;

    let mut c_fp_b = base_fact("c-fp-b", "fp-b", 645_000);
    c_fp_b.unknown_length_m = 200;

    let mut c_fp_a = base_fact("c-fp-a", "fp-a", 645_000);
    c_fp_a.unknown_length_m = 200;

    // Deliberately pass in reverse order to verify sorting
    let res = rank(&RankRequest {
        facts: vec![c_fp_b, c_fp_a, c_unk_win, c_eta_win],
        policy: RankPolicy::default(),
        profile: RankProfile::default(),
    })
    .unwrap();

    let order: Vec<(&str, u32, u64)> = res
        .ranked
        .iter()
        .map(|r| (r.candidate_id.as_str(), r.rank, r.score_ms))
        .collect();
    assert_eq!(
        order,
        vec![
            ("c-eta-win", 1, 645_000),
            ("c-unk-win", 2, 645_000),
            ("c-fp-a", 3, 645_000),
            ("c-fp-b", 4, 645_000),
        ]
    );
}

/// Display 3 axes report raw, unweighted counts and distances, and ETA is never altered
/// by comfort penalties or L2/L3 weights (03 §3.2 item 5).
#[test]
fn display_three_axes_are_raw_and_unweighted() {
    let mut base = base_fact("c1", "fp-1", 600_000);
    base.label = FactLabel::Baseline;

    let mut alt = base_fact("c2", "fp-2", 630_000);
    alt.counts.right_unprotected = 2;
    alt.counts.right_signal_unknown_protection = 1;
    alt.counts.right_signal_unknown = 3;
    alt.counts.right_protected = 4;
    alt.counts.right_entry = 1;
    alt.counts.right_entry_unknown = 1;
    alt.narrow_m.confirmed = 123;
    alt.narrow_m.inferred = 45;
    alt.unknown_width_residential_m = 67;

    let res = rank(&RankRequest {
        facts: vec![base, alt],
        policy: RankPolicy::default(),
        profile: RankProfile {
            right_turn_penalty_permille: 2_500,
            narrow_road_penalty_permille: 3_000,
            l3_weights: Weights {
                crossing_right: Some(Level::High),
                narrow_road: Some(Level::High),
                merge: None,
            },
            ..Default::default()
        },
    })
    .unwrap();

    let d = &res
        .ranked
        .iter()
        .find(|r| r.candidate_id == "c2")
        .unwrap()
        .display;
    assert_eq!(d.eta_ms, 630_000);
    assert_eq!(d.delta_eta_ms, 30_000);
    assert_eq!(d.right_turns.unprotected, 2);
    assert_eq!(d.right_turns.signal_unknown_protection, 1);
    assert_eq!(d.right_turns.signal_unknown, 3);
    assert_eq!(d.right_turns.protected, 4);
    assert_eq!(d.right_turns.right_entry, 1);
    assert_eq!(d.right_turns.right_entry_unknown, 1);
    assert_eq!(d.narrow.confirmed_m, 123);
    assert_eq!(d.narrow.inferred_m, 45);
    assert_eq!(d.narrow.unknown_width_residential_m, 67);
}

/// Integer truncation boundaries:
/// - `penalty_ms_per_100m * length_cm / 10_000` (floor)
/// - `unknown_width_residential_m` counted separately from `narrow_m`
/// - `L2 * L3 / 1000` (floor)
/// - `axis_raw_penalty_ms * weight_permille / 1000` (floor)
#[test]
fn integer_truncation_boundaries() {
    // 33_333 ms/100m * 100 cm / 10_000 = 333.33 -> 333
    assert_eq!(distance_penalty_ms(33_333, 1), 333);
    // 15_001 ms/100m * 300 cm / 10_000 = 450.03 -> 450
    assert_eq!(distance_penalty_ms(15_001, 3), 450);
    // 99 ms/100m * 100 cm / 10_000 = 0.99 -> 0 (truncates, never rounds up)
    assert_eq!(distance_penalty_ms(99, 1), 0);

    // L2=333 * L3=500 / 1000 = 166.5 -> 166
    let scale = L3LevelScale::default();
    assert_eq!(combined_weight_permille(333, Some(Level::Low), &scale), 166);

    // Separate truncation for narrow_m vs unknown_width_residential_m:
    // Use 33_333 ms/100m for narrow (1 m -> 333 ms) and 15_099 ms/100m for residential (1 m -> 150 ms)
    // Sum = 483 ms; apply weight 166 -> 483 * 166 / 1000 = 80_178 / 1000 = 80 ms (not 81!)
    let mut cand = base_fact("c1", "fp-1", 100_000);
    cand.label = FactLabel::Baseline;
    cand.narrow_m.confirmed = 1;
    cand.unknown_width_residential_m = 1;

    let policy = RankPolicy {
        detour: DetourPolicyConfig::default(),
        penalties_ms: PenaltyPolicy {
            narrow_ms_per_100m: 33_333,
            unknown_width_residential_ms_per_100m: 15_099,
            ..Default::default()
        },
        l3_level_permille: L3LevelScale::default(),
        expected_graph_version: None,
    };
    let profile = RankProfile {
        narrow_road_penalty_permille: 333,
        l3_weights: Weights {
            crossing_right: None,
            narrow_road: Some(Level::Low), // weight = 333 * 500 / 1000 = 166
            merge: None,
        },
        ..Default::default()
    };

    let res = rank(&RankRequest {
        facts: vec![cand],
        policy,
        profile,
    })
    .unwrap();
    assert_eq!(res.ranked[0].penalty_ms, 80);
    assert_eq!(res.ranked[0].score_ms, 100_080);
}

/// Filtering (illegal, incomplete, graph_version mismatch), duplicate fingerprint removal,
/// empty input, and 0-weight rejection.
#[test]
fn filtering_dedup_empty_and_validation() {
    let mut base = base_fact("c-base", "fp-1", 600_000);
    base.label = FactLabel::Baseline;

    let mut illegal = base_fact("c-illegal", "fp-2", 500_000);
    illegal.legal = false;

    let mut incomplete = base_fact("c-incomplete", "fp-3", 500_000);
    incomplete.complete = false;

    let mut wrong_gv = base_fact("c-wrong-gv", "fp-4", 500_000);
    wrong_gv.graph_version = "jp-2025.01.0".into();

    let dup_slower = base_fact("c-dup", "fp-1", 610_000);

    let res = rank(&RankRequest {
        facts: vec![base, illegal, incomplete, wrong_gv, dup_slower],
        policy: RankPolicy::default(),
        profile: RankProfile::default(),
    })
    .unwrap();
    assert_eq!(res.ranked.len(), 1);
    assert_eq!(res.ranked[0].candidate_id, "c-base");
    let drop_reasons: Vec<(&str, RankDropReason)> = res
        .dropped
        .iter()
        .map(|d| (d.candidate_id.as_str(), d.reason))
        .collect();
    assert_eq!(
        drop_reasons,
        vec![
            ("c-illegal", RankDropReason::NotLegal),
            ("c-incomplete", RankDropReason::NotComplete),
            ("c-wrong-gv", RankDropReason::GraphVersionMismatch),
            ("c-dup", RankDropReason::DuplicateFingerprint),
        ]
    );

    // Empty input
    let empty = rank(&RankRequest {
        facts: vec![],
        policy: RankPolicy::default(),
        profile: RankProfile::default(),
    })
    .unwrap();
    assert!(empty.baseline_id.is_none());
    assert!(empty.base_eta_ms.is_none());
    assert!(empty.ranked.is_empty());

    // L2 *_penalty_permille boundaries: 250 and 4000 allowed; 0, 249, 4001 -> E_SCHEMA
    for ok_w in [250, 4_000] {
        assert!(rank(&RankRequest {
            facts: vec![],
            policy: RankPolicy::default(),
            profile: RankProfile {
                right_turn_penalty_permille: ok_w,
                narrow_road_penalty_permille: ok_w,
                merge_penalty_permille: ok_w,
                ..Default::default()
            },
        })
        .is_ok());
    }
    for bad_w in [0, 249, 4_001] {
        let err = rank(&RankRequest {
            facts: vec![],
            policy: RankPolicy::default(),
            profile: RankProfile {
                right_turn_penalty_permille: bad_w,
                ..Default::default()
            },
        })
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Schema, "weight {bad_w}");
    }
}
