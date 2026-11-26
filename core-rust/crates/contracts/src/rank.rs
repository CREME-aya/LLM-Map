//! `dc_rank` input/output contract (`schema/route-comparison.schema.json`; 02 §2.1 #6,
//! 03 §3.2, 07 §1). Integer-only (no floating point).

use crate::facts::{CandidateFacts, FactLabel};
use crate::override_types::{Detour, Weights};
use serde::{Deserialize, Serialize};

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Allowed range for L2 `profile_weights.*_penalty_permille` (03 §5, commit `eb6b2ce`).
pub const L2_PENALTY_PERMILLE_MIN: u64 = 250;
pub const L2_PENALTY_PERMILLE_MAX: u64 = 4_000;
/// Allowed range for `profile_weights.max_detour_ratio_permille` (03 §3.2 / §5, commit `eb6b2ce`).
pub const MAX_DETOUR_RATIO_PERMILLE_MIN: u64 = 1_000;
pub const MAX_DETOUR_RATIO_PERMILLE_MAX: u64 = 1_350;

fn positive_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = u64::deserialize(d)?;
    if v == 0 {
        return Err(serde::de::Error::custom("must be >= 1 (0x forbidden)"));
    }
    Ok(v)
}

fn l2_penalty_permille<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = u64::deserialize(d)?;
    if !(L2_PENALTY_PERMILLE_MIN..=L2_PENALTY_PERMILLE_MAX).contains(&v) {
        return Err(serde::de::Error::custom(format!(
            "penalty_permille must be in {L2_PENALTY_PERMILLE_MIN}..={L2_PENALTY_PERMILLE_MAX}, got {v}"
        )));
    }
    Ok(v)
}

fn ratio_permille_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = u64::deserialize(d)?;
    if v < MAX_DETOUR_RATIO_PERMILLE_MIN {
        return Err(serde::de::Error::custom("ratio_permille must be >= 1000"));
    }
    Ok(v)
}

fn opt_ratio_permille_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    match Option::<u64>::deserialize(d)? {
        None => Ok(None),
        Some(v) if (MAX_DETOUR_RATIO_PERMILLE_MIN..=MAX_DETOUR_RATIO_PERMILLE_MAX).contains(&v) => {
            Ok(Some(v))
        }
        Some(v) => Err(serde::de::Error::custom(format!(
            "max_detour_ratio_permille must be in {MAX_DETOUR_RATIO_PERMILLE_MIN}..={MAX_DETOUR_RATIO_PERMILLE_MAX}, got {v}"
        ))),
    }
}

fn non_empty_opt_string<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<String>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) if !s.is_empty() => Ok(Some(s)),
        Some(_) => Err(serde::de::Error::custom("must not be empty")),
    }
}

/// Request of `dc_rank`: `{facts[], policy?, profile?}` (02 §2.1 #6, 03 §3.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankRequest {
    pub facts: Vec<CandidateFacts>,
    #[serde(default)]
    pub policy: RankPolicy,
    #[serde(default)]
    pub profile: RankProfile,
}

/// Provisional policy parameters for `dc_rank` (03 §3.2, 07 §1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RankPolicy {
    pub detour: DetourPolicyConfig,
    pub penalties_ms: PenaltyPolicy,
    pub l3_level_permille: L3LevelScale,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "non_empty_opt_string"
    )]
    pub expected_graph_version: Option<String>,
}

/// Serializable mirror of `drive_core::avoid::DetourPolicy` (03 §3.2, 07 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DetourPolicyConfig {
    pub short_threshold_s: u64,
    pub short_cap_s: u64,
    pub long_cap_s: u64,
    pub long_cap_ratio_permille: u64,
    #[serde(deserialize_with = "ratio_permille_u64")]
    pub ratio_cap_permille: u64,
    pub tight_factor_permille: u64,
}

impl Default for DetourPolicyConfig {
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

/// Provisional comfort penalties in ms (03 §3.2, 07 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PenaltyPolicy {
    pub right_unprotected_ms: u64,
    pub right_signal_unknown_ms: u64,
    pub right_signal_unknown_protection_ms: u64,
    pub right_protected_ms: u64,
    /// Replaces the arrival maneuver's right-turn penalty (+90 s, not additive).
    pub right_entry_ms: u64,
    /// Destination entry with unknown approach side (+45 s).
    pub right_entry_unknown_ms: u64,
    /// Narrow road (< 4.0 m) penalty per 100 m (+30 s / 100 m).
    pub narrow_ms_per_100m: u64,
    /// Unknown-width residential road penalty per 100 m (+15 s / 100 m).
    pub unknown_width_residential_ms_per_100m: u64,
    /// Ramp merge penalty (+15 s / occurrence).
    pub merge_ms: u64,
}

impl Default for PenaltyPolicy {
    fn default() -> Self {
        Self {
            right_unprotected_ms: 45_000,
            right_signal_unknown_ms: 45_000,
            right_signal_unknown_protection_ms: 25_000,
            right_protected_ms: 10_000,
            right_entry_ms: 90_000,
            right_entry_unknown_ms: 45_000,
            narrow_ms_per_100m: 30_000,
            unknown_width_residential_ms_per_100m: 15_000,
            merge_ms: 15_000,
        }
    }
}

/// Provisional L3 `low / normal / high` multipliers in per-mille (03 §3.2, 07 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct L3LevelScale {
    #[serde(deserialize_with = "positive_u64")]
    pub low: u64,
    #[serde(deserialize_with = "positive_u64")]
    pub normal: u64,
    #[serde(deserialize_with = "positive_u64")]
    pub high: u64,
}

impl Default for L3LevelScale {
    fn default() -> Self {
        Self {
            low: 500,
            normal: 1_000,
            high: 2_000,
        }
    }
}

/// L2 profile weights (`*_penalty_permille`, 250..=4000, default 1000;
/// `max_detour_ratio_permille`, 1000..=1350 or null) and optional L3 override (03 §3.2 / §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RankProfile {
    #[serde(deserialize_with = "l2_penalty_permille")]
    pub right_turn_penalty_permille: u64,
    #[serde(deserialize_with = "l2_penalty_permille")]
    pub narrow_road_penalty_permille: u64,
    #[serde(deserialize_with = "l2_penalty_permille")]
    pub merge_penalty_permille: u64,
    #[serde(default, deserialize_with = "opt_ratio_permille_u64")]
    pub max_detour_ratio_permille: Option<u64>,
    pub l3_weights: Weights,
    pub detour: Detour,
}

impl Default for RankProfile {
    fn default() -> Self {
        Self {
            right_turn_penalty_permille: 1_000,
            narrow_road_penalty_permille: 1_000,
            merge_penalty_permille: 1_000,
            max_detour_ratio_permille: None,
            l3_weights: Weights::default(),
            detour: Detour::Standard,
        }
    }
}

/// Output of `dc_rank` (02 §2.1 #6, 03 §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteComparison {
    pub baseline_id: Option<String>,
    pub base_eta_ms: Option<u64>,
    pub ranked: Vec<RankedCandidate>,
    pub dropped: Vec<DroppedCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankedCandidate {
    pub rank: u32,
    pub candidate_id: String,
    pub label: FactLabel,
    pub fingerprint: String,
    pub is_baseline: bool,
    pub score_ms: u64,
    pub penalty_ms: u64,
    pub display: ComparisonDisplay,
}

/// Raw, unweighted 3-axis display figures (03 §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonDisplay {
    pub eta_ms: u64,
    pub delta_eta_ms: i64,
    pub right_turns: RightTurnDisplay,
    pub narrow: NarrowDisplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RightTurnDisplay {
    pub unprotected: u64,
    pub signal_unknown_protection: u64,
    pub signal_unknown: u64,
    pub protected: u64,
    pub right_entry: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub right_entry_unknown: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NarrowDisplay {
    pub confirmed_m: u64,
    pub inferred_m: u64,
    pub unknown_width_residential_m: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DroppedCandidate {
    pub candidate_id: String,
    pub reason: RankDropReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankDropReason {
    NotLegal,
    NotComplete,
    GraphVersionMismatch,
    DuplicateFingerprint,
    OverDetourCap,
}
