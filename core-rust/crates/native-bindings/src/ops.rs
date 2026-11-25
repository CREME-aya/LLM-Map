//! Safe, byte-in/byte-out operations shared by the C ABI and JNI layers.
//! Every function returns a complete `{ok, data | error}` envelope.

use contracts::override_types::ValidateOverrideRequest;
use contracts::tick::TickInput;
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

/// `dc_route_commit`: `{route_key, ...}`. Only the `route_key` re-check is implemented
/// (`E_STALE` when the current route changed since the search started); the rest of the
/// route bundle is not specified yet (TODO(abi-v1)) and is not read.
pub fn route_commit(s: &mut Session, input: &[u8]) -> Vec<u8> {
    let v: serde_json::Value = match serde_json::from_slice(input) {
        Ok(v) => v,
        Err(e) => return envelope_err(ErrorCode::Schema, &format!("route_commit: {e}")),
    };
    let key = match v
        .get("route_key")
        .cloned()
        .map(serde_json::from_value::<RouteKey>)
    {
        Some(Ok(k)) => k,
        Some(Err(e)) => return envelope_err(ErrorCode::Schema, &format!("route_commit: {e}")),
        None => return envelope_err(ErrorCode::Schema, "route_commit: missing field `route_key`"),
    };
    match s.check_route_commit(&key) {
        Ok(()) => unimplemented("dc_route_commit", input),
        Err(e) => err(e),
    }
}

/// Placeholder for functions whose logic is not in this skeleton.
/// Still checks that the input is well-formed JSON so the transport can be tested.
pub fn unimplemented(name: &str, input: &[u8]) -> Vec<u8> {
    if let Err(e) = serde_json::from_slice::<serde_json::Value>(input) {
        return envelope_err(ErrorCode::Schema, &format!("{name}: {e}"));
    }
    envelope_err(
        ErrorCode::Unimplemented,
        &format!("{name}: TODO(abi-v1) not implemented"),
    )
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
