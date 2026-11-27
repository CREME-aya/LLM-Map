//! Safe, byte-in/byte-out operations shared by the C ABI and JNI layers.
//! Every function returns a complete `{ok, data | error}` envelope.

use contracts::facts::AvoidOfferEvalRequest;
use contracts::override_types::ValidateOverrideRequest;
use contracts::plan::PlanCandidatesRequest;
use contracts::rank::RankRequest;
use contracts::route::RouteCommitRequest;
use contracts::tick::TickInput;
use contracts::trip_score::TripScoreRequest;
use contracts::{envelope_err, envelope_ok, ErrorCode, RouteKey};
use drive_core::{CoreError, Session};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Opaque `dc_session`. The magic word lets us reject obviously wrong handles
/// with E_INVALID_HANDLE (best effort only; a freed pointer is still UB).
pub struct DcSession {
    magic: u64,
    pub(crate) inner: Session,
}

const MAGIC: u64 = 0x6463_5f73_6573_7331; // "dc_sess1"

impl DcSession {
    pub fn new(inner: Session) -> Self {
        Self {
            magic: MAGIC,
            inner,
        }
    }
    pub fn is_valid(&self) -> bool {
        self.magic == MAGIC
    }
}

impl Drop for DcSession {
    fn drop(&mut self) {
        self.magic = 0;
    }
}

/// Run `f`, converting a panic into an E_PANIC envelope.
pub fn guard(f: impl FnOnce() -> Vec<u8>) -> Vec<u8> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            envelope_err(ErrorCode::Panic, &msg)
        }
    }
}

fn err(e: CoreError) -> Vec<u8> {
    envelope_err(e.code, &e.msg)
}

pub fn session_new(config: &[u8]) -> (Vec<u8>, Option<Box<DcSession>>) {
    match Session::from_json(config) {
        Ok(s) => (
            envelope_ok(&serde_json::json!({})),
            Some(Box::new(DcSession::new(s))),
        ),
        Err(e) => (err(e), None),
    }
}

pub fn with_session(s: Option<&mut DcSession>, f: impl FnOnce(&mut Session) -> Vec<u8>) -> Vec<u8> {
    match s {
        Some(s) if s.is_valid() => f(&mut s.inner),
        _ => envelope_err(ErrorCode::InvalidHandle, "null or invalid dc_session"),
    }
}

pub fn tick(s: &mut Session, input: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<TickInput>(input) {
        Ok(t) => envelope_ok(&s.tick(t)),
        Err(e) => envelope_err(ErrorCode::Schema, &format!("tick: {e}")),
    }
}

pub fn validate_override(s: &mut Session, input: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<ValidateOverrideRequest>(input) {
        Ok(r) => match s.validate_override(r) {
            Ok(res) => envelope_ok(&res),
            Err(e) => err(e),
        },
        Err(e) => envelope_err(ErrorCode::Schema, &format!("validate_override: {e}")),
    }
}

/// `dc_plan_candidates(s, {now_mono_ms, eta_estimate_s?, snapshot})` →
/// `Session::plan_candidates` result.
pub fn plan_candidates(s: &mut Session, input: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<PlanCandidatesRequest>(input) {
        Ok(r) => envelope_ok(&s.plan_candidates(r.now_mono_ms, r.eta_estimate_s, &r.snapshot)),
        Err(e) => envelope_err(ErrorCode::Schema, &format!("plan_candidates: {e}")),
    }
}

/// `dc_route_commit`: `{req_id, expected_route_key, bundle}` (`contracts::route::RouteCommitRequest`).
/// `req_id` is mandatory (`null` / omitted → `E_SCHEMA`). The guard is checked before the
/// body (`E_STALE` or `E_FORBIDDEN_WHILE_DRIVING`, nothing replaced): `expected_route_key`
/// against the current route, and `req_id` against unsettled commits Rust issued or `ok`
/// candidates of the current plan while its selection window is open; then the request is
/// parsed (`E_SCHEMA`, including a bundle `route_revision` other than "0"), and
/// `Session::route_commit` checks trip / graph (`E_STALE`) and the bundle body (`E_SCHEMA`),
/// assigns the revision (current + 1) and makes it the current route.
pub fn route_commit(s: &mut Session, input: &[u8]) -> Vec<u8> {
    let v: serde_json::Value = match serde_json::from_slice(input) {
        Ok(v) => v,
        Err(e) => return envelope_err(ErrorCode::Schema, &format!("route_commit: {e}")),
    };
    // The guard is checked before the body so a late route is refused as E_STALE (or
    // E_FORBIDDEN_WHILE_DRIVING) even if its body would not parse.
    let key = match v.get("expected_route_key").cloned() {
        Some(serde_json::Value::Null) => None,
        Some(k) => match serde_json::from_value::<RouteKey>(k) {
            Ok(k) => Some(k),
            Err(e) => return envelope_err(ErrorCode::Schema, &format!("route_commit: {e}")),
        },
        None => {
            return envelope_err(
                ErrorCode::Schema,
                "route_commit: missing field `expected_route_key`",
            )
        }
    };
    let req_id = match v.get("req_id") {
        Some(serde_json::Value::String(r)) => r.as_str(),
        Some(_) => return envelope_err(ErrorCode::Schema, "route_commit: req_id: expected string"),
        None => return envelope_err(ErrorCode::Schema, "route_commit: missing field `req_id`"),
    };
    if let Err(e) = s.check_route_guard(req_id, key.as_ref()) {
        return err(e);
    }
    let req: RouteCommitRequest = match serde_json::from_value(v) {
        Ok(r) => r,
        Err(e) => return envelope_err(ErrorCode::Schema, &format!("route_commit: {e}")),
    };
    match s.route_commit(&req) {
        Ok(r) => envelope_ok(&r),
        Err(e) => err(e),
    }
}

/// `dc_avoid_offer_eval(s, {req_id, now_mono_ms, alt_facts, base_facts})` → `{"result": "offered", avoid_offer}`
/// | `{"result": "declined", reason}` (`Session::avoid_offer_eval`). A malformed request
/// (including `alt_facts` not matching schema/candidate-facts.schema.json) is `E_SCHEMA`.
pub fn avoid_offer_eval(s: &mut Session, input: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<AvoidOfferEvalRequest>(input) {
        Ok(r) => envelope_ok(&s.avoid_offer_eval(&r)),
        Err(e) => envelope_err(ErrorCode::Schema, &format!("avoid_offer_eval: {e}")),
    }
}

/// `dc_rank({facts[], policy?, profile?})` → `RouteComparison` (02 §2.1 #6, 03 §3.2).
pub fn rank(input: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<RankRequest>(input) {
        Ok(r) => match drive_core::rank::rank(&r) {
            Ok(res) => envelope_ok(&res),
            Err(e) => err(e),
        },
        Err(e) => envelope_err(ErrorCode::Schema, &format!("rank: {e}")),
    }
}

/// `dc_trip_score(req)` → `TripScoreResult` (02 §2.1 #8, 05 §6.1).
pub fn trip_score(input: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<TripScoreRequest>(input) {
        Ok(r) => match drive_core::trip_score::evaluate(&r) {
            Ok(res) => envelope_ok(&res),
            Err(e) => err(e),
        },
        Err(e) => envelope_err(ErrorCode::Schema, &format!("trip_score: {e}")),
    }
}

/// `dc_exchange_validate(json)` → validated `ExchangeDocument` (02 §2.1 #12, 06 §4.2,
/// `schema/exchange.schema.json`). Validates `IntentPreset`, `TripPlan`, or `DriveDiary`
/// (strict unknown fields, WGS84 e7 integer coordinates, `hard_soft` keys ⊆ `chips`,
/// `weights_permille` 250..=4000, total track points <= 20,000, non-decreasing `t_offset_s`
/// across all segments). Note: the 16 MB expanded zip limit is enforced by Kotlin/Web before
/// passing the extracted JSON bytes.
pub fn exchange_validate(input: &[u8]) -> Vec<u8> {
    match drive_core::exchange::validate_json(input) {
        Ok(doc) => envelope_ok(&doc),
        Err(e) => err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_turns_panic_into_e_panic() {
        let out = guard(|| panic!("boom"));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "E_PANIC");
        assert_eq!(v["error"]["msg"], "boom");
    }
}
