//! `CandidateFacts` (schema/candidate-facts.schema.json): the per-candidate scoring input
//! C++ generates once; Kotlin passes it as bytes to `dc_rank` and `dc_avoid_offer_eval`
//! (02 §2.1). Rust reads it, it never re-serialises it for C++.

use crate::horizon::{AvoidOffer, Evidence};
use crate::{MonoMs, U64Str};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateFacts {
    pub candidate_id: String,
    pub label: FactLabel,
    pub plan_request_id: String,
    pub graph_version: String,
    pub fingerprint: String,
    pub legal: bool,
    pub complete: bool,
    pub eta_ms: u64,
    pub length_m: u64,
    pub counts: FactCounts,
    pub narrow_m: NarrowMeters,
    pub unknown_width_residential_m: u64,
    pub unknown_length_m: u64,
    pub junctions: Vec<FactJunction>,
    pub through_anchors: Vec<ThroughAnchor>,
}

/// `label` of a candidate (the schema also has `engine_alternate`, which
/// `CandidateRequest` folds into the baseline call).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactLabel {
    Baseline,
    EngineAlternate,
    CostingVariant,
    ThroughAnchor,
}

/// `counts` (names = the junction classes, 04 §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactCounts {
    pub right_unprotected: u64,
    pub right_signal_unknown_protection: u64,
    pub right_signal_unknown: u64,
    pub right_protected: u64,
    pub right_entry: u64,
    pub merges: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NarrowMeters {
    pub confirmed: u64,
    pub inferred: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactJunction {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub edge_in: U64Str,
    pub edge_out: U64Str,
    /// TODO(schema-v1): closed enum not defined yet.
    pub kind: String,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThroughAnchor {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub edge: U64Str,
}

/// `dc_avoid_offer_eval(s, {req_id, now_mono_ms, alt_facts, base_facts})` (02 §2.1 #11,
/// docs 2a8a727). Kotlin calls it for `planner_cmds: evaluate_offer{req_id, facts_ref,
/// base_facts_ref}`: `alt_facts` is the `CandidateFacts` behind `facts_ref` (the
/// alternative), `base_facts` the one behind `base_facts_ref` (the current route from the
/// search-start position to the destination). `req_id` must be the waiting avoid search.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvoidOfferEvalRequest {
    pub req_id: String,
    pub now_mono_ms: MonoMs,
    pub alt_facts: CandidateFacts,
    pub base_facts: CandidateFacts,
}

/// Result of `dc_avoid_offer_eval`, tagged by `"result"` like `dc_validate_override`.
/// `offered`: the `AvoidOffer` now sits on its `HorizonEntry` (the next `dc_tick` returns
/// the frame; the horizon is the only place it is shown). `declined`: no offer, the waiting
/// result is dropped. TODO(abi-v1): the result shape is not in docs (provisional).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum AvoidOfferEvalResult {
    Offered { avoid_offer: AvoidOffer },
    Declined { reason: AvoidDeclineReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvoidDeclineReason {
    /// No avoid-chip search result is waiting for evaluation (or `req_id` is not it).
    NoPendingResult,
    /// The session is not `tracking` (03 §3.1: results are used only while tracking).
    NotTracking,
    /// The decision deadline (20 s before the branch, from `earliest`) has passed.
    DeadlinePassed,
    /// `alt_facts.graph_version` is not the current route's.
    GraphMismatch,
    /// `alt_facts` is not a legal, complete candidate (03 §3.2).
    NotLegalOrComplete,
    /// The extra time exceeds the detour cap (03 §3.2, base_eta = remaining ETA).
    OverDetourCap,
}
