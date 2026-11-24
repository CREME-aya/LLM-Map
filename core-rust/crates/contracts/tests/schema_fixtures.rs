//! schema/ conformance: every fixture in schema/fixtures/valid must pass its schema,
//! every fixture in schema/fixtures/invalid must fail. File name prefix = schema name.
//! Also checks that the Rust serde mirror agrees for PersonalRoutingOverride.

use contracts::override_types::PersonalRoutingOverride;
use jsonschema::JSONSchema;
use serde_json::Value;
use std::path::{Path, PathBuf};

const BASE: &str = "https://slm-map.invalid/schema/";
const SCHEMAS: [&str; 5] = [
    "common",
    "trip-plan-snapshot",
    "candidate-facts",
    "maneuver-horizon",
    "personal-routing-override",
];

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../schema")
}

fn load(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn compile(name: &str) -> JSONSchema {
    let mut opts = JSONSchema::options();
    opts.with_draft(jsonschema::Draft::Draft202012);
    for s in SCHEMAS {
        let doc = load(&schema_dir().join(format!("{s}.schema.json")));
        opts.with_document(format!("{BASE}{s}.schema.json"), doc);
    }
    let root = load(&schema_dir().join(format!("{name}.schema.json")));
    opts.compile(&root)
        .unwrap_or_else(|e| panic!("compile {name}: {e}"))
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
