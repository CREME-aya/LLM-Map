use contracts::override_types::{RejectCode, ValidateOverrideRequest, ValidateOverrideResult};
use contracts::ErrorCode;
use drive_core::session::ThermalLevel;
use drive_core::{Session, SessionConfig};

fn req(code_line: &str, pending: Option<&str>) -> ValidateOverrideRequest {
    let mut v = serde_json::json!({"code_line":code_line,"contract":{},"utterance_id":"u1","utterance_mono_ms":100,"ref_frame_seq":3});
    if let Some(p) = pending {
        v["pending_offer_id"] = p.into();
    }
    serde_json::from_value(v).unwrap()
}

fn rejected(s: &mut Session, line: &str, pending: Option<&str>) -> Option<RejectCode> {
    match s.validate_override(req(line, pending)) {
        Ok(ValidateOverrideResult::Rejected { code }) => Some(code),
        _ => None,
    }
}

#[test]
fn validate_override_skeleton_rules() {
    let mut s = Session::new(SessionConfig::default());
    assert_eq!(
        rejected(&mut s, "OVR rc=H rc=L", None),
        Some(RejectCode::Syntax)
    );
    assert_eq!(rejected(&mut s, "OVR", None), Some(RejectCode::Syntax));
    assert_eq!(rejected(&mut s, "OVR dt=R", None), Some(RejectCode::Syntax));
    assert_eq!(
        rejected(&mut s, "YES", None),
        Some(RejectCode::NoPendingTarget)
    );
    assert_eq!(
        rejected(&mut s, "CXL", None),
        Some(RejectCode::NoPendingTarget)
    );
    assert_eq!(
        rejected(&mut s, "NO", None),
        Some(RejectCode::NoPendingTarget)
    );
    assert_eq!(
        rejected(&mut s, "OVR jp=next", None),
        Some(RejectCode::UnknownRef)
    );
    assert_eq!(
        rejected(&mut s, "OVR rc=H jp=last", None),
        Some(RejectCode::UnknownRef)
    );
    // Not implemented yet → explicit E_UNIMPLEMENTED, never a silent "applied".
    let e = s.validate_override(req("OVR rc=H", None)).unwrap_err();
    assert_eq!(e.code, ErrorCode::Unimplemented);
    // YES with a pending offer must still not confirm anything here.
    assert!(!matches!(
        s.validate_override(req("YES", Some("o1"))),
        Ok(ValidateOverrideResult::Applied { .. })
    ));
}

#[test]
fn utterance_id_is_required_and_mono_ms_is_a_number() {
    let ok = serde_json::json!({"code_line":"YES","contract":{},"utterance_id":"u1","utterance_mono_ms":100,"ref_frame_seq":3});
    assert!(serde_json::from_value::<ValidateOverrideRequest>(ok.clone()).is_ok());
    let mut no_id = ok.clone();
    no_id.as_object_mut().unwrap().remove("utterance_id");
    assert!(serde_json::from_value::<ValidateOverrideRequest>(no_id).is_err());
    let mut str_ms = ok;
    str_ms["utterance_mono_ms"] = "100".into();
    assert!(serde_json::from_value::<ValidateOverrideRequest>(str_ms).is_err());
}

#[test]
fn rejected_serializes_with_result_tag() {
    let v = serde_json::to_value(ValidateOverrideResult::Rejected {
        code: RejectCode::NoPendingTarget,
    })
    .unwrap();
    assert_eq!(
        v,
        serde_json::json!({"result":"rejected","code":"NO_PENDING_TARGET"})
    );
}

#[test]
fn thermal_collapses_critical_and_above() {
    for (status, lvl) in [
        ("severe", ThermalLevel::Severe),
        ("critical", ThermalLevel::CriticalOrAbove),
        ("emergency", ThermalLevel::CriticalOrAbove),
        ("shutdown", ThermalLevel::CriticalOrAbove),
    ] {
        let mut s = Session::new(SessionConfig::default());
        s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms":1,"events":[{"at_mono_ms":1,"kind":"thermal","status":status}]})).unwrap());
        assert_eq!(s.thermal(), lvl, "{status}");
    }
}

#[test]
fn clock_regression_is_logged() {
    let mut s = Session::new(SessionConfig::default());
    s.tick(serde_json::from_str(r#"{"now_mono_ms":100}"#).unwrap());
    let o = s.tick(serde_json::from_str(r#"{"now_mono_ms":50}"#).unwrap());
    let reg: Vec<_> = o
        .log
        .iter()
        .filter(|l| l.what == "clock_regression")
        .collect();
    assert_eq!(reg.len(), 1);
    assert_eq!(
        reg[0].at_mono_ms, 100,
        "now is clamped to the previous value"
    );
    let o = s.tick(serde_json::from_str(r#"{"now_mono_ms":200,"events":[{"at_mono_ms":190,"kind":"low_memory"},{"at_mono_ms":180,"kind":"low_memory"}]}"#).unwrap());
    assert!(o.log.iter().any(|l| l.what == "clock_regression"));
}

#[test]
fn session_config_rejects_unknown_fields() {
    assert!(Session::from_json(br#"{}"#).is_ok());
    assert!(Session::from_json(br#"{"watchdog_short_ms":4000}"#).is_ok());
    assert_eq!(
        Session::from_json(br#"{"nope":1}"#).unwrap_err().code,
        ErrorCode::Schema
    );
    assert_eq!(
        Session::from_json(b"not json").unwrap_err().code,
        ErrorCode::Schema
    );
}

#[test]
fn out_of_order_event_is_clamped_not_resorted() {
    // r1 running from t=0; a search_finished stamped earlier than a previous event is
    // clamped to that event's time, so the follow-up start (r2) carries the clamped time.
    use contracts::tick::{PlannerCmd, SearchPriority};
    let mut s = Session::new(SessionConfig::default());
    let rk = serde_json::json!({"trip_id":"t","trip_revision":"1","route_revision":"1","graph_version":"g"});
    s.set_current_route_key(serde_json::from_value(rk.clone()).unwrap());
    s.request_search(0, SearchPriority::AvoidChip).unwrap();
    s.request_search(1, SearchPriority::Reroute).unwrap();
    let o = s.tick(serde_json::from_value(serde_json::json!({"now_mono_ms":300,"events":[
        {"at_mono_ms":200,"kind":"low_memory"},
        {"at_mono_ms":150,"kind":"search_finished","req_id":"r1","route_key":rk,"graph_version":"g","outcome":"cancelled","engine_ms":1}]})).unwrap());
    assert!(matches!(&o.planner_cmds[..], [PlannerCmd::Start { req_id, .. }] if req_id == "r2"));
    let started = o.log.iter().find(|l| l.what == "search_started").unwrap();
    assert_eq!(started.at_mono_ms, 200);
    assert!(o
        .log
        .iter()
        .any(|l| l.what == "clock_regression" && l.at_mono_ms == 200));
    // r2 deadline is computed from the clamped start time
    assert_eq!(o.next_wake_mono_ms, Some(10_200));
}
