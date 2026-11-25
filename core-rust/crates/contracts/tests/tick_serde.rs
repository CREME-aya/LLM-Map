use contracts::tick::*;
use contracts::U64Str;

#[test]
fn u64str_rejects_numbers_signs_and_leading_zeros() {
    for bad in [
        "1",
        "\"-1\"",
        "\"01\"",
        "\"\"",
        "\"18446744073709551616\"",
        "\"1e3\"",
    ] {
        assert!(serde_json::from_str::<U64Str>(bad).is_err(), "{bad}");
    }
    assert_eq!(
        serde_json::from_str::<U64Str>("\"18446744073709551615\"").unwrap(),
        U64Str(u64::MAX)
    );
    assert_eq!(
        serde_json::to_string(&U64Str(9007199254740993)).unwrap(),
        "\"9007199254740993\""
    );
}

#[test]
fn all_v2_event_kinds_parse() {
    let rk = r#"{"trip_id":"t","trip_revision":"1","route_revision":"1","graph_version":"g"}"#;
    let events = [
        r#"{"kind":"tts_started","token":"a"}"#.to_string(),
        r#"{"kind":"tts_done","token":"a"}"#.into(),
        r#"{"kind":"tts_stopped","token":"a"}"#.into(),
        r#"{"kind":"asr_started","utterance_id":"u"}"#.into(),
        r#"{"kind":"asr_result","utterance_id":"u","text_class":"dict","dict_norm":"はい"}"#.into(),
        r#"{"kind":"asr_failed","utterance_id":"u"}"#.into(),
        r#"{"kind":"override_ready","utterance_id":"u","status":"timeout","confirm_screen_id":"s"}"#.into(),
        r#"{"kind":"user_tap","target_id":"o1","action":"confirm"}"#.into(),
        r#"{"kind":"thermal","status":"emergency"}"#.into(),
        r#"{"kind":"low_memory"}"#.into(),
        r#"{"kind":"inference_down","reason":"oom"}"#.into(),
        r#"{"kind":"inference_up","tier":1}"#.into(),
        r#"{"kind":"audio_focus_lost"}"#.into(),
        r#"{"kind":"call_state","active":true}"#.into(),
        format!(r#"{{"kind":"search_finished","req_id":"r1","route_key":{rk},"graph_version":"g","outcome":"cancelled","engine_ms":12}}"#),
        r#"{"kind":"engine_reset"}"#.into(),
    ];
    for e in &events {
        let with_at = format!(r#"{{"at_mono_ms":5,{}"#, &e[1..]);
        let ev: Event = serde_json::from_str(&with_at).unwrap_or_else(|err| panic!("{e}: {err}"));
        assert_eq!(ev.at_mono_ms, 5);
    }
}

#[test]
fn mono_ms_is_a_json_number_not_a_string() {
    assert!(serde_json::from_str::<TickInput>(r#"{"now_mono_ms":"1"}"#).is_err());
    assert!(serde_json::from_str::<TickInput>(r#"{"now_mono_ms":-1}"#).is_err());
    assert!(serde_json::from_str::<TickInput>(r#"{"now_mono_ms":1.5}"#).is_err());
    let t: TickInput = serde_json::from_str(r#"{"now_mono_ms":18446744073709551615}"#).unwrap();
    assert_eq!(t.now_mono_ms, u64::MAX);
    let out = TickOutput {
        next_wake_mono_ms: Some(42),
        ..Default::default()
    };
    let v = serde_json::to_value(&out).unwrap();
    assert_eq!(v["next_wake_mono_ms"], serde_json::json!(42));
    assert_eq!(
        serde_json::to_value(TickOutput::default()).unwrap()["next_wake_mono_ms"],
        serde_json::Value::Null
    );
}

#[test]
fn search_finished_has_no_timeout_outcome() {
    let rk = r#"{"trip_id":"t","trip_revision":"1","route_revision":"1","graph_version":"g"}"#;
    for (outcome, ok) in [
        ("ok", true),
        ("cancelled", true),
        ("error", true),
        ("timeout", false),
    ] {
        let s = format!(
            r#"{{"at_mono_ms":1,"kind":"search_finished","req_id":"r1","route_key":{rk},"graph_version":"g","outcome":"{outcome}","engine_ms":1}}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&s).is_ok(), ok, "{outcome}");
    }
}

#[test]
fn enums_are_snake_case_on_the_wire() {
    let v = serde_json::to_value(PlannerCmd::Start {
        req_id: "r1".into(),
        priority: SearchPriority::AvoidChip,
        distance_class: DistanceClass::Short,
        route_key: serde_json::from_str(
            r#"{"trip_id":"t","trip_revision":"1","route_revision":"1","graph_version":"g"}"#,
        )
        .unwrap(),
        graph_version: "g".into(),
        candidate_id: None,
    })
    .unwrap();
    assert_eq!(v["cmd"], "start");
    assert!(v.get("candidate_id").is_none(), "absent, not null");
    assert_eq!(v["priority"], "avoid_chip");
    assert_eq!(v["distance_class"], "short");
    assert_eq!(
        serde_json::to_value(UiCmd::ShowCheckingRoute).unwrap()["ui"],
        "show_checking_route"
    );
    assert_eq!(
        serde_json::to_value(ThermalStatus::Critical).unwrap(),
        "critical"
    );
}

#[test]
fn fieldless_event_kinds_reject_extra_fields() {
    // unit-like kinds are `{}` variants so extra fields are not silently ignored
    for k in ["low_memory", "audio_focus_lost", "engine_reset"] {
        let ok = format!(r#"{{"at_mono_ms":1,"kind":"{k}"}}"#);
        let bad = format!(r#"{{"at_mono_ms":1,"kind":"{k}","x":0}}"#);
        assert!(serde_json::from_str::<Event>(&ok).is_ok(), "{k}");
        assert!(serde_json::from_str::<Event>(&bad).is_err(), "{k}");
        assert_eq!(
            serde_json::to_string(&serde_json::from_str::<Event>(&ok).unwrap()).unwrap(),
            ok
        );
    }
    // missing / duplicate at_mono_ms, missing kind
    for bad in [
        r#"{"kind":"low_memory"}"#,
        r#"{"at_mono_ms":1,"at_mono_ms":2,"kind":"low_memory"}"#,
        r#"{"at_mono_ms":1}"#,
        r#"{"at_mono_ms":1,"kind":"tts_done","token":"a","token":"b"}"#,
        r#"[1,"low_memory"]"#,
    ] {
        assert!(serde_json::from_str::<Event>(bad).is_err(), "{bad}");
    }
}

#[test]
fn abolished_terminal_events_are_rejected() {
    // v2: reroute_result / search_cancelled are replaced by search_finished.
    for k in ["reroute_result", "search_cancelled"] {
        let s = format!(r#"{{"at_mono_ms":1,"kind":"{k}","req_id":"r1"}}"#);
        assert!(serde_json::from_str::<Event>(&s).is_err(), "{k}");
    }
}

#[test]
fn tick_input_requires_now_and_rejects_unknown_fields() {
    assert!(serde_json::from_str::<TickInput>(r#"{"events":[]}"#).is_err());
    assert!(serde_json::from_str::<TickInput>(r#"{"now_mono_ms":1,"extra":1}"#).is_err());
    let t: TickInput = serde_json::from_str(r#"{"now_mono_ms":1}"#).unwrap();
    assert!(t.events.is_empty() && t.fix.is_none());
}
