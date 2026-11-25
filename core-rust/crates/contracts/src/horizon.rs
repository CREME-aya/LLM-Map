//! `ManeuverHorizon` v1 (docs 04 §2), the single source for UI and voice.
//! Mirrors `schema/maneuver-horizon.schema.json`; `Maneuver` is also what
//! `schema/route-bundle.schema.json` carries per leg (`$ref`, no separate type).
use crate::common::{MonoMs, RouteKey};
use serde::{Deserialize, Deserializer, Serialize};

/// `ManeuverId = { route_key, leg_index, ordinal }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManeuverId {
    pub route_key: RouteKey,
    pub leg_index: u32,
    pub ordinal: u32,
}

/// `RouteAnchor = { leg_index, leg_shape_index, along_route_cm: u32 }` (04 §2: u32 cm,
/// about 42,900 km at most). `along_route_cm` is counted from the route start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteAnchor {
    pub leg_index: u32,
    pub leg_shape_index: u32,
    pub along_route_cm: u32,
}

/// `Arrow` (04 §2). Unit variants are bare strings; `roundabout` is
/// `{"roundabout": {"exit_number"?: u8}}` (serde's external tagging = the schema's oneOf).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Arrow {
    Left,
    Right,
    SlightLeft,
    SlightRight,
    SharpLeft,
    SharpRight,
    UTurnLeft,
    UTurnRight,
    KeepLeft,
    KeepRight,
    ForkLeft,
    ForkRight,
    MergeLeft,
    MergeRight,
    Merge,
    ExitLeft,
    ExitRight,
    Exit,
    Roundabout {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_number: Option<u8>,
    },
    FerryBoard,
    FerryLeave,
    StraightDecision,
    Arrive,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Confirmed,
    Inferred,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    Missing,
    Conflict,
    DirectionUnclear,
    VersionMismatch,
}

/// `Evidence<T>` in the flattened form of common.schema.json (`unknown` has `reason` and no
/// `value`; the others have `value`). Rust passes it through: `value` is kept as JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    pub kind: EvidenceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<UnknownReason>,
}

impl<'de> Deserialize<'de> for Evidence {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            kind: EvidenceKind,
            #[serde(default)]
            value: Option<serde_json::Value>,
            #[serde(default)]
            source_ref: Option<String>,
            #[serde(default)]
            rule_id: Option<String>,
            #[serde(default)]
            reason: Option<UnknownReason>,
        }
        let r = Raw::deserialize(d)?;
        let ok = match r.kind {
            EvidenceKind::Unknown => r.reason.is_some() && r.value.is_none(),
            _ => r.value.is_some(),
        };
        if !ok {
            return Err(serde::de::Error::custom(
                "Evidence: unknown needs reason and no value; confirmed / inferred need value",
            ));
        }
        Ok(Evidence {
            kind: r.kind,
            value: r.value,
            source_ref: r.source_ref,
            rule_id: r.rule_id,
            reason: r.reason,
        })
    }
}

/// `Maneuver` (04 §2). Normalized by C++ (04 §2.1); Rust only orders and selects them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maneuver {
    pub id: ManeuverId,
    pub anchor: RouteAnchor,
    pub arrow: Arrow,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub junction_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub road_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_ref: Option<String>,
    pub side_of_traffic: SideOfTraffic,
    pub crossing: Evidence,
    pub approach_signal: Evidence,
    pub right_turn_protection: Evidence,
    pub approach_width_mm: Evidence,
    pub departure_width_mm: Evidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roundabout_leave_anchor: Option<RouteAnchor>,
    /// TODO(schema-v1): Flag enum not defined.
    pub advisory_flags: Vec<String>,
}

/// Japan only: `left` (04 §2 `side_of_traffic: left`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideOfTraffic {
    Left,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EtaRange {
    pub earliest: u32,
    pub typical: u32,
    pub latest: u32,
}

/// `HorizonEntry` (04 §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HorizonEntry {
    pub maneuver: Maneuver,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<ManeuverId>,
    /// First entry: on-route distance from the vehicle (m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_vehicle_m: Option<u32>,
    /// Second / third entry: distance from the previous maneuver (m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seg_from_prev_m: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_range_ms: Option<EtaRange>,
    pub action_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avoid_offer: Option<AvoidOffer>,
}

/// `status: tracking | uncertain | off_route | rerouting | finished` (04 §2 / §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HorizonStatus {
    Tracking,
    Uncertain,
    OffRoute,
    Rerouting,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComplexWindow {
    pub start_monotonic_ms: MonoMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_monotonic_ms: Option<MonoMs>,
}

/// `JunctionRef` (target of `jp=next`). TODO(schema-v1): `kind` enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JunctionRef {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub maneuver_id: ManeuverId,
    pub kind: String,
    pub deadline_mono_ms: MonoMs,
}

/// `PassedRef` (target of `jp=last`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassedRef {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub maneuver_id: ManeuverId,
    pub passed_at_mono_ms: MonoMs,
    pub passed_along_cm: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<JunctionRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<PassedRef>,
}

/// Maximum number of `upcoming` entries (04 §2 `[HorizonEntry; 0..3]`).
pub const UPCOMING_MAX: usize = 3;

/// `ManeuverHorizon` v1 (04 §2). Returned as `TickOutput.horizon` only when `frame_seq`
/// changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManeuverHorizon {
    /// Always 1.
    #[serde(deserialize_with = "schema_version_1")]
    pub schema_version: u8,
    pub route_key: RouteKey,
    pub frame_seq: u64,
    pub sampled_at_monotonic_ms: MonoMs,
    pub valid_until_monotonic_ms: MonoMs,
    pub status: HorizonStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_progress_cm: Option<u32>,
    #[serde(deserialize_with = "upcoming_max_3")]
    pub upcoming: Vec<HorizonEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_p1_deadline_monotonic_ms: Option<MonoMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_complex_window: Option<ComplexWindow>,
    pub refs: Refs,
}

fn schema_version_1<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    match u8::deserialize(d)? {
        1 => Ok(1),
        v => Err(serde::de::Error::custom(format!("schema_version {v} != 1"))),
    }
}

fn upcoming_max_3<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<HorizonEntry>, D::Error> {
    let v = Vec::<HorizonEntry>::deserialize(d)?;
    if v.len() > UPCOMING_MAX {
        return Err(serde::de::Error::custom(format!(
            "upcoming has {} entries (max {UPCOMING_MAX})",
            v.len()
        )));
    }
    Ok(v)
}
