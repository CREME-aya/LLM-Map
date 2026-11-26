//! dc_validate_override request/response and the expanded PersonalRoutingOverride.
use crate::common::{MonoMs, RouteKey, U64Str};
use serde::{Deserialize, Serialize};

/// Request of dc_validate_override (docs v1.2 02 §2.1 / 03 §4.2:
/// `{code_line, contract, utterance_id, utterance_mono_ms, ref_frame_seq, pending_offer_id?}`).
/// TODO(abi-v1): `contract` shape (IntentContract) is not defined; kept as raw JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateOverrideRequest {
    pub code_line: String,
    pub contract: serde_json::Value,
    /// Required: same id as `asr_started` / `asr_result` (Gate 4 latency matching).
    pub utterance_id: String,
    pub utterance_mono_ms: MonoMs,
    /// `ManeuverHorizon.frame_seq` the refs were read from. JSON number, not U64Str
    /// (04 §2; team agreement 2026-11-24: frame_seq stays numeric).
    pub ref_frame_seq: u64,
    #[serde(default)]
    pub pending_offer_id: Option<String>,
}

/// Rejection reasons (03 §4.2, 7 kinds). Fixed uppercase strings as written in the docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RejectCode {
    Syntax,
    UnknownRef,
    StaleRevision,
    Expired,
    ForbiddenWhileDriving,
    NoEffect,
    NoPendingTarget,
}

/// `Applied | Rejected` are type names; JSON carries the discriminator
/// `"result": "applied" | "rejected"` (ADR-0008 decision 1, 02 §2.1).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // boundary DTO, built once per call
pub enum ValidateOverrideResult {
    Applied {
        override_id: String,
        #[serde(rename = "override")]
        override_: PersonalRoutingOverride,
        #[serde(skip_serializing_if = "Option::is_none")]
        review_proposal: Option<serde_json::Value>,
        route_key: RouteKey,
        expires_at_mono_ms: MonoMs,
    },
    Rejected {
        code: RejectCode,
    },
    /// `CXL` taken as "戻して" of an avoid switch confirmed within 5 s (03 §3.1):
    /// `commit{req_id, route_key: current}` comes with the next `dc_tick` (`req_id` =
    /// `undo-<req_id>`). Before the avoid route was loaded: `cancel_commit{req_id}` instead,
    /// `req_id` = the consent commit's (docs 836a370).
    /// TODO(abi-v1): shape provisional (team decision 2026-11-26 did not name it).
    Undone {
        req_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Low,
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detour {
    Standard,
    Tight,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Weights {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub crossing_right: Option<Level>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub narrow_road: Option<Level>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub merge: Option<Level>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JunctionPreference {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub mode: JunctionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JunctionMode {
    AvoidIfPractical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideAction {
    SuggestRoutePreference,
}

/// Mirror of schema/personal-routing-override.schema.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalRoutingOverride {
    pub schema_version: SchemaV1,
    pub intent_contract_id: String,
    pub plan_request_id: String,
    pub trip_revision: U64Str,
    pub action: OverrideAction,
    pub weights: Weights,
    pub junction_preferences: Vec<JunctionPreference>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub through_ref: Option<String>,
    pub detour: Detour,
    pub expires_at_mono_ms: MonoMs,
}

/// `schema_version: 1` (const). Deserialization fails on any other value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchemaV1;

impl Serialize for SchemaV1 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(1)
    }
}

impl<'de> Deserialize<'de> for SchemaV1 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match u8::deserialize(d)? {
            1 => Ok(SchemaV1),
            v => Err(serde::de::Error::custom(format!("schema_version {v} != 1"))),
        }
    }
}
