//! dc_route_commit body: bundle validation, the route_key guard, ETA from cum_time_ms and
//! the fixed-point contract (04 §3.1).
use contracts::route::{RouteBundle, RouteCommitRequest};
use contracts::tick::{Fix, TickInput};
use contracts::{ErrorCode, RouteKey};
use drive_core::geo::{round_q16, LocalFrame};
use drive_core::route::LoadedRoute;
use drive_core::{Session, SessionConfig};
use serde_json::Value;
use std::path::PathBuf;

fn schema_fixtures(kind: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../../schema/fixtures/{kind}"))
}

fn min_value() -> Value {
    serde_json::from_slice(
        &std::fs::read(schema_fixtures("valid").join("route-bundle.min.json")).unwrap(),
    )
    .unwrap()
}

fn min_bundle() -> RouteBundle {
    serde_json::from_value(min_value()).unwrap()
}

fn commit(s: &mut Session, guard: Option<RouteKey>, b: RouteBundle) -> Result<(), ErrorCode> {
    s.route_commit(&RouteCommitRequest {
        expected_route_key: guard,
        bundle: b,
    })
    .map(|_| ())
    .map_err(|e| e.code)
}

#[test]
fn schema_fixtures_load_or_are_rejected() {
    for kind in ["valid", "invalid"] {
        for e in std::fs::read_dir(schema_fixtures(kind)).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if !name.starts_with("route-bundle.") {
                continue;
            }
            let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
            let loaded = serde_json::from_value::<RouteBundle>(v)
                .map_err(|e| e.to_string())
                .and_then(|b| LoadedRoute::load(&b));
            assert_eq!(
                loaded.is_ok(),
                kind == "valid",
                "{name}: {:?}",
                loaded.err()
            );
        }
    }
}

/// Structural rules JSON Schema cannot express are checked on load (→ E_SCHEMA).
#[test]
fn structural_violations_are_e_schema() {
    type Mutation = fn(&mut Value);
    let cases: Vec<(&str, Mutation)> = vec![
        ("chunk along not continuous", |v| {
            v["legs"][0]["chunks"][1]["seg_along_cm"][0] = 19_999.into()
        }),
        ("first_shape_index gap", |v| {
            v["legs"][0]["chunks"][1]["first_shape_index"] = 3.into()
        }),
        ("first along not 0", |v| {
            v["legs"][0]["chunks"][0]["seg_along_cm"][0] = 1.into()
        }),
        ("along decreases", |v| {
            v["legs"][0]["chunks"][0]["seg_along_cm"][1] = 30_000.into()
        }),
        ("bearing count", |v| {
            v["legs"][0]["chunks"][0]["bearing_cdeg"] = serde_json::json!([9000])
        }),
        ("edge gap", |v| {
            v["legs"][0]["edges"][1]["from_pt"] = 1.into()
        }),
        ("edge length", |v| {
            v["legs"][0]["edges"][0]["length_cm"] = 19_000.into()
        }),
        ("edges short", |v| {
            v["legs"][0]["edges"].as_array_mut().unwrap().pop();
        }),
        ("cum_time decreases", |v| {
            v["legs"][0]["edges"][1]["cum_time_ms"] = 1_000.into()
        }),
        ("eta mismatch", |v| v["eta_ms"] = 3_001.into()),
        ("length mismatch", |v| v["length_cm"] = 30_001.into()),
        ("leg length mismatch", |v| {
            v["legs"][0]["length_cm"] = 29_000.into()
        }),
        ("leg_index", |v| v["legs"][0]["leg_index"] = 1.into()),
        ("along_route_start", |v| {
            v["legs"][0]["along_route_start_cm"] = 5.into()
        }),
        ("maneuver route_key", |v| {
            v["legs"][0]["maneuvers"][0]["id"]["route_key"]["route_revision"] = "9".into()
        }),
        ("maneuver order", |v| {
            v["legs"][0]["maneuvers"].as_array_mut().unwrap().reverse()
        }),
        ("anchor off its segment", |v| {
            v["legs"][0]["maneuvers"][0]["anchor"]["along_route_cm"] = 5_000.into()
        }),
        ("anchor leg_index", |v| {
            v["legs"][0]["maneuvers"][0]["anchor"]["leg_index"] = 1.into()
        }),
        ("empty legs", |v| v["legs"] = serde_json::json!([])),
    ];
    for (what, m) in cases {
        let mut v = min_value();
        m(&mut v);
        let b: RouteBundle = serde_json::from_value(v).unwrap_or_else(|e| panic!("{what}: {e}"));
        assert!(LoadedRoute::load(&b).is_err(), "{what}");
        let mut s = Session::new(SessionConfig::default());
        assert_eq!(commit(&mut s, None, b), Err(ErrorCode::Schema), "{what}");
        assert!(s.route().is_none(), "{what}: nothing replaced");
    }
}

#[test]
fn guard_route_key_is_checked_before_anything_is_replaced() {
    let mut s = Session::new(SessionConfig::default());
    let a = min_bundle();
    let ka = a.route_key.clone();
    // first route: guard null
    assert_eq!(
        commit(&mut s, Some(ka.clone()), a.clone()),
        Err(ErrorCode::Stale)
    );
    assert_eq!(commit(&mut s, None, a.clone()), Ok(()));
    assert_eq!(s.route().unwrap().route_key, ka);
    // replacing it needs its key as the guard
    let mut b = min_value();
    b["route_key"]["route_revision"] = "9".into();
    for m in b["legs"][0]["maneuvers"].as_array_mut().unwrap() {
        m["id"]["route_key"]["route_revision"] = "9".into();
    }
    let b: RouteBundle = serde_json::from_value(b).unwrap();
    assert_eq!(commit(&mut s, None, b.clone()), Err(ErrorCode::Stale));
    let mut wrong = ka.clone();
    wrong.route_revision = contracts::U64Str(1);
    assert_eq!(
        commit(&mut s, Some(wrong), b.clone()),
        Err(ErrorCode::Stale)
    );
    assert_eq!(s.route().unwrap().route_key, ka, "nothing replaced");
    assert_eq!(commit(&mut s, Some(ka.clone()), b.clone()), Ok(()));
    assert_eq!(s.route().unwrap().route_key, b.route_key);
    // the old key is stale now
    assert_eq!(commit(&mut s, Some(ka), b), Err(ErrorCode::Stale));
}

/// `min_bundle()` with `route_revision` = `rev` (route_key and maneuver ids), and optionally
/// another `trip_revision` / `graph_version`.
fn bundle_rev(rev: u64, trip_rev: Option<u64>, graph: Option<&str>) -> RouteBundle {
    let mut b = min_bundle();
    b.route_key.route_revision = contracts::U64Str(rev);
    if let Some(t) = trip_rev {
        b.route_key.trip_revision = contracts::U64Str(t);
    }
    if let Some(g) = graph {
        b.route_key.graph_version = g.into();
    }
    for leg in &mut b.legs {
        for m in &mut leg.maneuvers {
            m.id.route_key = b.route_key.clone();
        }
    }
    b
}

/// A replacement route must carry route_revision = current + 1 (`E_STALE` otherwise,
/// nothing replaced); the first route may carry any revision.
#[test]
fn replacement_route_revision_must_be_current_plus_one() {
    // first route: any revision (here 8, and 0 in another session)
    let mut s = Session::new(SessionConfig::default());
    assert_eq!(commit(&mut s, None, bundle_rev(8, None, None)), Ok(()));
    let mut s0 = Session::new(SessionConfig::default());
    assert_eq!(commit(&mut s0, None, bundle_rev(0, None, None)), Ok(()));
    let k8 = s.route().unwrap().route_key.clone();
    // same revision, skipped revision, older revision → E_STALE, nothing replaced
    for rev in [8, 10, 7, 0] {
        assert_eq!(
            commit(&mut s, Some(k8.clone()), bundle_rev(rev, None, None)),
            Err(ErrorCode::Stale),
            "rev {rev}"
        );
        assert_eq!(
            s.route().unwrap().route_key,
            k8,
            "rev {rev}: nothing replaced"
        );
    }
    // current + 1
    assert_eq!(commit(&mut s, Some(k8), bundle_rev(9, None, None)), Ok(()));
    let k9 = s.route().unwrap().route_key.clone();
    assert_eq!(k9.route_revision.0, 9);
    assert_eq!(commit(&mut s, Some(k9), bundle_rev(10, None, None)), Ok(()));
    // u64::MAX + 1 does not exist
    let mut sm = Session::new(SessionConfig::default());
    assert_eq!(
        commit(&mut sm, None, bundle_rev(u64::MAX, None, None)),
        Ok(())
    );
    let km = sm.route().unwrap().route_key.clone();
    assert_eq!(
        commit(&mut sm, Some(km.clone()), bundle_rev(0, None, None)),
        Err(ErrorCode::Stale)
    );
    assert_eq!(
        commit(&mut sm, Some(km), bundle_rev(u64::MAX, None, None)),
        Err(ErrorCode::Stale)
    );
}

/// The guard compares the whole RouteKey (04 §3: trip_revision / route_revision /
/// graph_version): a guard differing only in trip_revision or graph_version is E_STALE.
#[test]
fn guard_mismatch_in_trip_revision_or_graph_version_is_stale() {
    let mut s = Session::new(SessionConfig::default());
    assert_eq!(commit(&mut s, None, bundle_rev(8, None, None)), Ok(()));
    let k = s.route().unwrap().route_key.clone();
    let mut g = k.clone();
    g.trip_revision = contracts::U64Str(k.trip_revision.0 + 1);
    assert_eq!(
        commit(&mut s, Some(g), bundle_rev(9, None, None)),
        Err(ErrorCode::Stale)
    );
    let mut g = k.clone();
    g.graph_version = "jp-2026.12.0".into();
    assert_eq!(
        commit(&mut s, Some(g), bundle_rev(9, None, None)),
        Err(ErrorCode::Stale)
    );
    let mut g = k.clone();
    g.trip_id = "other".into();
    assert_eq!(
        commit(&mut s, Some(g), bundle_rev(9, None, None)),
        Err(ErrorCode::Stale)
    );
    assert_eq!(s.route().unwrap().route_key, k, "nothing replaced");
}

#[test]
fn remaining_eta_from_cum_time_ms() {
    let r = LoadedRoute::load(&min_bundle()).unwrap();
    // edges: 0..20000 cm → 2000 ms, 20000..30000 cm → 3000 ms
    assert_eq!(r.time_at(0), 0);
    assert_eq!(r.time_at(10_000), 1_000);
    assert_eq!(r.time_at(20_000), 2_000);
    assert_eq!(r.time_at(25_000), 2_500);
    assert_eq!(r.time_at(30_000), 3_000);
    assert_eq!(r.remaining_ms(25_000), 500);
    assert_eq!(r.remaining_ms(40_000), 0);
    assert_eq!(r.segs.len(), 3);
    assert_eq!(r.chunks.len(), 2);
    // along is continuous across the chunk boundary
    assert_eq!(r.segs[1].along_b, r.segs[2].along_a);
}

fn fix_at(at: u64, lat_e7: i32, lon_e7: i32) -> Fix {
    Fix {
        at_mono_ms: at,
        lat_e7,
        lon_e7,
        acc_cm: 300,
        bearing_cdeg: None,
        speed_cm_s: None,
    }
}

/// Near a chunk boundary the fix is converted with both chunks; the smaller lateral
/// distance wins. min bundle: chunk 0 goes east 200 m, chunk 1 turns north at the shared
/// point. A fix 20 m north of the turn, 1 m east of the line, lies inside both chunks'
/// ranges and is a candidate in both: chunk 0's last segment reaches it only at its end
/// point (lateral ~20 m, along 20000), chunk 1 projects it at 1 m (along 22000) → chunk 1.
#[test]
fn chunk_boundary_takes_the_smaller_lateral_distance() {
    let b = min_bundle();
    let c1 = &b.legs[0].chunks[1];
    let f1 = LocalFrame {
        origin_lat_e7: c1.origin_e7.lat,
        origin_lon_e7: c1.origin_e7.lon,
        k_lat_q16: c1.k_lat_q16,
        k_lon_q16: c1.k_lon_q16,
    };
    let mut s = Session::new(SessionConfig::default());
    commit(&mut s, None, b.clone()).unwrap();
    // 1e-7 deg ≈ 1.1 cm north; 2000 cm north of chunk 1's origin, ~100 cm east
    let lat = c1.origin_e7.lat + 1_797;
    let lon = c1.origin_e7.lon + 110;
    let (x, y) = f1.to_local(lat, lon);
    assert!(
        (99..=101).contains(&x) && (2_000..=2_001).contains(&y),
        "{x} {y}"
    );
    let out = s.tick(TickInput {
        now_mono_ms: 1,
        fix: Some(fix_at(1, lat, lon)),
        events: vec![],
    });
    assert!(out.horizon.is_some());
    let p = s.aligner().progress_cm().unwrap();
    assert!((21_999..=22_001).contains(&p), "{p}");
}

/// Ties on the lateral distance take the smaller along: a route that turns back on the
/// same line (U-turn); a fix on the line within the local window matches the outbound part.
#[test]
fn equal_lateral_takes_the_smaller_along() {
    let mut v = min_value();
    let c = &mut v["legs"][0]["chunks"];
    *c = serde_json::json!([{
        "first_shape_index": 0, "origin_e7": {"lat": 350000000, "lon": 1390000000},
        "k_lat_q16": 72955, "k_lon_q16": 59762,
        "points_cm": [[0,0],[10000,0],[0,0]], "seg_along_cm": [0,10000,20000],
        "bearing_cdeg": [9000, 27000]}]);
    v["legs"][0]["length_cm"] = 20_000.into();
    v["length_cm"] = 20_000.into();
    v["eta_ms"] = 2_000.into();
    v["legs"][0]["edges"] = serde_json::json!([
        {"edge_id":"1","from_pt":0,"to_pt":2,"length_cm":20000,"cum_time_ms":2000,"flags":0}]);
    let m = v["legs"][0]["maneuvers"][1].clone();
    v["legs"][0]["maneuvers"] = serde_json::json!([m]);
    v["legs"][0]["maneuvers"][0]["anchor"] =
        serde_json::json!({"leg_index":0,"leg_shape_index":2,"along_route_cm":20000});
    v["legs"][0]["maneuvers"][0]["id"]["ordinal"] = 0.into();
    let b: RouteBundle = serde_json::from_value(v).unwrap();
    let mut s = Session::new(SessionConfig::default());
    commit(&mut s, None, b).unwrap();
    // first fix at the start (unique: along 0 and 20000 differ by 200 m → ambiguous!)
    // so start tracking with a fix 10 cm into the outbound segment at a usable heading
    let mut f = fix_at(1, 350_000_000, 1_390_000_009);
    f.bearing_cdeg = Some(9_000);
    f.speed_cm_s = Some(500);
    s.tick(TickInput {
        now_mono_ms: 1,
        fix: Some(f),
        events: vec![],
    });
    assert!(s.aligner().progress_cm().unwrap() < 100);
    // now standing still at x = 50 m: both parts within the local window, same lateral
    let f = fix_at(1_000, 350_000_000, 1_390_000_000 + 5_483);
    s.tick(TickInput {
        now_mono_ms: 1_000,
        fix: Some(f),
        events: vec![],
    });
    let p = s.aligner().progress_cm().unwrap();
    assert!((4_990..=5_010).contains(&p), "outbound part: {p}");
}

/// The fixed-point contract at the docs' chunk length (20 km, plus the 1 km margin) and a
/// high latitude: no overflow, error against the exact (float) conversion within ~20 cm.
/// The f64 here plays C++ (computing k with cos once); Rust only uses the integers.
#[test]
fn fixed_point_error_at_20km_chunk() {
    let cm_per_e7_lat = 2.0 * std::f64::consts::PI * 6_378_137.0 / 360.0 / 1e7 * 100.0;
    let mut worst: f64 = 0.0;
    for lat_deg in [24.0_f64, 35.0, 43.0, 45.5] {
        let exact_lon = cm_per_e7_lat * lat_deg.to_radians().cos();
        let f = LocalFrame {
            origin_lat_e7: (lat_deg * 1e7).round() as i32,
            origin_lon_e7: 1_400_000_000,
            k_lat_q16: (cm_per_e7_lat * 65536.0).round() as u32,
            k_lon_q16: (exact_lon * 65536.0).round() as u32,
        };
        // 21 km east and north (chunk 20 km + 1 km margin), both signs
        let dlon = (2_100_000.0 / exact_lon) as i32;
        let dlat = (2_100_000.0 / cm_per_e7_lat) as i32;
        for (sx, sy) in [(1, 1), (-1, -1), (1, -1)] {
            let (x, y) = f.to_local(f.origin_lat_e7 + sy * dlat, f.origin_lon_e7 + sx * dlon);
            let ex = f64::from(sx * dlon) * exact_lon;
            let ey = f64::from(sy * dlat) * cm_per_e7_lat;
            worst = worst.max((x as f64 - ex).abs()).max((y as f64 - ey).abs());
            // intermediate magnitude stays far below i64 (docs estimate ~1.3e11)
            assert!((i64::from(dlon) * i64::from(f.k_lon_q16)).abs() < 1 << 40);
        }
    }
    println!("[fixed-point] worst error at 21 km from the chunk origin: {worst:.2} cm");
    assert!(worst < 25.0, "{worst}");
    // rounding rule: exact halves toward +inf
    assert_eq!(round_q16(1 << 15), 1);
    assert_eq!(round_q16(-(1 << 15)), 0);
}
