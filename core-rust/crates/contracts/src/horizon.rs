//! Parts of `ManeuverHorizon` (docs v1.2 04 §2) that already have a Rust mirror.
//! TODO(drive-core): the full typed ManeuverHorizon (TickOutput.horizon is raw JSON for now).
use crate::common::MonoMs;
use serde::{Deserialize, Serialize};

/// Avoid chip attached to a `HorizonEntry` (04 §2). No display text: Kotlin builds the
/// text from `summary.template_id` + numbers, shared by screen and voice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvoidOffer {
    /// What `user_tap.target_id` points at for an avoid chip (same treatment as
    /// `alt_route_id` / `override_id`: an opaque String).
    pub offer_id: String,
    pub alt_route_id: String,
    pub delta_eta_ms: i64,
    pub decision_deadline_monotonic_ms: MonoMs,
    pub summary: AvoidSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvoidSummary {
    pub template_id: String,
    pub delta_eta_ms: i64,
    pub avoided: Avoided,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Avoided {
    pub unprotected_right: u8,
    pub narrow_m: u32,
}
