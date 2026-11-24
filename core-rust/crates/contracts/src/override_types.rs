//! dc_validate_override request/response and the expanded PersonalRoutingOverride.
use crate::common::{MonoMs, RouteKey, U64Str};
use serde::{Deserialize, Serialize};

/// Request of dc_validate_override (docs v1.2 02 §2.1:
/// `{code_line, contract, utterance_mono_ms, ref_frame_seq, pending_offer_id?}`).
/// TODO(abi-v1): `contract` shape (IntentContract) is not defined; kept as raw JSON.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateOverrideRequest {
    pub code_line: String,
    pub contract: serde_json::Value,
    /// In the #40 header draft but not in docs v1.2 02 §2.1 → optional. TODO(abi-v1): decide.
    #[serde(default)]
    pub utterance_id: Option<String>,
    pub utterance_mono_ms: MonoMs,
    pub ref_frame_seq: U64Str,
    #[serde(default)]
    pub pending_offer_id: Option<String>,
}

/// Rejection reasons (devteam #40 / #37).
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

/// TODO(abi-v1): JSON tagging of Applied | Rejected is not specified; `result` tag is provisional.
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
