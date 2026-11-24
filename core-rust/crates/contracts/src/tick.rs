//! `dc_tick` v2 contract. Canonical: docs v1.2 ADR-0008 (decision 2/3/6), 03 §3.1.
//!
//! ```text
//! TickInput  = { now_mono_ms, fix?: Fix, events: [Event] }   // events in at_mono_ms ascending order
//! TickOutput = { horizon?, voice_cmds, ui_cmds, planner_cmds, next_wake_mono_ms: u64 | null, log }
//! ```
//! - `*_mono_ms` (`MonoMs`) are JSON numbers; enum values are snake_case.
//! - `reroute_result` / `search_cancelled` are abolished: one `search_finished` per search.
//! - There is no `timeout` outcome: the watchdog deadline is detected in Rust from
//!   `now_mono_ms` (→ `engine_stuck`); Kotlin only calls `dc_tick` by `next_wake_mono_ms`.
use crate::common::{MonoMs, RouteKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TickInput {
    pub now_mono_ms: MonoMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
    #[serde(default)]
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fix {
    pub at_mono_ms: MonoMs,
    pub lat: f64,
    pub lon: f64,
    pub acc_m: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearing: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
}

/// `Event = { at_mono_ms } & (kind-specific fields)`.
/// NOTE: `#[serde(flatten)]` prevents `deny_unknown_fields` on the outer struct;
/// variants still reject unknown `kind` values. TODO(abi-v1): decide strictness.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Event {
    pub at_mono_ms: MonoMs,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventBody {
    // TODO(abi-v1): token / utterance_id / tier / reason value types are not specified (strings/u8 provisional).
    TtsStarted {
        token: String,
    },
    TtsDone {
        token: String,
    },
    TtsStopped {
        token: String,
    },
    AsrStarted {
        utterance_id: String,
    },
    /// Dictionary matching ("はい" etc.) is done in Rust; only the normalized string comes in.
    /// Raw utterance text is never logged (only text_class + dictionary id).
    AsrResult {
        utterance_id: String,
        text_class: TextClass,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dict_norm: Option<String>,
    },
    AsrFailed {
        utterance_id: String,
    },
    OverrideReady {
        utterance_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        override_id: Option<String>,
        status: OverrideReadyStatus,
        confirm_screen_id: String,
    },
    /// Only taps come from Kotlin as consent; dict "yes" consent is produced inside Rust.
    /// `target_id` = `AvoidOffer.offer_id` (avoid chip) | `override_id` | `confirm_screen_id`
    /// (ADR-0008, 03 §4.2). Re-validated on confirm; never carried over to a replaced offer.
    UserTap {
        target_id: String,
        action: TapAction,
    },
    Thermal {
        status: ThermalStatus,
    },
    LowMemory,
    InferenceDown {
        reason: String,
    },
    InferenceUp {
        tier: u8,
    },
    AudioFocusLost,
    CallState {
        active: bool,
    },
    /// The single terminal event: exactly one per started search.
    SearchFinished {
        req_id: String,
        route_key: RouteKey,
        graph_version: String,
        outcome: SearchOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comparison_ref: Option<String>,
        /// Wall time of the Valhalla call (measurement only, never used for decisions).
        engine_ms: u64,
    },
    /// Kotlin re-created the Valhalla actor. Does NOT by itself resume searching
    /// (03 §3.1: the slot is freed only by the real `search_finished`).
    EngineReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextClass {
    Dict,
    Natural,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideReadyStatus {
    Ok,
    Timeout,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TapAction {
    Confirm,
    Avoid,
    Undo,
}

/// Android THERMAL_STATUS_* (7 levels; to be confirmed in Gate 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThermalStatus {
    None,
    Light,
    Moderate,
    Severe,
    Critical,
    Emergency,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchOutcome {
    Ok,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TickOutput {
    /// Only present when frame_seq changed. TODO(abi-v1): typed ManeuverHorizon (schema/maneuver-horizon.schema.json).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub horizon: Option<serde_json::Value>,
    /// TODO(abi-v1): VoiceCmd shape not specified.
    pub voice_cmds: Vec<serde_json::Value>,
    pub ui_cmds: Vec<UiCmd>,
    pub planner_cmds: Vec<PlannerCmd>,
    /// Earliest mono time at which the output may change without new input, including the
    /// search watchdog deadline. Kotlin calls dc_tick by then (earlier is fine).
    /// `null` = nothing to wait for (tick on each Fix only). ADR-0008 decision 3.
    /// Always present on the wire (explicit `null`), so a missing field is rejected.
    #[serde(deserialize_with = "required_nullable")]
    pub next_wake_mono_ms: Option<MonoMs>,
    pub log: Vec<LogEvent>,
}

/// `planner_cmds: start{req_id, priority, route_key, graph_version} | cancel{req_id}` (03 §3.1).
/// TODO(abi-v1): tag field name `cmd` is provisional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum PlannerCmd {
    Start {
        req_id: String,
        priority: SearchPriority,
        route_key: RouteKey,
        graph_version: String,
    },
    Cancel {
        req_id: String,
    },
}

/// Single priority queue order (03 §3.1 table; team decision 2026-11-24 for the JSON names).
/// Highest first: `reroute` (逸脱再探索) > `explicit_plan` (明示計画) > `avoid_chip`
/// (回避チップ) > `comparison` (比較探索). `Ord` follows the same order (higher wins);
/// variant order below is therefore lowest → highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPriority {
    Comparison,
    AvoidChip,
    ExplicitPlan,
    Reroute,
}

/// Distance class fixed by the Planner *before* searching (team decision 2026-11-24).
/// It selects the comparison budget (03 §3.1 item 3, 07 §1: short 3 s / long 8 s, counted
/// from the plan start) and the native watchdog limit (03 §3.1, 07 §1: short 5 s /
/// long 10 s, counted from each native call's start). Fixed for all candidate searches of
/// the same plan_request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistanceClass {
    Short,
    Long,
}

impl DistanceClass {
    /// Pre-search ETA estimate threshold: `< 1800 s` (30 min) → short.
    pub const SHORT_ETA_LIMIT_S: u64 = 1800;

    /// `eta_estimate_s < 1800` → short; `>= 1800` or no estimate → long.
    pub fn from_eta_estimate_s(eta_estimate_s: Option<u64>) -> Self {
        match eta_estimate_s {
            Some(s) if s < Self::SHORT_ETA_LIMIT_S => Self::Short,
            _ => Self::Long,
        }
    }
}

/// TODO(abi-v1): UiCmd catalogue not specified; only what the planner needs today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ui", rename_all = "snake_case")]
pub enum UiCmd {
    /// "経路を確認中" (Gate 7 t1, engine_stuck).
    ShowCheckingRoute,
    /// Adopted search result; Kotlin performs the atomic commit via dc_route_commit.
    SearchResultReady {
        req_id: String,
        comparison_ref: Option<String>,
    },
}

/// Structured, privacy-safe log (no raw utterance text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEvent {
    pub at_mono_ms: MonoMs,
    pub what: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub req_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
}

/// `Option<T>` that must be present (may be `null`). Without `deserialize_with`, serde would
/// treat a missing `Option` field as `None`; tick.schema.json requires the key.
fn required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}
