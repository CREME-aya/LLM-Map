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
//!    route removes it (`dismiss{route_changed}`).
//!
//! Disabled by default: "停止遅延を実測するまでは、走行中の回避チップ先読みは無効（既定）"
//! (04 §4.3 item 6, 03 §3.1); `SessionConfig::avoid_chip_lookahead` turns it on.
//!
//! TODO(04 §4.3): not built yet — consent by the dictionary "はい" (`asr_result.dict_norm`;
//! the dictionary values are not specified), undo within 5 s ("戻して" / `CXL`; the docs
//! only have a proposal), the voice preannouncement about 60 s before (max 5 per trip; no
//! voice scheduler yet), skipping spots accepted in the pre-departure comparison (item 4;
//! that state is not passed to Rust), cancelling a search that outlives the 1.5 s budget,
//! checking that the alternative really avoids the target (the bundle has no edge ids per
//! maneuver), `avoided.narrow_m` (the bundle has no narrow length: 0 for now),
//! `profile_weights.max_detour_ratio` (03 §5) and the `dt=T` tight factor while driving.
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
}

impl Default for AvoidConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            budget_ms: 1_500,
            deadline_before_ms: 20_000,
            trigger_eta_ms: 120_000,
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

/// `dc_avoid_offer_eval` decision for a waiting result (`now` < deadline and tracking are
/// checked by the caller first).
pub fn build_offer(
    req_id: &str,
    target: &AvoidTarget,
    graph_version: &str,
    facts: &CandidateFacts,
    policy: &DetourPolicy,
) -> Result<AvoidOffer, AvoidDeclineReason> {
    if facts.graph_version != graph_version {
        return Err(AvoidDeclineReason::GraphMismatch);
    }
    if !facts.legal || !facts.complete {
        return Err(AvoidDeclineReason::NotLegalOrComplete);
    }
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
}
