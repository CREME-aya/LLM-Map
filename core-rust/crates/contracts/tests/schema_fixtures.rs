//! schema/ conformance: every fixture in schema/fixtures/valid must pass its schema,
//! every fixture in schema/fixtures/invalid must fail. File name prefix = schema name.
//! Also checks that the Rust serde mirror agrees for PersonalRoutingOverride and for
//! tick.schema.json (contracts::tick, including core-rust/tests/fixtures/tick).

use contracts::override_types::PersonalRoutingOverride;
use jsonschema::JSONSchema;
use serde_json::Value;
use std::path::{Path, PathBuf};

const BASE: &str = "https://slm-map.invalid/schema/";
const SCHEMAS: [&str; 6] = [
    "common",
    "trip-plan-snapshot",
    "candidate-facts",
    "maneuver-horizon",
    "personal-routing-override",
    "tick",
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

// ---- tick.schema.json ⇔ contracts::tick ------------------------------------------------

mod tick {
    use super::*;
    use contracts::tick::{
        DistanceClass, Event, LogEvent, PlannerCmd, SearchPriority, TickInput, TickOutput, UiCmd,
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
    /// parses with serde; `request` priorities are SearchPriority values.
    #[test]
    fn replay_fixtures_pass_tick_schema() {
        let input = tick_def("TickInput");
        let prio = tick_def("SearchPriority");
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
                    o => panic!("{}: unknown step {o}", p.display()),
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
            planner_cmds.push(PlannerCmd::Start {
                req_id: "r2".into(),
                priority,
                route_key: rk(),
                graph_version: "g".into(),
            });
        }
        let horizon = load(&schema_dir().join("fixtures/valid/maneuver-horizon.min.json"));
        for (next_wake, horizon) in [(None, None), (Some(u64::MAX), Some(horizon))] {
            let out = TickOutput {
                horizon,
                voice_cmds: vec![],
                ui_cmds: vec![
                    UiCmd::ShowCheckingRoute,
                    UiCmd::SearchResultReady {
                        req_id: "r2".into(),
                        comparison_ref: None,
                    },
                    UiCmd::SearchResultReady {
                        req_id: "r2".into(),
                        comparison_ref: Some("cmp".into()),
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
        // schema lists priorities highest first, matching Ord (higher wins)
        let order: Vec<SearchPriority> = doc["$defs"]["SearchPriority"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| serde_json::from_value(x.clone()).unwrap())
            .collect();
        assert!(order.windows(2).all(|w| w[0] > w[1]), "{order:?}");
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
        // DistanceClass is internal to the planner today, but snake_case like the rest.
        assert_eq!(serde_json::to_value(DistanceClass::Short).unwrap(), "short");
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
        assert_eq!(schema_kinds.len(), 16, "v2 has 16 event kinds");
    }
}
