//! Driving-time avoid chip (04 §4.3, 03 §3.1 / §3.2). Integer-only.
//!
//! Flow (one chip at a time; the session drives it from `dc_tick` and `dc_avoid_offer_eval`):
//! 1. Trigger: while `tracking`, an unpassed avoid target (`junction::is_avoid_target`)
//!    among the next 3 maneuvers, or with `eta_range.earliest` <= 120 s ("到達約 120 秒前";
//!    `earliest` is used for every deadline, 04 §2.2). Each maneuver is tried once per route.
//! 2. Start condition: decision deadline = branch `earliest` − 20 s (fixed when the search
//!    starts, from that tick's sample); the search starts only when `now + 1.5 s budget <=
//!    deadline` and the search slot is free (nothing running or waiting), so it starts at
//!    once with priority `avoid_chip`. The remaining ETA then is the `base_eta`.
//! 3. `search_finished{ok}` (valid: not cancelled / stuck, same `route_key`, `tracking`)
//!    is **not** committed: it waits for `dc_avoid_offer_eval`, which checks the deadline,
//!    the candidate and the detour cap (`DetourPolicy`, the function `dc_rank` uses too)
//!    and builds the `AvoidOffer`; the horizon carries it on that maneuver's `HorizonEntry`.
//! 4. Consent: `user_tap{target_id: offer_id, action: avoid}` before the deadline, with
//!    the offer's `route_key` still the current one, while `tracking` → `planner_cmds:
//!    commit{req_id, route_key}` (req_id = the avoid search, route_key = the route when
//!    it started). The deadline passing removes the offer (`dismiss{expired}`); leaving the
//!    route removes it (`dismiss{route_changed}`). The consent `commit` carries
//!    `undo_req_id` (`undo-<req_id>`, provisional): Kotlin keeps the previous route body
//!    under it for 5 s.
//! 5. "戻して" (03 §3.1; team decision 2026-11-26): the divergence point is computed once,
//!    in the `dc_route_commit` that loads the consented avoid route (Rust then has both the
//!    old route and the new bundle), from the position when the search started
//!    ([`divergence_cm`]); it is kept in the [`UndoWindow`]. `user_tap{offer_id, undo}` or
//!    `CXL` (`dc_validate_override`): more than 5 s after consent → `NO_PENDING_TARGET`;
//!    avoid route not loaded yet → `cancel_commit{req_id}` (docs 836a370: the consent commit
//!    is withdrawn, its late `dc_route_commit` is `E_STALE`; a success, CXL → `undone{req_id}`);
//!    divergence unknown, position not `tracking` (e.g.
//!    `uncertain`), or at / past the divergence point → `NO_EFFECT`; otherwise
//!    `commit{req_id: undo_req_id, route_key: current}` and any held result is dropped.
//!    A refusal also speaks [`UNDO_UNAVAILABLE_TEMPLATE_ID`] once.
//!
//! Disabled by default: "停止遅延を実測するまでは、走行中の回避チップ先読みは無効（既定）"
//! (04 §4.3 item 6, 03 §3.1); `SessionConfig::avoid_chip_lookahead` turns it on.
//!
//! TODO(04 §4.3): not built yet — consent by the dictionary "はい" (`asr_result.dict_norm`;
//! the dictionary values are not specified), the voice preannouncement about 60 s before
//! (max 5 per trip; no voice scheduler yet), skipping spots accepted in the pre-departure
//! comparison (item 4; that state is not passed to Rust), cancelling a search that outlives
//! the 1.5 s budget, `avoided.narrow_m` (the bundle has no narrow length: 0 for now),
//! `profile_weights.max_detour_ratio` (03 §5) and the `dt=T` tight factor while driving.
//! (a) only sees `alt_facts.junctions`: a pair the alternative passes but C++ does not list
//! there is not caught (docs say "the alternative's edge sequence"; `CandidateFacts` has none).
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::junction::{classify, JunctionClass};
use contracts::facts::{AvoidDeclineReason, CandidateFacts};
use contracts::horizon::{AvoidOffer, AvoidSummary, Avoided, Maneuver, ManeuverId};
use contracts::RouteKey;

/// `DetourPolicy` (03 §3.2, 07 §1; provisional initial values). Ratios in per-mille.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetourPolicy {
    pub short_threshold_s: u64,
    pub short_cap_s: u64,
    pub long_cap_s: u64,
    /// 0.15 → 150.
    pub long_cap_ratio_permille: u64,
    /// 1.35 → 1350.
    pub ratio_cap_permille: u64,
    /// 0.5 → 500.
    pub tight_factor_permille: u64,
}

impl Default for DetourPolicy {
    fn default() -> Self {
        Self {
            short_threshold_s: 1_800,
            short_cap_s: 180,
            long_cap_s: 900,
            long_cap_ratio_permille: 150,
            ratio_cap_permille: 1_350,
            tight_factor_permille: 500,
        }
    }
}

impl DetourPolicy {
    /// Allowed extra time (ms) over `base_eta_ms` (03 §3.2):
    /// `min(time_cap, base × (ratio_cap − 1)) × (tight ? tight_factor : 1)`,
    /// `time_cap = base < short_threshold ? short_cap : min(long_cap, base × long_cap_ratio)`.
    /// Products are rounded down (the strict side).
    pub fn cap_ms(&self, base_eta_ms: u64, tight: bool) -> u64 {
        let time_cap = if base_eta_ms < self.short_threshold_s.saturating_mul(1_000) {
            self.short_cap_s.saturating_mul(1_000)
        } else {
            self.long_cap_s
                .saturating_mul(1_000)
                .min(base_eta_ms.saturating_mul(self.long_cap_ratio_permille) / 1_000)
        };
        let ratio =
            base_eta_ms.saturating_mul(self.ratio_cap_permille.saturating_sub(1_000)) / 1_000;
        let cap = time_cap.min(ratio);
        if tight {
            cap.saturating_mul(self.tight_factor_permille) / 1_000
        } else {
            cap
        }
    }

    /// The candidate is within the detour cap: `cand − base <= cap` (a faster candidate
    /// always is). Shared by `dc_rank` and `dc_avoid_offer_eval` (03 §3.2).
    pub fn within(&self, base_eta_ms: u64, cand_eta_ms: u64, tight: bool) -> bool {
        cand_eta_ms.saturating_sub(base_eta_ms) <= self.cap_ms(base_eta_ms, tight)
    }
}

/// Avoid-chip parameters (04 §4.3, 07 §1; provisional).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvoidConfig {
    /// Look-ahead while driving (04 §4.3 item 6: off until the stop latency is measured).
    pub enabled: bool,
    /// Compute budget (1.5 s).
    pub budget_ms: u64,
    /// Decision deadline before the branch (20 s), from `earliest`.
    pub deadline_before_ms: u64,
    /// "到達約 120 秒前" trigger, compared with `earliest`.
    pub trigger_eta_ms: u64,
    /// "戻して" window after consent (5 s; 04 §4.3 item 3, 03 §3.1).
    pub undo_window_ms: u64,
}

impl Default for AvoidConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            budget_ms: 1_500,
            deadline_before_ms: 20_000,
            trigger_eta_ms: 120_000,
            undo_window_ms: 5_000,
        }
    }
}

/// What a chip is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvoidTarget {
    pub maneuver_id: ManeuverId,
    /// The current route when the search started (the `commit` key).
    pub route_key: RouteKey,
    pub deadline_mono_ms: u64,
    /// Remaining ETA on the current route when the search started (`base_eta`, 03 §3.2).
    pub base_eta_ms: u64,
    pub avoided: Avoided,
    /// Matched position on the current route when the search started (`along_route_cm`):
    /// the origin of the divergence search (03 §3.1 "探索開始時の位置").
    pub start_progress_cm: u32,
    /// `(edge_in, edge_out)` of the target maneuver on the current route
    /// ([`crate::route::LoadedRoute::edge_pair_at`]); `None` = unknown (no offer).
    pub edge_pair: Option<(u64, u64)>,
}

/// Consent given (`commit` issued), the avoid route not loaded yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consent {
    /// The avoid search's `req_id` (= the consent `commit`'s; `dc_route_commit` names it).
    pub req_id: String,
    pub offer_id: String,
    pub undo_req_id: String,
    /// The route the avoid search started on (= the consent `commit`'s `route_key`).
    pub from_route_key: RouteKey,
    pub start_progress_cm: u32,
    pub consent_at_mono_ms: u64,
}

/// The consented avoid route is current; "戻して" is possible until the window ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoWindow {
    pub offer_id: String,
    pub undo_req_id: String,
    /// The avoid route's key (assigned by `dc_route_commit`).
    pub route_key: RouteKey,
    pub consent_at_mono_ms: u64,
    /// Divergence point on the avoid route (`along_route_cm`); `None` = unknown (no undo).
    pub divergence_cm: Option<u32>,
}

/// Where the consent stands for "戻して".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoState {
    None,
    Consented(Consent),
    Window(UndoWindow),
}

/// Provisional id of the previous route body Kotlin keeps for the undo (team decision
/// 2026-11-26).
pub fn undo_req_id(avoid_req_id: &str) -> String {
    format!("undo-{avoid_req_id}")
}

/// Provisional template id of the short phrase spoken when "戻して" is refused
/// (`NO_EFFECT` / `NO_PENDING_TARGET`; 03 §3.1). TODO(04 §2 / 05 §5): not named in docs.
pub const UNDO_UNAVAILABLE_TEMPLATE_ID: &str = "undo_unavailable";

/// Divergence point of the avoid route from the original one (03 §3.1 steps 1–3), as
/// `along_route_cm` on the avoid route. Edges are `(directed edge_id, along_start,
/// along_end)` in route order ([`crate::route::LoadedRoute::edge_spans`]).
/// 1. The avoid route's first edge (it starts mid-edge, at the search-start position) is
///    looked up among the original edges ahead of `start_progress_cm` (an edge ending after
///    it, so the one the vehicle was on counts; not the current position: the vehicle
///    moved during the search).
/// 2. From that match both sequences are compared forward by edge id; the first avoid
///    edge that differs is the divergence point (its `along_start`). Running past the end
///    of the original route also counts as a difference.
/// 3. No match, more than one match (a looping route), or no difference at all → `None`.
pub fn divergence_cm(
    original: &[(u64, u32, u32)],
    avoid: &[(u64, u32, u32)],
    start_progress_cm: u32,
) -> Option<u32> {
    let first = avoid.first()?.0;
    let mut matches = original
        .iter()
        .enumerate()
        .filter(|(_, e)| e.2 > start_progress_cm && e.0 == first)
        .map(|(i, _)| i);
    let at = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let mut k = 1;
    loop {
        let a = avoid.get(k)?;
        match original.get(at + k) {
            Some(o) if o.0 == a.0 => k += 1,
            _ => return Some(a.1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvoidState {
    Idle,
    Searching {
        req_id: String,
        target: AvoidTarget,
    },
    /// `ok` returned; waiting for `dc_avoid_offer_eval`.
    Ready {
        req_id: String,
        target: AvoidTarget,
    },
    Offered {
        req_id: String,
        target: AvoidTarget,
        offer: AvoidOffer,
    },
}

impl AvoidState {
    pub fn target(&self) -> Option<&AvoidTarget> {
        match self {
            Self::Idle => None,
            Self::Searching { target, .. }
            | Self::Ready { target, .. }
            | Self::Offered { target, .. } => Some(target),
        }
    }

    pub fn offer(&self) -> Option<(&ManeuverId, &AvoidOffer)> {
        match self {
            Self::Offered { target, offer, .. } => Some((&target.maneuver_id, offer)),
            _ => None,
        }
    }
}

/// `summary.avoided` for a target: 1 right turn when it is one of the right-turn classes.
/// `narrow_m` is 0 (TODO, see module docs).
pub fn avoided(m: &Maneuver) -> Avoided {
    let right = classify(m).iter().any(|c| {
        matches!(
            c,
            JunctionClass::RightUnprotected
                | JunctionClass::RightSignalUnknownProtection
                | JunctionClass::RightSignalUnknown
        )
    });
    Avoided {
        unprotected_right: u8::from(right),
        narrow_m: 0,
    }
}

/// Provisional template id of the chip text (Kotlin renders it; TODO(04 §2): not named).
pub const AVOID_TEMPLATE_ID: &str = "avoid_chip";

/// Number of avoid-chip targets among the right turns of a `CandidateFacts` (04 §2.2 /
/// §4.3: `right_unprotected`, `right_signal_unknown_protection`, `right_signal_unknown`;
/// narrow roads are compared through `narrow_m`).
pub fn target_count(f: &CandidateFacts) -> u64 {
    f.counts
        .right_unprotected
        .saturating_add(f.counts.right_signal_unknown_protection)
        .saturating_add(f.counts.right_signal_unknown)
}

/// Narrow length of a `CandidateFacts` (m): confirmed + inferred.
pub fn narrow_total_m(f: &CandidateFacts) -> u64 {
    f.narrow_m.confirmed.saturating_add(f.narrow_m.inferred)
}

/// 04 §4.3 item 2: is the alternative really better? (a) the target's `(edge_in, edge_out)`
/// is not among `alt.junctions` (unknown pair → cannot be confirmed); (b) against
/// `base` (the current route from the search start, also built by C++: same yardstick),
/// fewer avoid targets and `narrow_m` not longer.
pub fn check_alternative(
    target: &AvoidTarget,
    alt: &CandidateFacts,
    base: &CandidateFacts,
) -> Result<(), AvoidDeclineReason> {
    let Some((edge_in, edge_out)) = target.edge_pair else {
        return Err(AvoidDeclineReason::TargetEdgesUnknown);
    };
    if alt
        .junctions
        .iter()
        .any(|j| j.edge_in.0 == edge_in && j.edge_out.0 == edge_out)
    {
        return Err(AvoidDeclineReason::TargetNotAvoided);
    }
    if target_count(alt) >= target_count(base) || narrow_total_m(alt) > narrow_total_m(base) {
        return Err(AvoidDeclineReason::NotFewerHazards);
    }
    Ok(())
}

/// `dc_avoid_offer_eval` decision for a waiting result (`now` < deadline and tracking are
/// checked by the caller first). Order: graph version (both), legal / complete,
/// [`check_alternative`] (a) then (b), detour cap.
pub fn build_offer(
    req_id: &str,
    target: &AvoidTarget,
    graph_version: &str,
    facts: &CandidateFacts,
    base: &CandidateFacts,
    policy: &DetourPolicy,
) -> Result<AvoidOffer, AvoidDeclineReason> {
    if facts.graph_version != graph_version || base.graph_version != graph_version {
        return Err(AvoidDeclineReason::GraphMismatch);
    }
    if !facts.legal || !facts.complete {
        return Err(AvoidDeclineReason::NotLegalOrComplete);
    }
    check_alternative(target, facts, base)?;
    if !policy.within(target.base_eta_ms, facts.eta_ms, false) {
        return Err(AvoidDeclineReason::OverDetourCap);
    }
    let delta = i64::try_from(facts.eta_ms)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(target.base_eta_ms).unwrap_or(i64::MAX));
    Ok(AvoidOffer {
        // TODO(abi-v1): id formats are provisional. alt_route_id is the search's req_id,
        // the key Kotlin holds the route body by (the `commit` names it).
        offer_id: format!("avoid-{req_id}"),
        alt_route_id: req_id.to_owned(),
        delta_eta_ms: delta,
        decision_deadline_monotonic_ms: target.deadline_mono_ms,
        summary: AvoidSummary {
            template_id: AVOID_TEMPLATE_ID.to_owned(),
            delta_eta_ms: delta,
            avoided: target.avoided,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_edge_pair_cannot_be_confirmed() {
        let f: CandidateFacts = serde_json::from_str(include_str!(
            "../../../../schema/fixtures/valid/candidate-facts.min.json"
        ))
        .unwrap();
        let mut base = f.clone();
        base.counts.right_unprotected = f.counts.right_unprotected + 1;
        base.narrow_m.confirmed = f.narrow_m.confirmed + f.narrow_m.inferred;
        let mut alt = f;
        alt.junctions.clear();
        let key = RouteKey {
            trip_id: "t".into(),
            trip_revision: contracts::U64Str(1),
            route_revision: contracts::U64Str(1),
            graph_version: "g".into(),
        };
        let mut t = AvoidTarget {
            maneuver_id: ManeuverId {
                route_key: key.clone(),
                leg_index: 0,
                ordinal: 0,
            },
            route_key: key,
            deadline_mono_ms: 0,
            base_eta_ms: 0,
            avoided: Avoided {
                unprotected_right: 1,
                narrow_m: 0,
            },
            start_progress_cm: 0,
            edge_pair: None,
        };
        assert_eq!(
            check_alternative(&t, &alt, &base),
            Err(AvoidDeclineReason::TargetEdgesUnknown)
        );
        t.edge_pair = Some((1, 2));
        assert_eq!(check_alternative(&t, &alt, &base), Ok(()));
    }

    #[test]
    fn detour_cap_formula() {
        let p = DetourPolicy::default();
        // short (< 30 min): min(180 s, base × 0.35)
        assert_eq!(p.cap_ms(600_000, false), 180_000);
        assert_eq!(p.cap_ms(300_000, false), 105_000);
        assert_eq!(p.cap_ms(1_799_999, false), 180_000);
        // long: min(900 s, base × 0.15, base × 0.35)
        assert_eq!(p.cap_ms(1_800_000, false), 270_000);
        assert_eq!(p.cap_ms(7_200_000, false), 900_000);
        // tight: × 0.5
        assert_eq!(p.cap_ms(600_000, true), 90_000);
        assert!(p.within(600_000, 780_000, false));
        assert!(!p.within(600_000, 780_001, false));
        assert!(p.within(600_000, 500_000, false));
    }

    fn spans(ids: &[u64], start: u32, len: u32) -> Vec<(u64, u32, u32)> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| {
                let i = i as u32;
                (*id, start + i * len, start + (i + 1) * len)
            })
            .collect()
    }

    #[test]
    fn divergence_point_rules() {
        // original A B1 B2 C, edges of 100; search started at 150 (in B1)
        let orig = spans(&[1, 2, 3, 4], 0, 100);
        // avoid: B1 (partial 50) B2 D → diverges at its own along 150
        let avoid = vec![(2, 0, 50), (3, 50, 150), (9, 150, 250)];
        assert_eq!(divergence_cm(&orig, &avoid, 150), Some(150));
        // first edge not ahead of the start position (B1 ended at 200 <= 200)
        assert_eq!(divergence_cm(&orig, &avoid, 200), None);
        // not found
        assert_eq!(divergence_cm(&orig, &[(7, 0, 50)], 150), None);
        // found twice ahead (loop) → unknown
        let looped = spans(&[1, 2, 3, 2, 5], 0, 100);
        assert_eq!(divergence_cm(&looped, &avoid, 150), None);
        // an occurrence behind the start position does not count
        let behind = spans(&[2, 2, 3, 4], 0, 100);
        assert_eq!(divergence_cm(&behind, &avoid, 150), Some(150));
        // identical → unknown; avoid longer than the original → first extra edge
        assert_eq!(divergence_cm(&orig, &[(2, 0, 50), (3, 50, 150)], 150), None);
        let longer = vec![(3, 0, 50), (4, 50, 150), (8, 150, 200)];
        assert_eq!(divergence_cm(&orig, &longer, 250), Some(150));
        assert_eq!(divergence_cm(&orig, &[], 0), None);
    }
}
