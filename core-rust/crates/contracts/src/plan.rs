//! Mirror of schema/trip-plan-snapshot.schema.json (input of `dc_plan_candidates` and of
//! `Session::plan_candidates`). Unknown fields are rejected (`additionalProperties: false`).
//! TODO(schema-v1): GeoPoint / vias / budget units are provisional in the schema too.
use crate::common::U64Str;
use crate::override_types::SchemaV1;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TripPlanSnapshot {
    pub schema_version: SchemaV1,
    pub trip_id: String,
    pub trip_revision: U64Str,
    pub plan_request_id: String,
    pub graph_version: String,
    pub policy_version: String,
    pub origin: GeoPoint,
    pub destination: Destination,
    pub vias: Vec<GeoPoint>,
    pub vehicle: Vehicle,
    pub avoid: Avoid,
    pub depart_at_unix_s: u64,
    pub budget: PlanBudget,
}

/// Provisional WGS84 point (range checks are schema-only).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeoPoint {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub display_point: GeoPoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_entry: Option<VehicleEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pedestrian_exit: Option<GeoPoint>,
}

/// `evidence` is common.schema.json#/$defs/Evidence; kept as raw JSON (not mirrored yet).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VehicleEntry {
    pub id: String,
    pub evidence: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vehicle {
    pub profile: VehicleProfile,
    pub width_mm: u32,
    pub height_mm: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VehicleProfile {
    StandardCar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Avoid {
    pub toll: bool,
    pub ferry: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanBudget {
    /// `minimum: 1` in the schema; the planner also caps it at 5 requests (03 §3.1 item 3).
    pub max_requests: u32,
    pub deadline_ms: u64,
}

/// One search request produced by `Session::plan_candidates` (03 §2 `CandidatePlanner`:
/// ordered `CandidateRequest[]`, labels per 03 §3.1). Kotlin turns it into a Valhalla
/// request when a `start{candidate_id}` for it arrives.
/// TODO(schema-v1): no schema file yet (output of dc_plan_candidates, which is not wired).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateRequest {
    pub candidate_id: String,
    pub label: CandidateLabel,
    /// Queue priority of its search: `explicit_plan` for the baseline, `comparison` for the
    /// additional searches (team decision 2026-11-25).
    pub priority: crate::tick::SearchPriority,
    /// Baseline only: how many `engine_alternate`s to ask Valhalla for in the same call
    /// (max 2, and 0 when there are vias: Valhalla returns no alternates then, 03 §3.1 item 1).
    pub engine_alternates: u8,
    /// `costing_variant` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub costing_variant: Option<CostingVariant>,
}

/// 03 §2 labels. `engine_alternate` is not a separate request: it comes back from the
/// baseline call (`CandidateRequest::engine_alternates`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateLabel {
    Baseline,
    CostingVariant,
    ThroughAnchor,
}

/// The two existing Auto-costing combinations named in 03 §3.1 item 2.
/// TODO(schema-v1): concrete Valhalla parameter values are not specified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostingVariant {
    /// 幹線道路選好.
    PreferTrunk,
    /// `service_penalty` で生活道路入口を抑制.
    ServicePenalty,
}

/// Result of `Session::plan_candidates` (shape: team decision 2026-11-25, plus the wake
/// time and log every session call returns).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCandidates {
    pub plan_request_id: String,
    pub distance_class: crate::tick::DistanceClass,
    pub candidates: Vec<CandidateRequest>,
    pub planner_cmds: Vec<crate::tick::PlannerCmd>,
    pub next_wake_mono_ms: Option<u64>,
    pub log: Vec<crate::tick::LogEvent>,
}
