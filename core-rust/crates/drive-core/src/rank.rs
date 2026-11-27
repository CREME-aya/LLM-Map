//! `dc_rank` candidate filtering, 3-axis integer scoring, and deterministic tie-breaking
//! (docs 02 §2.1 #6, 03 §3.2, 04 §2.2, 07 §1).
//!
//! Integer-only (`u64` / `u128`): no `f32` / `f64` anywhere.
//!
//! Formula (03 §3.2, commit `8b833a1`, all numeric defaults provisional in `RankPolicy`):
//! 1. Filter to legal, complete, same-`graph_version` candidates; deduplicate by
//!    `fingerprint`.
//! 2. Pick `baseline_id` as the minimum-ETA accepted candidate (`base_eta_ms`).
//! 3. Drop non-baseline candidates failing `DetourPolicy::within(base_eta_ms, cand.eta_ms,
//!    tight)` — the exact same function used by `dc_avoid_offer_eval`. The baseline
//!    (`baseline_id`) is always retained.
//! 4. Axis weights (per-mille, truncated):
//!    `weight_permille = l2_permille * l3_permille / 1000`
//!    with L3 `low / normal / high = 500 / 1000 / 2000` (default 1000 when omitted).
//! 5. Per-axis raw penalty (ms, truncated):
//!    - Right-turn axis:
//!      `right_unprotected * 45_000`
//!      `+ right_signal_unknown * 45_000`
//!      `+ right_signal_unknown_protection * 25_000`
//!      `+ right_protected * 10_000`
//!      `+ right_entry * 90_000` (replaces the arrival maneuver's right-turn penalty;
//!      `CandidateFacts.counts` buckets `right_entry` exclusively from `right_*` per
//!      `junction::classify` / 04 §2.2, so this is never added on top of a right-turn count)
//!      `+ right_entry_unknown * 45_000` (destination entry direction unknown)
//!    - Narrow-road axis (distance in cm = meters * 100; `penalty_ms_per_100m * length_cm / 10_000`):
//!      `narrow_ms_per_100m * ((narrow_m.confirmed + narrow_m.inferred) * 100) / 10_000`
//!      `+ unknown_width_residential_ms_per_100m * (unknown_width_residential_m * 100) / 10_000`
//!      (`unknown_width_residential_m` is scored separately from `narrow_m`)
//!    - Merge axis:
//!      `merges * 15_000`
//! 6. Total score (ms, no axis normalization):
//!    `score_ms = eta_ms + Σ (axis_raw_penalty_ms * axis_weight_permille / 1000)` (truncated).
//! 7. Deterministic sort order (all ascending):
//!    `score_ms` -> `eta_ms` -> `unknown_length_m` -> `fingerprint` (lexicographical).
//! 8. Display 3 axes: raw, unweighted counts and distances from `CandidateFacts`
//!    (ETA is never altered by penalties).
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::avoid::DetourPolicy;
use crate::session::CoreError;
use contracts::facts::{CandidateFacts, FactLabel};
use contracts::override_types::{Detour, Level};
use contracts::rank::{
    ComparisonDisplay, DetourPolicyConfig, DroppedCandidate, L3LevelScale, NarrowDisplay,
    PenaltyPolicy, RankDropReason, RankRequest, RankedCandidate, RightTurnDisplay, RouteComparison,
    L2_PENALTY_PERMILLE_MAX, L2_PENALTY_PERMILLE_MIN, MAX_DETOUR_RATIO_PERMILLE_MAX,
    MAX_DETOUR_RATIO_PERMILLE_MIN,
};
use contracts::ErrorCode;
use std::collections::BTreeMap;

/// Maximum user detour ratio (1.35 = 1350 permille; 03 §3.2 / §5 / 07 §1).
pub const USER_MAX_DETOUR_RATIO_CAP_PERMILLE: u64 = 1_350;

/// Build a [`DetourPolicy`] from [`DetourPolicyConfig`] and an optional user
/// `max_detour_ratio_permille` (which only tightens the policy cap and is clamped at 1350).
pub fn effective_detour_policy(
    cfg: &DetourPolicyConfig,
    user_max_ratio_permille: Option<u64>,
) -> DetourPolicy {
    let mut ratio_cap = cfg.ratio_cap_permille;
    if let Some(user_ratio) = user_max_ratio_permille {
        let clamped = user_ratio.min(USER_MAX_DETOUR_RATIO_CAP_PERMILLE);
        ratio_cap = ratio_cap.min(clamped);
    }
    DetourPolicy {
        short_threshold_s: cfg.short_threshold_s,
        short_cap_s: cfg.short_cap_s,
        long_cap_s: cfg.long_cap_s,
        long_cap_ratio_permille: cfg.long_cap_ratio_permille,
        ratio_cap_permille: ratio_cap,
        tight_factor_permille: cfg.tight_factor_permille,
    }
}

/// Combined L2 × L3 axis weight in per-mille (truncated integer division; 03 §3.2).
pub fn combined_weight_permille(
    l2_permille: u64,
    l3_level: Option<Level>,
    l3_scale: &L3LevelScale,
) -> u64 {
    let l3 = match l3_level {
        None => 1_000,
        Some(Level::Low) => l3_scale.low,
        Some(Level::Normal) => l3_scale.normal,
        Some(Level::High) => l3_scale.high,
    };
    let prod = u128::from(l2_permille) * u128::from(l3) / 1_000;
    u64::try_from(prod).unwrap_or(u64::MAX)
}

/// Distance-based penalty in ms using the 03 §3.2 formula:
/// `penalty_ms_per_100m * length_cm / 10_000` (truncated), where `length_cm = length_m * 100`.
pub fn distance_penalty_ms(penalty_ms_per_100m: u64, length_m: u64) -> u64 {
    let length_cm = u128::from(length_m) * 100;
    let pen = u128::from(penalty_ms_per_100m) * length_cm / 10_000;
    u64::try_from(pen).unwrap_or(u64::MAX)
}

/// Per-axis raw (unweighted) penalties in ms for a single candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawAxisPenalties {
    pub right_turn_ms: u64,
    pub narrow_road_ms: u64,
    pub merge_ms: u64,
}

/// Compute per-axis unweighted penalties (ms) for `facts` under `penalties`.
pub fn raw_axis_penalties(facts: &CandidateFacts, penalties: &PenaltyPolicy) -> RawAxisPenalties {
    let c = &facts.counts;
    let right_turn = u128::from(c.right_unprotected) * u128::from(penalties.right_unprotected_ms)
        + u128::from(c.right_signal_unknown) * u128::from(penalties.right_signal_unknown_ms)
        + u128::from(c.right_signal_unknown_protection)
            * u128::from(penalties.right_signal_unknown_protection_ms)
        + u128::from(c.right_protected) * u128::from(penalties.right_protected_ms)
        + u128::from(c.right_entry) * u128::from(penalties.right_entry_ms)
        + u128::from(c.right_entry_unknown) * u128::from(penalties.right_entry_unknown_ms);

    let narrow_total_m = facts
        .narrow_m
        .confirmed
        .saturating_add(facts.narrow_m.inferred);
    let narrow_pen = distance_penalty_ms(penalties.narrow_ms_per_100m, narrow_total_m);
    let res_pen = distance_penalty_ms(
        penalties.unknown_width_residential_ms_per_100m,
        facts.unknown_width_residential_m,
    );
    let narrow_road = u128::from(narrow_pen) + u128::from(res_pen);

    let merge = u128::from(c.merges) * u128::from(penalties.merge_ms);

    RawAxisPenalties {
        right_turn_ms: u64::try_from(right_turn).unwrap_or(u64::MAX),
        narrow_road_ms: u64::try_from(narrow_road).unwrap_or(u64::MAX),
        merge_ms: u64::try_from(merge).unwrap_or(u64::MAX),
    }
}

/// Apply per-axis `weight_permille` (truncated per axis) and return `(score_ms, penalty_ms)`.
pub fn score_candidate(
    facts: &CandidateFacts,
    penalties: &PenaltyPolicy,
    w_right: u64,
    w_narrow: u64,
    w_merge: u64,
) -> (u64, u64) {
    let raw = raw_axis_penalties(facts, penalties);
    let p_right = u128::from(raw.right_turn_ms) * u128::from(w_right) / 1_000;
    let p_narrow = u128::from(raw.narrow_road_ms) * u128::from(w_narrow) / 1_000;
    let p_merge = u128::from(raw.merge_ms) * u128::from(w_merge) / 1_000;
    let penalty_u128 = p_right + p_narrow + p_merge;
    let penalty_ms = u64::try_from(penalty_u128).unwrap_or(u64::MAX);
    let score_ms = facts.eta_ms.saturating_add(penalty_ms);
    (score_ms, penalty_ms)
}

fn validate_request(req: &RankRequest) -> Result<(), CoreError> {
    let p = &req.profile;
    let l2_range = L2_PENALTY_PERMILLE_MIN..=L2_PENALTY_PERMILLE_MAX;
    if !l2_range.contains(&p.right_turn_penalty_permille)
        || !l2_range.contains(&p.narrow_road_penalty_permille)
        || !l2_range.contains(&p.merge_penalty_permille)
    {
        return Err(CoreError::new(
            ErrorCode::Schema,
            format!(
                "L2 *_penalty_permille must be in {L2_PENALTY_PERMILLE_MIN}..={L2_PENALTY_PERMILLE_MAX}"
            ),
        ));
    }
    let l3 = &req.policy.l3_level_permille;
    if l3.low == 0 || l3.normal == 0 || l3.high == 0 {
        return Err(CoreError::new(
            ErrorCode::Schema,
            "L3 level multiplier must be >= 1 (0x forbidden)",
        ));
    }
    if req.policy.detour.ratio_cap_permille < MAX_DETOUR_RATIO_PERMILLE_MIN {
        return Err(CoreError::new(
            ErrorCode::Schema,
            "policy.detour.ratio_cap_permille must be >= 1000",
        ));
    }
    if let Some(r) = p.max_detour_ratio_permille {
        if !(MAX_DETOUR_RATIO_PERMILLE_MIN..=MAX_DETOUR_RATIO_PERMILLE_MAX).contains(&r) {
            return Err(CoreError::new(
                ErrorCode::Schema,
                format!(
                    "profile.max_detour_ratio_permille must be in {MAX_DETOUR_RATIO_PERMILLE_MIN}..={MAX_DETOUR_RATIO_PERMILLE_MAX}"
                ),
            ));
        }
    }
    Ok(())
}

/// Rank candidate routes (02 §2.1 #6, 03 §3.2). Pure, deterministic, integer-only.
pub fn rank(req: &RankRequest) -> Result<RouteComparison, CoreError> {
    validate_request(req)?;

    let mut dropped = Vec::new();
    let mut legal_complete: Vec<&CandidateFacts> = Vec::new();

    for f in &req.facts {
        if !f.legal {
            dropped.push(DroppedCandidate {
                candidate_id: f.candidate_id.clone(),
                reason: RankDropReason::NotLegal,
            });
            continue;
        }
        if !f.complete {
            dropped.push(DroppedCandidate {
                candidate_id: f.candidate_id.clone(),
                reason: RankDropReason::NotComplete,
            });
            continue;
        }
        legal_complete.push(f);
    }

    if legal_complete.is_empty() {
        return Ok(RouteComparison {
            baseline_id: None,
            base_eta_ms: None,
            ranked: Vec::new(),
            dropped,
        });
    }

    // Reference graph_version: explicit policy.expected_graph_version, else the first
    // legal+complete baseline candidate's graph_version, else the first legal+complete one.
    let ref_graph_version: &str = req
        .policy
        .expected_graph_version
        .as_deref()
        .or_else(|| {
            legal_complete
                .iter()
                .find(|f| f.label == FactLabel::Baseline)
                .map(|f| f.graph_version.as_str())
        })
        .unwrap_or_else(|| legal_complete[0].graph_version.as_str());

    let mut same_version: Vec<&CandidateFacts> = Vec::new();
    for f in legal_complete {
        if f.graph_version != ref_graph_version {
            dropped.push(DroppedCandidate {
                candidate_id: f.candidate_id.clone(),
                reason: RankDropReason::GraphVersionMismatch,
            });
        } else {
            same_version.push(f);
        }
    }

    if same_version.is_empty() {
        return Ok(RouteComparison {
            baseline_id: None,
            base_eta_ms: None,
            ranked: Vec::new(),
            dropped,
        });
    }

    // Deduplicate by fingerprint (03 §3.2). Keep the deterministic winner per fingerprint:
    // smallest (eta_ms, label != Baseline, candidate_id).
    let mut by_fp: BTreeMap<&str, &CandidateFacts> = BTreeMap::new();
    for f in same_version {
        match by_fp.get(f.fingerprint.as_str()) {
            None => {
                by_fp.insert(f.fingerprint.as_str(), f);
            }
            Some(&cur) => {
                let f_key = (f.eta_ms, f.label != FactLabel::Baseline, &f.candidate_id);
                let cur_key = (
                    cur.eta_ms,
                    cur.label != FactLabel::Baseline,
                    &cur.candidate_id,
                );
                if f_key < cur_key {
                    dropped.push(DroppedCandidate {
                        candidate_id: cur.candidate_id.clone(),
                        reason: RankDropReason::DuplicateFingerprint,
                    });
                    by_fp.insert(f.fingerprint.as_str(), f);
                } else {
                    dropped.push(DroppedCandidate {
                        candidate_id: f.candidate_id.clone(),
                        reason: RankDropReason::DuplicateFingerprint,
                    });
                }
            }
        }
    }
    let unique: Vec<&CandidateFacts> = by_fp.into_values().collect();

    // Baseline = minimum-ETA accepted candidate (03 §3.2). Deterministic tie-break:
    // label == Baseline first, then smaller unknown_length_m, then fingerprint lex.
    let baseline = *unique
        .iter()
        .min_by(|a, b| {
            (
                a.eta_ms,
                a.label != FactLabel::Baseline,
                a.unknown_length_m,
                a.fingerprint.as_str(),
            )
                .cmp(&(
                    b.eta_ms,
                    b.label != FactLabel::Baseline,
                    b.unknown_length_m,
                    b.fingerprint.as_str(),
                ))
        })
        .expect("unique is non-empty");
    let baseline_id = baseline.candidate_id.clone();
    let base_eta_ms = baseline.eta_ms;

    let detour_policy =
        effective_detour_policy(&req.policy.detour, req.profile.max_detour_ratio_permille);
    let tight = req.profile.detour == Detour::Tight;

    let w_right = combined_weight_permille(
        req.profile.right_turn_penalty_permille,
        req.profile.l3_weights.crossing_right,
        &req.policy.l3_level_permille,
    );
    let w_narrow = combined_weight_permille(
        req.profile.narrow_road_penalty_permille,
        req.profile.l3_weights.narrow_road,
        &req.policy.l3_level_permille,
    );
    let w_merge = combined_weight_permille(
        req.profile.merge_penalty_permille,
        req.profile.l3_weights.merge,
        &req.policy.l3_level_permille,
    );

    let mut kept: Vec<(&CandidateFacts, u64, u64)> = Vec::new();
    for f in unique {
        let is_baseline = f.candidate_id == baseline_id;
        // Baseline is always kept (03 §1 / §3.2); all others use the shared DetourPolicy::within.
        if !is_baseline && !detour_policy.within(base_eta_ms, f.eta_ms, tight) {
            dropped.push(DroppedCandidate {
                candidate_id: f.candidate_id.clone(),
                reason: RankDropReason::OverDetourCap,
            });
            continue;
        }
        let (score_ms, penalty_ms) =
            score_candidate(f, &req.policy.penalties_ms, w_right, w_narrow, w_merge);
        kept.push((f, score_ms, penalty_ms));
    }

    // Deterministic ranking (03 §3.2):
    // 1. score_ms (asc) -> 2. eta_ms (asc) -> 3. unknown_length_m (asc) -> 4. fingerprint (lex asc)
    kept.sort_by(|(a, a_score, _), (b, b_score, _)| {
        (
            *a_score,
            a.eta_ms,
            a.unknown_length_m,
            a.fingerprint.as_str(),
        )
            .cmp(&(
                *b_score,
                b.eta_ms,
                b.unknown_length_m,
                b.fingerprint.as_str(),
            ))
    });

    let base_eta_i64 = i64::try_from(base_eta_ms).unwrap_or(i64::MAX);
    let ranked = kept
        .into_iter()
        .enumerate()
        .map(|(idx, (f, score_ms, penalty_ms))| {
            let delta_eta_ms = i64::try_from(f.eta_ms)
                .unwrap_or(i64::MAX)
                .saturating_sub(base_eta_i64);
            RankedCandidate {
                rank: u32::try_from(idx + 1).unwrap_or(u32::MAX),
                candidate_id: f.candidate_id.clone(),
                label: f.label,
                fingerprint: f.fingerprint.clone(),
                is_baseline: f.candidate_id == baseline_id,
                score_ms,
                penalty_ms,
                display: ComparisonDisplay {
                    eta_ms: f.eta_ms,
                    delta_eta_ms,
                    right_turns: RightTurnDisplay {
                        unprotected: f.counts.right_unprotected,
                        signal_unknown_protection: f.counts.right_signal_unknown_protection,
                        signal_unknown: f.counts.right_signal_unknown,
                        protected: f.counts.right_protected,
                        right_entry: f.counts.right_entry,
                        right_entry_unknown: f.counts.right_entry_unknown,
                    },
                    narrow: NarrowDisplay {
                        confirmed_m: f.narrow_m.confirmed,
                        inferred_m: f.narrow_m.inferred,
                        unknown_width_residential_m: f.unknown_width_residential_m,
                    },
                },
            }
        })
        .collect();

    Ok(RouteComparison {
        baseline_id: Some(baseline_id),
        base_eta_ms: Some(base_eta_ms),
        ranked,
        dropped,
    })
}
