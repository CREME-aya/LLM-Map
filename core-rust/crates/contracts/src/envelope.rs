use serde::Serialize;
use serde_json::{json, Value};

/// Error codes of dc_core ABI v1 (devteam #40).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ErrorCode {
    #[serde(rename = "E_SCHEMA")]
    Schema,
    #[serde(rename = "E_VERSION")]
    Version,
    #[serde(rename = "E_STALE")]
    Stale,
    #[serde(rename = "E_REJECTED")]
    Rejected,
    #[serde(rename = "E_PANIC")]
    Panic,
    #[serde(rename = "E_INVALID_HANDLE")]
    InvalidHandle,
    /// Plan-candidate `dc_route_commit` after the vehicle started driving (02 §2.1 #9).
    /// Distinct from L3 `RejectCode::ForbiddenWhileDriving` (`FORBIDDEN_WHILE_DRIVING`).
    #[serde(rename = "E_FORBIDDEN_WHILE_DRIVING")]
    ForbiddenWhileDriving,
    /// TODO(abi-v1): skeleton-only placeholder for functions not implemented yet.
    /// Not one of the 6 agreed codes; remove before freezing ABI v1.
    #[serde(rename = "E_UNIMPLEMENTED")]
    Unimplemented,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Schema => "E_SCHEMA",
            ErrorCode::Version => "E_VERSION",
            ErrorCode::Stale => "E_STALE",
            ErrorCode::Rejected => "E_REJECTED",
            ErrorCode::Panic => "E_PANIC",
            ErrorCode::InvalidHandle => "E_INVALID_HANDLE",
            ErrorCode::ForbiddenWhileDriving => "E_FORBIDDEN_WHILE_DRIVING",
            ErrorCode::Unimplemented => "E_UNIMPLEMENTED",
        }
    }
}

/// `{"ok":true,"data":...}`
pub fn envelope_ok<T: Serialize>(data: &T) -> Vec<u8> {
    match serde_json::to_value(data) {
        Ok(v) => serde_json::to_vec(&json!({ "ok": true, "data": v })).unwrap_or_default(),
        Err(e) => envelope_err(ErrorCode::Schema, &format!("serialize: {e}")),
    }
}

/// `{"ok":false,"error":{"code":..,"msg":..}}`. `msg` is for logs only (Kotlin branches on `code`).
pub fn envelope_err(code: ErrorCode, msg: &str) -> Vec<u8> {
    let v: Value = json!({ "ok": false, "error": { "code": code.as_str(), "msg": msg } });
    serde_json::to_vec(&v).unwrap_or_default()
}
