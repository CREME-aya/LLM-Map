//! schema/ conformance: every fixture in schema/fixtures/valid must pass its schema,
//! every fixture in schema/fixtures/invalid must fail. File name prefix = schema name.
//! Also checks that the Rust serde mirror agrees for PersonalRoutingOverride and for
//! tick.schema.json (contracts::tick, including core-rust/tests/fixtures/tick).

use contracts::override_types::PersonalRoutingOverride;
use jsonschema::JSONSchema;
use serde_json::Value;
use std::path::{Path, PathBuf};

const BASE: &str = "https://slm-map.invalid/schema/";
const SCHEMAS: [&str; 9] = [
    "common",
    "trip-plan-snapshot",
    "candidate-facts",
    "maneuver-horizon",
    "personal-routing-override",
    "tick",
    "route-bundle",
    "route-comparison",
    "trip-score",
];

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../schema")
}

fn load(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn compile(name: &str) -> JSONSchema {
    compile_doc(
        name,
        &load(&schema_dir().join(format!("{name}.schema.json"))),
    )
}

fn compile_doc(name: &str, root: &Value) -> JSONSchema {
    let mut opts = JSONSchema::options();
    opts.with_draft(jsonschema::Draft::Draft202012);
    for s in SCHEMAS {
        let doc = load(&schema_dir().join(format!("{s}.schema.json")));
        opts.with_document(format!("{BASE}{s}.schema.json"), doc);
    }
    opts.compile(root)
        .unwrap_or_else(|e| panic!("compile {name}: {e}"))
}

/// Compiles `route-bundle.schema.json#/$defs/<def>`.
fn route_def(def: &str) -> JSONSchema {
    compile_doc(
        def,
        &serde_json::json!({ "$ref": format!("{BASE}route-bundle.schema.json#/$defs/{def}") }),
    )
}

fn minimal_bundle() -> Value {
    load(&schema_dir().join("fixtures/valid/route-bundle.min.json"))
}

/// Compiles `tick.schema.json#/$defs/<def>`.
fn tick_def(def: &str) -> JSONSchema {
    compile_doc(
        def,
        &serde_json::json!({ "$ref": format!("{BASE}tick.schema.json#/$defs/{def}") }),
    )
}

fn errors(c: &JSONSchema, v: &Value) -> Vec<String> {
    match c.validate(v) {
        Ok(()) => Vec::new(),
        Err(errs) => errs
            .map(|e| format!("{} at {}", e, e.instance_path))
            .collect(),
    }
}

fn fixtures(kind: &str) -> Vec<(String, PathBuf)> {
    let mut v: Vec<_> = std::fs::read_dir(schema_dir().join("fixtures").join(kind))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let f = p.file_name().unwrap().to_string_lossy().to_string();
            let schema = SCHEMAS
                .iter()
                .filter(|s| f.starts_with(&format!("{s}.")))
                .max_by_key(|s| s.len())
                .unwrap_or_else(|| panic!("fixture {f} has no schema prefix"));
            (schema.to_string(), p)
        })
        .collect();
    v.sort();
    v
}

#[test]
fn all_schemas_are_valid_2020_12_and_compile() {
    for s in SCHEMAS {
        let doc = load(&schema_dir().join(format!("{s}.schema.json")));
        assert_eq!(
            doc["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{s}"
        );
        assert_eq!(doc["$id"], format!("{BASE}{s}.schema.json"), "{s}");
        compile(s);
    }
}

#[test]
fn valid_fixtures_pass() {
    let fx = fixtures("valid");
    assert!(fx.len() >= 4);
    for (schema, path) in fx {
        let v = load(&path);
        let c = compile(&schema);
        let msgs: Vec<String> = match c.validate(&v) {
            Ok(()) => Vec::new(),
            Err(errs) => errs
                .map(|e| format!("{} at {}", e, e.instance_path))
                .collect(),
        };
        assert!(
            msgs.is_empty(),
            "{} should be valid: {msgs:#?}",
            path.display()
        );
    }
}

#[test]
fn invalid_fixtures_fail() {
    let fx = fixtures("invalid");
    assert!(fx.len() >= 5);
    for (schema, path) in fx {
        let v = load(&path);
        assert!(
            !compile(&schema).is_valid(&v),
            "{} should be invalid",
            path.display()
        );
    }
}

/// contracts::plan::TripPlanSnapshot (input of Session::plan_candidates) agrees with
/// trip-plan-snapshot.schema.json on every fixture, including unknown fields.
#[test]
fn rust_mirror_agrees_on_trip_plan_snapshot() {
    let mut seen = 0;
    for kind in ["valid", "invalid"] {
        for (schema, path) in fixtures(kind) {
            if schema != "trip-plan-snapshot" {
                continue;
            }
            seen += 1;
            let v = load(&path);
            let by_schema = compile(&schema).is_valid(&v);
            let by_serde =
                serde_json::from_value::<contracts::plan::TripPlanSnapshot>(v.clone()).is_ok();
            assert_eq!(by_schema, by_serde, "{}", path.display());
            if kind == "valid" {
                let mut extra = v.clone();
                extra["budget"]["extra"] = serde_json::json!(1);
                assert!(!compile(&schema).is_valid(&extra));
                assert!(
                    serde_json::from_value::<contracts::plan::TripPlanSnapshot>(extra).is_err()
                );
            }
        }
    }
    assert!(seen >= 2);
}

/// contracts::facts::CandidateFacts (the `alt_facts` of dc_avoid_offer_eval) agrees with
/// candidate-facts.schema.json on every fixture, and rejects unknown fields like the schema.
#[test]
fn rust_mirror_agrees_on_candidate_facts() {
    use contracts::facts::CandidateFacts;
    let mut seen = 0;
    for kind in ["valid", "invalid"] {
        for (schema, path) in fixtures(kind) {
            if schema != "candidate-facts" {
                continue;
            }
            seen += 1;
            let v = load(&path);
            let by_schema = compile(&schema).is_valid(&v);
            let by_serde = serde_json::from_value::<CandidateFacts>(v.clone());
            assert_eq!(by_schema, by_serde.is_ok(), "{}", path.display());
            if let Ok(typed) = by_serde {
                assert_eq!(
                    serde_json::to_value(&typed).unwrap(),
                    v,
                    "{}",
                    path.display()
                );
                let mut extra = v.clone();
                extra["counts"]["extra"] = serde_json::json!(1);
                assert!(!compile(&schema).is_valid(&extra));
                assert!(serde_json::from_value::<CandidateFacts>(extra).is_err());
            }
        }
    }
    assert!(seen >= 3);
}

/// contracts::rank::{RankRequest, RouteComparison} agrees with route-comparison.schema.json
/// on every valid and invalid fixture, including unknown fields and weight constraints.
#[test]
fn rust_mirror_agrees_on_route_comparison() {
    use contracts::rank::{RankRequest, RouteComparison};
    let req_schema = compile_doc(
        "RankRequest",
        &serde_json::json!({ "$ref": format!("{BASE}route-comparison.schema.json#/$defs/RankRequest") }),
    );
    let out_schema = compile_doc(
        "RouteComparison",
        &serde_json::json!({ "$ref": format!("{BASE}route-comparison.schema.json#/$defs/RouteComparison") }),
    );
    let mut seen = 0;
    for kind in ["valid", "invalid"] {
        for (schema, path) in fixtures(kind) {
            if schema != "route-comparison" {
                continue;
            }
            seen += 1;
            let f = path.file_name().unwrap().to_string_lossy().to_string();
            let v = load(&path);
            let is_req = f.starts_with("route-comparison.request.");
            assert!(is_req || f.starts_with("route-comparison.output."), "{f}");
            let c = if is_req { &req_schema } else { &out_schema };
            let serde_back: Result<Value, String> = if is_req {
                serde_json::from_value::<RankRequest>(v.clone())
                    .map(|t| serde_json::to_value(t).unwrap())
                    .map_err(|e| e.to_string())
            } else {
                serde_json::from_value::<RouteComparison>(v.clone())
                    .map(|t| serde_json::to_value(t).unwrap())
                    .map_err(|e| e.to_string())
            };
            if kind == "valid" {
                assert_eq!(errors(c, &v), Vec::<String>::new(), "{f}");
                let back = serde_back.unwrap_or_else(|e| panic!("serde {f}: {e}"));
                assert_eq!(errors(c, &back), Vec::<String>::new(), "round trip {f}");
                if !is_req {
                    assert_eq!(back, v, "exact round trip {f}");
                }
            } else {
                assert!(!c.is_valid(&v), "schema should reject {f}");
                assert!(serde_back.is_err(), "serde should reject {f}");
            }
        }
    }
    assert!(seen >= 6, "route-comparison fixtures found: {seen}");
}

/// contracts::trip_score::{TripScoreRequest, TripScoreResult} agrees with
/// trip-score.schema.json on every valid and invalid fixture.
#[test]
fn rust_mirror_agrees_on_trip_score() {
    use contracts::trip_score::{TripScoreRequest, TripScoreResult};
    let req_schema = compile_doc(
        "TripScoreRequest",
        &serde_json::json!({ "$ref": format!("{BASE}trip-score.schema.json#/$defs/TripScoreRequest") }),
    );
    let out_schema = compile_doc(
        "TripScoreResult",
        &serde_json::json!({ "$ref": format!("{BASE}trip-score.schema.json#/$defs/TripScoreResult") }),
    );
    let mut seen = 0;
    for kind in ["valid", "invalid"] {
        for (schema, path) in fixtures(kind) {
            if schema != "trip-score" {
                continue;
            }
            seen += 1;
            let f = path.file_name().unwrap().to_string_lossy().to_string();
            let v = load(&path);
            let is_req = f.starts_with("trip-score.request.");
            assert!(is_req || f.starts_with("trip-score.output."), "{f}");
            let c = if is_req { &req_schema } else { &out_schema };
            let serde_back: Result<Value, String> = if is_req {
                serde_json::from_value::<TripScoreRequest>(v.clone())
                    .map(|t| serde_json::to_value(t).unwrap())
                    .map_err(|e| e.to_string())
            } else {
                serde_json::from_value::<TripScoreResult>(v.clone())
                    .map(|t| serde_json::to_value(t).unwrap())
                    .map_err(|e| e.to_string())
            };
            if kind == "valid" {
                assert_eq!(errors(c, &v), Vec::<String>::new(), "{f}");
                let back = serde_back.unwrap_or_else(|e| panic!("serde {f}: {e}"));
                assert_eq!(errors(c, &back), Vec::<String>::new(), "round trip {f}");
                if !is_req {
                    assert_eq!(back, v, "exact round trip {f}");
                }
            } else {
                assert!(!c.is_valid(&v), "schema should reject {f}");
                assert!(serde_back.is_err(), "serde should reject {f}");
            }
        }
    }
    assert!(seen >= 4, "trip-score fixtures found: {seen}");
}

#[test]
fn rust_mirror_agrees_on_personal_routing_override() {
    for (schema, path) in fixtures("valid") {
        if schema == "personal-routing-override" {
            let raw = load(&path);
            let typed: PersonalRoutingOverride = serde_json::from_value(raw.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&typed).unwrap(),
                raw,
                "round trip {}",
                path.display()
            );
        }
    }
    // maxItems:3 is a schema-only rule; Rust enforces it in the validator, not in serde.
    // TODO(drive-core): enforce junction_preferences.len() <= 3 in dc_validate_override.
    for (schema, path) in fixtures("invalid") {
        if schema == "personal-routing-override"
            && !path.ends_with("personal-routing-override.four-junctions.json")
        {
            assert!(
                serde_json::from_value::<PersonalRoutingOverride>(load(&path)).is_err(),
                "serde should reject {}",
                path.display()
            );
        }
    }
}

#[test]
fn rust_mirror_agrees_on_avoid_offer() {
    use contracts::horizon::AvoidOffer;
    let dir = schema_dir().join("fixtures");
    let valid = load(&dir.join("valid/maneuver-horizon.min.json"));
    let raw = valid["upcoming"][0]["avoid_offer"].clone();
    let typed: AvoidOffer = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(typed.offer_id, "o1");
    assert_eq!(serde_json::to_value(&typed).unwrap(), raw);
    let invalid = load(&dir.join("invalid/maneuver-horizon.avoid-offer-without-offer-id.json"));
    assert!(
        serde_json::from_value::<AvoidOffer>(invalid["upcoming"][0]["avoid_offer"].clone())
            .is_err()
    );
}

/// `Evidence` (04 §2): every `evidence` in the candidate-facts fixtures and every Evidence
/// of a Maneuver in the valid fixtures is accepted by serde exactly when the schema accepts
/// it, and round-trips unchanged. `source_refs` / `observed_at` are carried through.
#[test]
fn rust_mirror_agrees_on_evidence() {
    use contracts::horizon::{Evidence, EvidenceKind};
    let ev_schema = compile_doc(
        "Evidence",
        &serde_json::json!({ "$ref": format!("{BASE}common.schema.json#/$defs/Evidence") }),
    );
    let mut checked = 0;
    for kind in ["valid", "invalid"] {
        for (_, path) in fixtures(kind) {
            let f = path.file_name().unwrap().to_string_lossy().to_string();
            if !f.starts_with("candidate-facts.") {
                continue;
            }
            let v = load(&path);
            for j in v["junctions"].as_array().unwrap() {
                let raw = j["evidence"].clone();
                let schema_ok = ev_schema.is_valid(&raw);
                let serde = serde_json::from_value::<Evidence>(raw.clone());
                assert_eq!(serde.is_ok(), schema_ok, "{f}: {raw}");
                if let Ok(e) = serde {
                    assert_eq!(serde_json::to_value(&e).unwrap(), raw, "{f}");
                }
                checked += 1;
            }
        }
    }
    assert!(checked >= 9, "{checked}");

    let dir = schema_dir().join("fixtures/valid");
    let bundle = load(&dir.join("route-bundle.min.json"));
    let m0 = &bundle["legs"][0]["maneuvers"][0];
    let w: Evidence = serde_json::from_value(m0["approach_width_mm"].clone()).unwrap();
    assert_eq!(w.kind, EvidenceKind::Inferred);
    assert_eq!(
        w.source_refs.as_deref(),
        Some(&["osm:way/1".to_string()][..])
    );
    let horizon = load(&dir.join("maneuver-horizon.min.json"));
    let raw = horizon["upcoming"][0]["maneuver"]["approach_width_mm"].clone();
    let typed: Evidence = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), raw);

    let observed: Evidence = serde_json::from_value(serde_json::json!(
        {"kind":"confirmed","value":"absent","source_ref":"osm:node/2","observed_at":1764115200}))
    .unwrap();
    assert_eq!(observed.observed_at, Some(serde_json::json!(1764115200)));
    for bad in [
        serde_json::json!({"kind":"unknown","reason":"missing","source_refs":[]}),
        serde_json::json!({"kind":"unknown","reason":"missing","rule_id":"r"}),
        serde_json::json!({"kind":"unknown","reason":"missing","observed_at":1}),
        serde_json::json!({"kind":"confirmed","value":1,"source_ref":"s","reason":"missing"}),
        serde_json::json!({"kind":"inferred","value":1,"rule_id":"r","source_refs":[],"observed_at":1}),
        serde_json::json!({"kind":"inferred","value":1,"rule_id":"r","source_refs":[],"extra":1}),
    ] {
        assert!(!ev_schema.is_valid(&bad), "schema accepts {bad}");
        assert!(
            serde_json::from_value::<Evidence>(bad.clone()).is_err(),
            "serde accepts {bad}"
        );
    }
}

// ---- tick.schema.json ⇔ contracts::tick ------------------------------------------------

mod tick {
    use super::*;
    use contracts::tick::{
        DismissReason, DistanceClass, Event, LogEvent, PlannerCmd, RankReason, SaveForLaterReason,
        SearchPriority, TickInput, TickOutput, UiCmd, VoiceCmd, VoicePriority,
    };

    fn rk() -> contracts::RouteKey {
        serde_json::from_value(serde_json::json!(
            {"trip_id":"t1","trip_revision":"3","route_revision":"7","graph_version":"g"}))
        .unwrap()
    }

    /// Every file under schema/fixtures/*/tick.input.* is a TickInput, tick.output.* a
    /// TickOutput. Valid ones: schema ok + serde ok + serde round trip passes the schema.
    /// Invalid ones: schema rejects AND serde rejects (both sides agree).
    #[test]
    fn tick_fixtures_agree_with_serde() {
        let input = tick_def("TickInput");
        let output = tick_def("TickOutput");
        let mut n = 0;
        for kind in ["valid", "invalid"] {
            for (schema, path) in fixtures(kind) {
                if schema != "tick" {
                    continue;
                }
                n += 1;
                let f = path.file_name().unwrap().to_string_lossy().to_string();
                let v = load(&path);
                let is_input = f.starts_with("tick.input.");
                assert!(is_input || f.starts_with("tick.output."), "{f}");
                let c = if is_input { &input } else { &output };
                let serde_back: Result<Value, String> = if is_input {
                    serde_json::from_value::<TickInput>(v.clone())
                        .map(|t| serde_json::to_value(t).unwrap())
                        .map_err(|e| e.to_string())
                } else {
                    serde_json::from_value::<TickOutput>(v.clone())
                        .map(|t| serde_json::to_value(t).unwrap())
                        .map_err(|e| e.to_string())
                };
                if kind == "valid" {
                    assert_eq!(errors(c, &v), Vec::<String>::new(), "{f}");
                    let back = serde_back.unwrap_or_else(|e| panic!("serde {f}: {e}"));
                    assert_eq!(errors(c, &back), Vec::<String>::new(), "round trip {f}");
                } else {
                    assert!(!c.is_valid(&v), "schema should reject {f}");
                    assert!(serde_back.is_err(), "serde should reject {f}");
                }
            }
        }
        assert!(n >= 8, "tick fixtures found: {n}");
    }

    /// core-rust/tests/fixtures/tick/*: every `tick` step input is a valid TickInput and
    /// parses with serde; `request` priorities are SearchPriority values; `expect_cmds`
    /// are valid PlannerCmds; a `plan_candidates` snapshot is a valid TripPlanSnapshot.
    #[test]
    fn replay_fixtures_pass_tick_schema() {
        let input = tick_def("TickInput");
        let prio = tick_def("SearchPriority");
        let class = tick_def("DistanceClass");
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tick");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        assert!(files.len() >= 3);
        let mut ticks = 0;
        for p in files {
            let fx = load(&p);
            for step in fx["steps"].as_array().unwrap() {
                match step["step"].as_str().unwrap() {
                    "tick" => {
                        let v = &step["input"];
                        assert_eq!(errors(&input, v), Vec::<String>::new(), "{}", p.display());
                        serde_json::from_value::<TickInput>(v.clone()).unwrap();
                        ticks += 1;
                    }
                    "request" => {
                        assert!(prio.is_valid(&step["priority"]), "{}", p.display());
                        serde_json::from_value::<SearchPriority>(step["priority"].clone()).unwrap();
                    }
                    "plan" => {}
                    // test hook standing in for a commit{req_id, route_key} or plan ok not driven here
                    "issue_commit" | "plan_ok" => {
                        let key = compile_doc(
                            "RouteKey",
                            &serde_json::json!({ "$ref": format!("{BASE}common.schema.json#/$defs/RouteKey") }),
                        );
                        assert!(key.is_valid(&step["route_key"]), "{}", p.display());
                        assert!(step["req_id"].is_string(), "{}", p.display());
                    }
                    "set_route" => {
                        let key = compile_doc(
                            "RouteKey",
                            &serde_json::json!({ "$ref": format!("{BASE}common.schema.json#/$defs/RouteKey") }),
                        );
                        assert!(key.is_valid(&step["route_key"]), "{}", p.display());
                    }
                    // with a bundle: a full RouteCommitRequest; without: the guard only
                    "route_commit" => {
                        let req = if step.get("bundle").is_some() {
                            serde_json::json!({"req_id": step["req_id"], "expected_route_key": step["expected_route_key"], "bundle": step["bundle"]})
                        } else {
                            serde_json::json!({"req_id": step.get("req_id").cloned().unwrap_or(serde_json::Value::Null), "expected_route_key": step["expected_route_key"], "bundle": minimal_bundle()})
                        };
                        let c = route_def("RouteCommitRequest");
                        // E_SCHEMA steps (e.g. a bundle route_revision other than "0") must
                        // fail both the schema and serde
                        let parsed = serde_json::from_value::<contracts::route::RouteCommitRequest>(
                            req.clone(),
                        );
                        if step["expect_error"] == "E_SCHEMA" {
                            assert!(!c.is_valid(&req), "{}", p.display());
                            assert!(parsed.is_err(), "{}", p.display());
                        } else {
                            assert_eq!(errors(&c, &req), Vec::<String>::new(), "{}", p.display());
                            parsed.unwrap();
                        }
                    }
                    "plan_candidates" => {
                        let snap = compile("trip-plan-snapshot");
                        assert!(snap.is_valid(&fx["snapshot"]), "{}", p.display());
                    }
                    o => panic!("{}: unknown step {o}", p.display()),
                }
                // expected planner_cmds are valid PlannerCmd values
                let cmd = tick_def("PlannerCmd");
                for c in step["expect_cmds"].as_array().into_iter().flatten() {
                    assert_eq!(errors(&cmd, c), Vec::<String>::new(), "{}", p.display());
                    serde_json::from_value::<PlannerCmd>(c.clone()).unwrap();
                }
                // expected starts use the schema's priority / distance_class names
                for e in step["expect_starts"].as_array().into_iter().flatten() {
                    assert!(prio.is_valid(&e["priority"]), "{}", p.display());
                    assert!(class.is_valid(&e["distance_class"]), "{}", p.display());
                }
            }
        }
        assert!(ticks >= 6);
    }

    /// Outputs built from every serde variant pass TickOutput.
    #[test]
    fn serialized_tick_output_passes_schema() {
        let output = tick_def("TickOutput");
        let mut planner_cmds = vec![PlannerCmd::Cancel {
            req_id: "r1".into(),
        }];
        for priority in [
            SearchPriority::Reroute,
            SearchPriority::ExplicitPlan,
            SearchPriority::AvoidChip,
            SearchPriority::Comparison,
        ] {
            for distance_class in [DistanceClass::Short, DistanceClass::Long] {
                planner_cmds.push(PlannerCmd::Start {
                    req_id: "r2".into(),
                    priority,
                    distance_class,
                    route_key: rk(),
                    graph_version: "g".into(),
                    candidate_id: None,
                    fix: None,
                });
                planner_cmds.push(PlannerCmd::Start {
                    req_id: "r3".into(),
                    priority,
                    distance_class,
                    route_key: rk(),
                    graph_version: "g".into(),
                    candidate_id: Some("c2".into()),
                    fix: None,
                });
                planner_cmds.push(PlannerCmd::Start {
                    req_id: "r4".into(),
                    priority,
                    distance_class,
                    route_key: rk(),
                    graph_version: "g".into(),
                    candidate_id: None,
                    fix: Some(contracts::tick::Fix {
                        at_mono_ms: 9,
                        lat_e7: 356_812_345,
                        lon_e7: 1_397_671_234,
                        acc_cm: 500,
                        bearing_cdeg: Some(35_999),
                        speed_cm_s: Some(1_390),
                    }),
                });
            }
        }
        for (facts_refs, reason) in [
            (vec![], RankReason::BudgetExpired),
            (vec!["f1".to_string(), "f2".into()], RankReason::AllDone),
        ] {
            planner_cmds.push(PlannerCmd::Rank {
                plan_request_id: "p1".into(),
                facts_refs,
                reason,
            });
        }
        planner_cmds.push(PlannerCmd::Commit {
            req_id: "r2".into(),
            route_key: rk(),
            undo_req_id: None,
        });
        let horizon: contracts::horizon::ManeuverHorizon = serde_json::from_value(load(
            &schema_dir().join("fixtures/valid/maneuver-horizon.min.json"),
        ))
        .unwrap();
        for (next_wake, horizon) in [(None, None), (Some(u64::MAX), Some(horizon))] {
            let out = TickOutput {
                horizon,
                voice_cmds: all_voice_cmds(),
                ui_cmds: vec![
                    UiCmd::ShowConfirm {
                        confirm_screen_id: "s1".into(),
                        override_id: "ov1".into(),
                        expires_at_mono_ms: u64::MAX,
                    },
                    UiCmd::Dismiss {
                        target_id: "o1".into(),
                        reason: DismissReason::Superseded,
                    },
                    UiCmd::SaveForLater {
                        poi_id: "poi-castle".into(),
                        reason: SaveForLaterReason::Interrupted,
                    },
                    UiCmd::SaveForLater {
                        poi_id: "poi-shrine".into(),
                        reason: SaveForLaterReason::Bmk,
                    },
                ],
                planner_cmds: planner_cmds.clone(),
                next_wake_mono_ms: next_wake,
                log: vec![
                    LogEvent {
                        at_mono_ms: 1,
                        what: "engine_stuck".into(),
                        req_id: Some("r1".into()),
                        detail: None,
                    },
                    LogEvent {
                        at_mono_ms: 2,
                        what: "plan_started".into(),
                        req_id: None,
                        detail: Some("distance_class=long".into()),
                    },
                ],
            };
            let v = serde_json::to_value(&out).unwrap();
            assert_eq!(errors(&output, &v), Vec::<String>::new());
        }
        assert_eq!(
            errors(
                &output,
                &serde_json::to_value(TickOutput::default()).unwrap()
            ),
            Vec::<String>::new()
        );
    }

    /// Enum value sets in the schema equal the serde names (no drift either way).
    #[test]
    fn enum_sets_match_serde() {
        let doc = load(&schema_dir().join("tick.schema.json"));
        let names = |def: &str| -> Vec<String> {
            let mut v: Vec<String> = doc["$defs"][def]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_owned())
                .collect();
            v.sort();
            v
        };
        let ser = |vals: Vec<Value>| -> Vec<String> {
            let mut v: Vec<String> = vals
                .iter()
                .map(|x| x.as_str().unwrap().to_owned())
                .collect();
            v.sort();
            v
        };
        use contracts::tick::{SearchOutcome, ThermalStatus};
        assert_eq!(
            names("SearchPriority"),
            ser(vec![
                serde_json::to_value(SearchPriority::Reroute).unwrap(),
                serde_json::to_value(SearchPriority::ExplicitPlan).unwrap(),
                serde_json::to_value(SearchPriority::AvoidChip).unwrap(),
                serde_json::to_value(SearchPriority::Comparison).unwrap(),
            ])
        );
        assert_eq!(
            names("RankReason"),
            ser(vec![
                serde_json::to_value(RankReason::AllDone).unwrap(),
                serde_json::to_value(RankReason::BudgetExpired).unwrap(),
            ])
        );
        // schema lists priorities highest first, matching Ord (higher wins)
        let order: Vec<SearchPriority> = doc["$defs"]["SearchPriority"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| serde_json::from_value(x.clone()).unwrap())
            .collect();
        assert!(order.windows(2).all(|w| w[0] > w[1]), "{order:?}");
        assert_eq!(
            names("VoicePriority"),
            ser([
                VoicePriority::P0,
                VoicePriority::P1,
                VoicePriority::P2,
                VoicePriority::P3
            ]
            .iter()
            .map(|x| serde_json::to_value(x).unwrap())
            .collect())
        );
        // P0 > P1 > P2 > P3, schema lists highest first
        let order: Vec<VoicePriority> = doc["$defs"]["VoicePriority"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| serde_json::from_value(x.clone()).unwrap())
            .collect();
        assert!(order.windows(2).all(|w| w[0] > w[1]), "{order:?}");
        assert_eq!(
            names("DismissReason"),
            ser([
                DismissReason::Expired,
                DismissReason::Cancelled,
                DismissReason::RouteChanged,
                DismissReason::Superseded,
            ]
            .iter()
            .map(|x| serde_json::to_value(x).unwrap())
            .collect())
        );
        assert_eq!(
            names("SaveForLaterReason"),
            ser([SaveForLaterReason::Interrupted, SaveForLaterReason::Bmk]
                .iter()
                .map(|x| serde_json::to_value(x).unwrap())
                .collect())
        );
        assert_eq!(
            names("SearchOutcome"),
            ser([
                SearchOutcome::Ok,
                SearchOutcome::Cancelled,
                SearchOutcome::Error
            ]
            .iter()
            .map(|x| serde_json::to_value(x).unwrap())
            .collect())
        );
        assert_eq!(
            names("ThermalStatus"),
            ser([
                ThermalStatus::None,
                ThermalStatus::Light,
                ThermalStatus::Moderate,
                ThermalStatus::Severe,
                ThermalStatus::Critical,
                ThermalStatus::Emergency,
                ThermalStatus::Shutdown,
            ]
            .iter()
            .map(|x| serde_json::to_value(x).unwrap())
            .collect())
        );
        assert_eq!(
            names("DistanceClass"),
            ser(vec![
                serde_json::to_value(DistanceClass::Short).unwrap(),
                serde_json::to_value(DistanceClass::Long).unwrap(),
            ])
        );
    }

    /// Each event kind in the schema is accepted by serde and vice versa: the schema's
    /// Event.oneOf `kind` consts equal the kinds in the all-events fixture, and each
    /// serde-serialized Event passes #/$defs/Event.
    #[test]
    fn every_event_kind_is_in_schema_and_serde() {
        let doc = load(&schema_dir().join("tick.schema.json"));
        let mut schema_kinds: Vec<String> = doc["$defs"]["Event"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                v["properties"]["kind"]["const"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        schema_kinds.sort();
        let all = load(&schema_dir().join("fixtures/valid/tick.input.all-events.json"));
        let event = tick_def("Event");
        let mut fixture_kinds = Vec::new();
        for e in all["events"].as_array().unwrap() {
            let typed: Event = serde_json::from_value(e.clone()).unwrap();
            let back = serde_json::to_value(&typed).unwrap();
            assert_eq!(&back, e, "round trip");
            assert_eq!(errors(&event, &back), Vec::<String>::new());
            fixture_kinds.push(e["kind"].as_str().unwrap().to_owned());
        }
        fixture_kinds.sort();
        assert_eq!(schema_kinds, fixture_kinds);
        assert_eq!(schema_kinds.len(), 17, "v2 has 17 event kinds");
    }

    /// Unknown fields: for every event kind (and TickInput / Fix / PlannerCmd / TickOutput /
    /// LogEvent) adding a field makes BOTH the schema (`additionalProperties: false`) and
    /// serde reject it, and the unmodified value is accepted by both.
    #[test]
    fn unknown_fields_rejected_by_schema_and_serde() {
        let event = tick_def("Event");
        let input = tick_def("TickInput");
        let output = tick_def("TickOutput");
        let all = load(&schema_dir().join("fixtures/valid/tick.input.all-events.json"));
        let mut kinds = 0;
        for e in all["events"].as_array().unwrap() {
            assert!(event.is_valid(e));
            assert!(serde_json::from_value::<Event>(e.clone()).is_ok());
            let mut bad = e.clone();
            bad["unexpected"] = Value::from(1);
            assert!(!event.is_valid(&bad), "schema should reject {bad}");
            assert!(
                serde_json::from_value::<Event>(bad.clone()).is_err(),
                "serde should reject {bad}"
            );
            kinds += 1;
        }
        assert_eq!(kinds, 17);
        let fix = serde_json::json!({"now_mono_ms":1,"fix":{"at_mono_ms":1,"lat_e7":0,"lon_e7":0,"acc_cm":100,"alt":2}});
        let top = serde_json::json!({"now_mono_ms":1,"extra":true});
        for bad in [fix, top] {
            assert!(!input.is_valid(&bad));
            assert!(serde_json::from_value::<TickInput>(bad).is_err());
        }
        let good = load(&schema_dir().join("fixtures/valid/tick.output.planner.json"));
        assert!(output.is_valid(&good));
        serde_json::from_value::<TickOutput>(good.clone()).unwrap();
        let mut bads = Vec::new();
        let mut b = good.clone();
        b["extra"] = Value::from(1);
        bads.push(b);
        let mut b = good.clone();
        b["planner_cmds"][1]["extra"] = Value::from(1);
        bads.push(b);
        let mut b = good.clone();
        b["log"][0]["extra"] = Value::from(1);
        bads.push(b);
        for bad in bads {
            assert!(!output.is_valid(&bad), "schema should reject {bad}");
            assert!(serde_json::from_value::<TickOutput>(bad).is_err());
        }
    }

    fn all_voice_cmds() -> Vec<VoiceCmd> {
        let mut v: Vec<VoiceCmd> = [
            VoicePriority::P0,
            VoicePriority::P1,
            VoicePriority::P2,
            VoicePriority::P3,
        ]
        .into_iter()
        .map(|priority| VoiceCmd::Speak {
            token: "t1".into(),
            priority,
            template_id: "turn".into(),
            params: [("distance_m".to_string(), 200), ("delta_eta_ms".into(), -1)]
                .into_iter()
                .collect(),
        })
        .collect();
        v.push(VoiceCmd::SpeakContent {
            token: "p3:t1:1:poi-1:1".into(),
            content_ref: "pack:poi-1:short".into(),
        });
        v.push(VoiceCmd::Stop { token: "t0".into() });
        v
    }

    /// Every VoiceCmd / UiCmd variant: serde output passes the schema, the schema's tag
    /// consts equal serde's, and an unknown field is rejected by both.
    #[test]
    fn voice_and_ui_cmds_agree_with_schema() {
        let doc = load(&schema_dir().join("tick.schema.json"));
        let tags = |def: &str, tag: &str| -> Vec<String> {
            let mut v: Vec<String> = doc["$defs"][def]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["properties"][tag]["const"].as_str().unwrap().to_owned())
                .collect();
            v.sort();
            v.dedup();
            v
        };
        let voice = tick_def("VoiceCmd");
        let ui = tick_def("UiCmd");
        let voices: Vec<Value> = all_voice_cmds()
            .iter()
            .map(|c| serde_json::to_value(c).unwrap())
            .collect();
        let uis: Vec<Value> = [
            UiCmd::ShowConfirm {
                confirm_screen_id: "s".into(),
                override_id: "o".into(),
                expires_at_mono_ms: 1,
            },
            UiCmd::Dismiss {
                target_id: "o".into(),
                reason: DismissReason::Expired,
            },
            UiCmd::SaveForLater {
                poi_id: "poi-1".into(),
                reason: SaveForLaterReason::Interrupted,
            },
            UiCmd::SaveForLater {
                poi_id: "poi-2".into(),
                reason: SaveForLaterReason::Bmk,
            },
        ]
        .iter()
        .map(|c| serde_json::to_value(c).unwrap())
        .collect();
        let serde_tags = |vals: &[Value], tag: &str| -> Vec<String> {
            let mut v: Vec<String> = vals
                .iter()
                .map(|x| x[tag].as_str().unwrap().to_owned())
                .collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(tags("VoiceCmd", "voice"), serde_tags(&voices, "voice"));
        assert_eq!(tags("UiCmd", "ui"), serde_tags(&uis, "ui"));
        for v in &voices {
            assert_eq!(errors(&voice, v), Vec::<String>::new(), "{v}");
            let mut bad = v.clone();
            bad["text"] = Value::from("raw text");
            assert!(!voice.is_valid(&bad), "{bad}");
            assert!(serde_json::from_value::<VoiceCmd>(bad).is_err());
        }
        for v in &uis {
            assert_eq!(errors(&ui, v), Vec::<String>::new(), "{v}");
            assert_eq!(
                &serde_json::to_value(serde_json::from_value::<UiCmd>(v.clone()).unwrap()).unwrap(),
                v
            );
            let mut bad = v.clone();
            bad["extra"] = Value::from(1);
            assert!(!ui.is_valid(&bad), "{bad}");
            assert!(serde_json::from_value::<UiCmd>(bad).is_err(), "{v}");
        }
        // show_checking_route is abolished on both sides
        let gone = serde_json::json!({"ui": "show_checking_route"});
        assert!(!ui.is_valid(&gone));
        assert!(serde_json::from_value::<UiCmd>(gone).is_err());

        // VoiceCmd.speak content_ref is P3-only and mutually exclusive with template_id/params
        // on both schema and serde (ADR-0008, 05 §6.2).
        for bad_prio in ["p0", "p1", "p2"] {
            let v = serde_json::json!({
                "voice": "speak",
                "token": "t1",
                "priority": bad_prio,
                "content_ref": "pack:poi:short"
            });
            assert!(
                !voice.is_valid(&v),
                "schema must reject content_ref with {bad_prio}"
            );
            assert!(
                serde_json::from_value::<VoiceCmd>(v).is_err(),
                "serde must reject content_ref with {bad_prio}"
            );
        }
        let ok_p3 = serde_json::json!({
            "voice": "speak",
            "token": "t1",
            "priority": "p3",
            "content_ref": "pack:poi:short"
        });
        assert_eq!(errors(&voice, &ok_p3), Vec::<String>::new());
        let parsed: VoiceCmd = serde_json::from_value(ok_p3.clone()).unwrap();
        assert_eq!(
            parsed,
            VoiceCmd::SpeakContent {
                token: "t1".into(),
                content_ref: "pack:poi:short".into(),
            }
        );
        assert_eq!(serde_json::to_value(&parsed).unwrap(), ok_p3);
        for bad in [
            serde_json::json!({"voice": "speak", "token": "t1", "priority": "p3", "template_id": "x", "params": {}, "content_ref": "c"}),
            serde_json::json!({"voice": "speak", "token": "t1", "priority": "p3", "content_ref": "c", "params": {}}),
            serde_json::json!({"voice": "speak", "token": "t1", "priority": "p3", "content_ref": "c", "template_id": "x"}),
            serde_json::json!({"voice": "speak", "token": "t1", "priority": "p3"}),
            serde_json::json!({"voice": "speak", "token": "t1", "priority": "p1", "template_id": "x"}),
            serde_json::json!({"voice": "speak", "token": "t1", "priority": "p1", "params": {}}),
        ] {
            assert!(!voice.is_valid(&bad), "schema should reject {bad}");
            assert!(
                serde_json::from_value::<VoiceCmd>(bad.clone()).is_err(),
                "serde should reject {bad}"
            );
        }
    }

    /// `planner_cmds.start` carries a required `distance_class`; every serde Start passes
    /// the schema and a Start without it is rejected by both.
    #[test]
    fn planner_start_distance_class_is_required() {
        let cmd = tick_def("PlannerCmd");
        for distance_class in [DistanceClass::Short, DistanceClass::Long] {
            let v = serde_json::to_value(PlannerCmd::Start {
                req_id: "r1".into(),
                priority: SearchPriority::Reroute,
                distance_class,
                route_key: rk(),
                graph_version: "g".into(),
                candidate_id: None,
                fix: None,
            })
            .unwrap();
            assert_eq!(errors(&cmd, &v), Vec::<String>::new());
            let mut missing = v.clone();
            missing.as_object_mut().unwrap().remove("distance_class");
            assert!(!cmd.is_valid(&missing));
            assert!(serde_json::from_value::<PlannerCmd>(missing).is_err());
        }
    }
}

// ---- route-bundle.schema.json ⇔ contracts::route ----------------------------------------

/// Valid route-bundle fixtures parse with serde and round-trip through the schema;
/// invalid ones that break a JSON type / range / unknown-field rule are rejected by both.
/// (Rules JSON Schema cannot express, e.g. chunk continuity or edge coverage, are covered
/// by drive-core's route_bundle tests.)
#[test]
fn rust_mirror_agrees_on_route_bundle() {
    use contracts::route::RouteBundle;
    let c = compile("route-bundle");
    let mut n = 0;
    for kind in ["valid", "invalid"] {
        for (schema, path) in fixtures(kind) {
            if schema != "route-bundle" {
                continue;
            }
            n += 1;
            let v = load(&path);
            let serde = serde_json::from_value::<RouteBundle>(v.clone());
            let f = path.display();
            if kind == "valid" {
                let b = serde.unwrap_or_else(|e| panic!("{f}: {e}"));
                let back = serde_json::to_value(&b).unwrap();
                assert_eq!(back, v, "{f}: round trip");
                assert_eq!(errors(&c, &back), Vec::<String>::new(), "{f}");
            } else {
                assert!(serde.is_err(), "serde should reject {f}");
            }
        }
    }
    assert!(n >= 5, "route-bundle fixtures found: {n}");
    // RouteCommitRequest: req_id is a required non-null string; expected_route_key is
    // required and nullable; unknown fields rejected.
    let req = route_def("RouteCommitRequest");
    let rk = serde_json::json!({"trip_id":"t1","trip_revision":"3","route_revision":"7","graph_version":"g"});
    let b = minimal_bundle();
    for (v, ok) in [
        (
            serde_json::json!({"req_id": "r1", "expected_route_key": null, "bundle": b}),
            true,
        ),
        (
            serde_json::json!({"req_id": "r1", "expected_route_key": rk, "bundle": b}),
            true,
        ),
        (
            serde_json::json!({"req_id": null, "expected_route_key": null, "bundle": b}),
            false,
        ),
        (
            serde_json::json!({"expected_route_key": rk, "bundle": b}),
            false,
        ),
        (
            serde_json::json!({"req_id": 1, "expected_route_key": rk, "bundle": b}),
            false,
        ),
        (serde_json::json!({"req_id": "r1", "bundle": b}), false),
        (
            serde_json::json!({"req_id": "r1", "route_key": null, "bundle": b}),
            false,
        ),
        (
            serde_json::json!({"req_id": "r1", "expected_route_key": null}),
            false,
        ),
        (
            serde_json::json!({"req_id": "r1", "expected_route_key": null, "bundle": b, "x": 1}),
            false,
        ),
    ] {
        assert_eq!(req.is_valid(&v), ok, "{v}");
        assert_eq!(
            serde_json::from_value::<contracts::route::RouteCommitRequest>(v.clone()).is_ok(),
            ok,
            "{v}"
        );
    }
}

/// core-rust/tests/fixtures/route/*.json (GPX-like alignment cases): the bundle passes
/// route-bundle.schema.json and every tick input passes TickInput, both also via serde.
#[test]
fn route_fixtures_pass_schemas() {
    use contracts::route::RouteBundle;
    use contracts::tick::TickInput;
    let bundle = compile("route-bundle");
    let input = tick_def("TickInput");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/route");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(files.len() >= 8, "{files:?}");
    for p in files {
        let fx = load(&p);
        let f = p.display();
        assert_eq!(errors(&bundle, &fx["bundle"]), Vec::<String>::new(), "{f}");
        serde_json::from_value::<RouteBundle>(fx["bundle"].clone()).unwrap();
        for t in fx["ticks"].as_array().into_iter().flatten() {
            assert_eq!(errors(&input, &t["input"]), Vec::<String>::new(), "{f}");
            serde_json::from_value::<TickInput>(t["input"].clone()).unwrap();
        }
    }
}

/// contracts::horizon::ManeuverHorizon agrees with maneuver-horizon.schema.json on every
/// fixture (valid: parse + identical round trip; invalid: rejected by serde too).
#[test]
fn rust_mirror_agrees_on_maneuver_horizon() {
    use contracts::horizon::ManeuverHorizon;
    let c = compile("maneuver-horizon");
    let mut n = 0;
    for kind in ["valid", "invalid"] {
        for (schema, path) in fixtures(kind) {
            if schema != "maneuver-horizon" {
                continue;
            }
            n += 1;
            let v = load(&path);
            let serde = serde_json::from_value::<ManeuverHorizon>(v.clone());
            let f = path.display();
            if kind == "valid" {
                let h = serde.unwrap_or_else(|e| panic!("{f}: {e}"));
                let back = serde_json::to_value(&h).unwrap();
                assert_eq!(back, v, "{f}");
                assert_eq!(errors(&c, &back), Vec::<String>::new(), "{f}");
            } else {
                assert!(serde.is_err(), "serde should reject {f}");
            }
        }
    }
    assert!(n >= 6);
}

/// ABI error code `ErrorCode::ForbiddenWhileDriving` (`E_FORBIDDEN_WHILE_DRIVING`, 02 §2.1 #9)
/// and L3 override rejection code `RejectCode::ForbiddenWhileDriving` (`FORBIDDEN_WHILE_DRIVING`,
/// 03 §4.2) are distinct types with distinct wire strings; `common.schema.json#/$defs/ErrorCode`
/// accepts `E_FORBIDDEN_WHILE_DRIVING` and rejects `FORBIDDEN_WHILE_DRIVING`.
#[test]
fn abi_error_code_and_l3_reject_code_for_driving_are_distinct() {
    use contracts::override_types::{RejectCode, ValidateOverrideResult};
    use contracts::{envelope_err, ErrorCode};

    let err_code_schema = compile_doc(
        "ErrorCode",
        &serde_json::json!({ "$ref": format!("{BASE}common.schema.json#/$defs/ErrorCode") }),
    );
    let envelope_schema = compile_doc(
        "DcEnvelope",
        &serde_json::json!({ "$ref": format!("{BASE}common.schema.json#/$defs/DcEnvelope") }),
    );

    // ABI ErrorCode uses E_FORBIDDEN_WHILE_DRIVING
    assert_eq!(
        ErrorCode::ForbiddenWhileDriving.as_str(),
        "E_FORBIDDEN_WHILE_DRIVING"
    );
    let err_code_json = serde_json::to_value(ErrorCode::ForbiddenWhileDriving).unwrap();
    assert_eq!(
        err_code_json,
        serde_json::json!("E_FORBIDDEN_WHILE_DRIVING")
    );
    assert_eq!(
        errors(&err_code_schema, &err_code_json),
        Vec::<String>::new()
    );
    let env: Value =
        serde_json::from_slice(&envelope_err(ErrorCode::ForbiddenWhileDriving, "driving")).unwrap();
    assert_eq!(env["error"]["code"], "E_FORBIDDEN_WHILE_DRIVING");
    assert_eq!(errors(&envelope_schema, &env), Vec::<String>::new());

    // L3 RejectCode remains FORBIDDEN_WHILE_DRIVING (without E_) and is NOT a valid ABI ErrorCode
    let reject_json = serde_json::to_value(RejectCode::ForbiddenWhileDriving).unwrap();
    assert_eq!(reject_json, serde_json::json!("FORBIDDEN_WHILE_DRIVING"));
    assert!(!err_code_schema.is_valid(&reject_json));
    let rejected = serde_json::to_value(ValidateOverrideResult::Rejected {
        code: RejectCode::ForbiddenWhileDriving,
    })
    .unwrap();
    assert_eq!(
        rejected,
        serde_json::json!({"result": "rejected", "code": "FORBIDDEN_WHILE_DRIVING"})
    );
}
