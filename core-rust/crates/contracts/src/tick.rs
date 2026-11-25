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
use crate::common::{required_nullable, MonoMs, RouteKey};
use crate::horizon::ManeuverHorizon;
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

/// GNSS fix, integers only (docs 04 §3.1, ADR-0008): Kotlin rounds the `Location` doubles
/// once, so everything from here on is deterministic integer arithmetic.
/// - `lat_e7` / `lon_e7` = `Math.round(deg * 1e7)` (WGS84 degrees * 1e7).
/// - `acc_cm` = horizontal accuracy in cm.
/// - `bearing_cdeg` = `Math.round(deg * 100) % 36000` (centidegrees clockwise from north,
///   `0..=35999`; 359.996° rounds to 36000 and wraps to 0). 36000 or more is rejected.
/// - `speed_cm_s` = ground speed in cm/s.
///
/// Out-of-range values are rejected at parse time (→ `E_SCHEMA`), like the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fix {
    pub at_mono_ms: MonoMs,
    #[serde(deserialize_with = "lat_e7")]
    pub lat_e7: i32,
    #[serde(deserialize_with = "lon_e7")]
    pub lon_e7: i32,
    pub acc_cm: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "bearing_cdeg"
    )]
    pub bearing_cdeg: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_cm_s: Option<u32>,
}

/// `|lat_e7| <= 900_000_000`.
pub const LAT_E7_MAX: i32 = 900_000_000;
/// `|lon_e7| <= 1_800_000_000`.
pub const LON_E7_MAX: i32 = 1_800_000_000;
/// Bearings are `0..BEARING_CDEG_FULL` (36000 = 360.00°).
pub const BEARING_CDEG_FULL: u16 = 36_000;

fn ranged_i32<'de, D: serde::Deserializer<'de>>(
    d: D,
    max: i32,
    what: &str,
) -> Result<i32, D::Error> {
    let v = i32::deserialize(d)?;
    if v < -max || v > max {
        return Err(serde::de::Error::custom(format!(
            "{what} out of range: {v}"
        )));
    }
    Ok(v)
}

fn lat_e7<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    ranged_i32(d, LAT_E7_MAX, "lat_e7")
}

fn lon_e7<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    ranged_i32(d, LON_E7_MAX, "lon_e7")
}

fn bearing_cdeg<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u16>, D::Error> {
    let v = u16::deserialize(d)?;
    if v >= BEARING_CDEG_FULL {
        return Err(serde::de::Error::custom(format!(
            "bearing_cdeg out of range (0..=35999): {v}"
        )));
    }
    Ok(Some(v))
}

/// `Event = { at_mono_ms } & (kind-specific fields)`.
///
/// Unknown fields are rejected like `additionalProperties: false` in tick.schema.json.
/// serde cannot combine `#[serde(flatten)]` with `deny_unknown_fields`, so `Deserialize`
/// is hand-written: it reads the object once (duplicate keys rejected), takes
/// `at_mono_ms`, and hands the rest to `EventBody`, whose variants each reject unknown
/// fields (`deny_unknown_fields` on the internally tagged enum; field-less kinds are
/// `{}` struct variants because unit variants would silently ignore extra fields).
/// `Serialize` still uses `flatten` (only deserialization has the restriction).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub at_mono_ms: MonoMs,
    #[serde(flatten)]
    pub body: EventBody,
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut obj = d.deserialize_map(StrictObject)?;
        let at = obj
            .remove("at_mono_ms")
            .ok_or_else(|| D::Error::missing_field("at_mono_ms"))?;
        let at_mono_ms = MonoMs::deserialize(at).map_err(D::Error::custom)?;
        let body =
            EventBody::deserialize(serde_json::Value::Object(obj)).map_err(D::Error::custom)?;
        Ok(Event { at_mono_ms, body })
    }
}

/// JSON object reader that rejects duplicate keys (derived structs do the same; a plain
/// `serde_json::Map` would keep the last value silently).
struct StrictObject;

impl<'de> serde::de::Visitor<'de> for StrictObject {
    type Value = serde_json::Map<String, serde_json::Value>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an event object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Self::Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(k) = m.next_key::<String>()? {
            if out.contains_key(&k) {
                return Err(serde::de::Error::custom(format!("duplicate field `{k}`")));
            }
            let v: serde_json::Value = m.next_value()?;
            out.insert(k, v);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
    LowMemory {},
    InferenceDown {
        reason: String,
    },
    InferenceUp {
        tier: u8,
    },
    AudioFocusLost {},
    CallState {
        active: bool,
    },
    /// The single terminal event: exactly one per started search.
    SearchFinished {
        req_id: String,
        route_key: RouteKey,
        graph_version: String,
        outcome: SearchOutcome,
        /// Reference to the CandidateFacts JSON C++ produced once for this search (Kotlin
        /// keeps the bytes; Rust only collects the refs and hands them back in `rank`).
        /// Renamed from `comparison_ref` (team decision 2026-11-25; docs still say comparison_ref).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        facts_ref: Option<String>,
        /// Wall time of the Valhalla call (measurement only, never used for decisions).
        engine_ms: u64,
    },
    /// Kotlin re-created the Valhalla actor. Does NOT by itself resume searching
    /// (03 §3.1: the slot is freed only by the real `search_finished`).
    EngineReset {},
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
#[serde(deny_unknown_fields)]
pub struct TickOutput {
    /// Only present when `frame_seq` changed (04 §2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon: Option<ManeuverHorizon>,
    /// Instructions to Kotlin's single TTS queue (05 §5). Nothing emits them yet.
    pub voice_cmds: Vec<VoiceCmd>,
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

/// `planner_cmds: start{req_id, priority, distance_class, route_key, graph_version,
/// candidate_id?} | cancel{req_id} | rank{plan_request_id, facts_refs, reason}
/// | commit{req_id, route_key}` (03 §3.1; `rank`, `candidate_id` and `commit` are team
/// decisions 2026-11-25, not in docs yet).
/// `distance_class` is required: for a plan's candidate search, the class the Planner fixed
/// for the plan before searching; for a search outside a plan (reroute / avoid chip), the
/// class fixed at its start from the valid remaining ETA on the current route (`< 1800 s` →
/// short; `>= 1800 s`, unknown or invalidated by off-route → long; team decision 2026-11-25).
/// TODO(abi-v1): tag field name `cmd` is provisional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlannerCmd {
    Start {
        req_id: String,
        priority: SearchPriority,
        distance_class: DistanceClass,
        route_key: RouteKey,
        graph_version: String,
        /// Which `CandidateRequest` of the current plan to run (`Session::plan_candidates`
        /// output). Absent for searches outside a plan (reroute / avoid chip).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        candidate_id: Option<String>,
        /// Current position for a `reroute` (04 §3.1: Rust decided the vehicle left the
        /// route and hands the fix that confirmed it to Valhalla as the new origin).
        /// Absent for other searches.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fix: Option<Fix>,
    },
    Cancel {
        req_id: String,
    },
    /// Rank the plan's completed candidates now: Kotlin calls `dc_rank` with the
    /// CandidateFacts behind `facts_refs` (the `facts_ref` of every search of this plan that
    /// returned `ok`, in request order; may be empty). After `rank` the plan is over: no
    /// more `start` for it, and its late `search_finished` only frees the slot
    /// (`discarded_late`). Emitted even when `facts_refs` is empty.
    /// Team decision 2026-11-25 (not in docs yet).
    Rank {
        plan_request_id: String,
        facts_refs: Vec<String>,
        reason: RankReason,
    },
    /// Commit the route found by search `req_id`: Kotlin calls `dc_route_commit` with the
    /// route body of that search. Emitted only for a `reroute` / ad-hoc `explicit_plan`
    /// search that returned `ok` with a still valid state (not cancelled / stuck, its
    /// `route_key` equal to the current route). For an avoid-chip search only after
    /// consent (`dc_avoid_offer_eval` put its `AvoidOffer` on the horizon, then
    /// `user_tap{target_id: offer_id, action: avoid}` arrived before the deadline with the
    /// offer's `route_key` still current; 03 §3.1 / 04 §4.3). Never for a plan's candidate
    /// (→ `rank`).
    /// `route_key` is the current route's key *when the search started* (not the new
    /// route's); `dc_route_commit` re-checks it against the current route and returns
    /// `E_STALE` without replacing anything when the route changed meanwhile.
    /// Team decision 2026-11-25 (not in docs yet).
    Commit {
        req_id: String,
        route_key: RouteKey,
    },
}

/// Why `rank` was emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankReason {
    /// Every candidate search of the plan has returned.
    AllDone,
    /// The comparison budget (03 §3.1 item 3: short 3 s / long 8 s from the plan start)
    /// passed first: remaining queued candidates are dropped, a running one is cancelled.
    BudgetExpired,
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
/// Outside a plan (reroute / avoid chip) it is fixed at each search start from the valid
/// remaining ETA on the current route and selects only the watchdog limit (team decision
/// 2026-11-25).
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

/// Minimal UI catalogue: only what docs v1.2 names. Drawing state that already lives in
/// `ManeuverHorizon` (3-ahead cards, `HorizonEntry.avoid_offer` chip, "経路を確認中" =
/// `horizon.status` off_route / rerouting) is NOT repeated here: the horizon is the single
/// source (04 §1, ADR-0005), returned as `TickOutput.horizon`. The former
/// `show_checking_route` is abolished (ADR-0008).
/// TODO(abi-v1): tag field name `ui` is provisional; see core-rust/README.md for the items
/// docs do not define yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ui", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiCmd {
    /// Present a validated override on the confirm screen Kotlin opened (05 §3.2: only
    /// while `route_key` matches and that screen is still valid; ADR-0007: confirmation
    /// itself only comes from dict `yes` or `user_tap`). `expires_at_mono_ms` is the
    /// `Applied.expires_at_mono_ms` of dc_validate_override (03 §4.2 item 4: ≤ 30 s).
    ShowConfirm {
        confirm_screen_id: String,
        override_id: String,
        expires_at_mono_ms: MonoMs,
    },
    /// Remove a shown offer / confirmation at once. `target_id` uses the `user_tap`
    /// namespace: `AvoidOffer.offer_id` | `override_id` | `confirm_screen_id` (ADR-0008).
    Dismiss {
        target_id: String,
        reason: DismissReason,
    },
}

/// Why a shown item is removed; only the cases docs v1.2 name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DismissReason {
    /// Deadline / expiry passed (04 §4.3 item 3 "締切経過後は即座に消す", 05 §3.2 期限切れ).
    Expired,
    /// Cancelled (05 §3.2 取消, 03 §4.1 `CXL`).
    Cancelled,
    /// The route changed meanwhile (05 §3.2 経路更新後, Gate 4/7 "承認直前に逸脱").
    RouteChanged,
    /// Replaced by a newer proposal; consent never carries over (ADR-0007 decision 7).
    Superseded,
}

/// Voice command for Kotlin's single TTS queue (05 §5). The text is never in the command:
/// Kotlin renders `template_id` + numeric `params` with its templates, the same wording as
/// the screen (04 §2 `AvoidOffer.summary`, ADR-0007 decision 8). No raw text crosses the
/// boundary (and so none reaches the log, ADR-0008 decision 7).
/// TODO(abi-v1): tag field name `voice` is provisional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "voice", rename_all = "snake_case", deny_unknown_fields)]
pub enum VoiceCmd {
    /// Start an utterance. Kotlin reports `tts_started` / `tts_done` / `tts_stopped` with
    /// the same `token`; callbacks of older generations are ignored (05 §5).
    Speak {
        token: String,
        priority: VoicePriority,
        template_id: String,
        /// Numbers only (ids are in `token` / `template_id`). Sorted keys: deterministic.
        params: std::collections::BTreeMap<String, i64>,
    },
    /// Silence the utterance now (P3 on P0/P1 / call / off-route / stop operation, 05 §5).
    /// Kotlin must not wait for `onStop` before the next P1 (05 §5 TTS safety rule).
    Stop { token: String },
}

/// `P0 > P1 > P2 > P3` (05 §5). `Ord` follows it (higher wins); variant order below is
/// therefore lowest → highest. JSON names are snake_case `p0`..`p3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoicePriority {
    /// 長距離同意済みの歴史・地名解説.
    P3,
    /// 許可済み音声コマンド応答・回避提案予告.
    P2,
    /// 右左折・分岐・再探索案内 (ManeuverHorizon 連動).
    P1,
    /// 案内継続に必要な緊急状態・危険注意.
    P0,
}

/// Structured, privacy-safe log (no raw utterance text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogEvent {
    pub at_mono_ms: MonoMs,
    pub what: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub req_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
}
