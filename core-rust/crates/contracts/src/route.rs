//! Route body passed once to `dc_route_commit` (docs 04 §3.1). Mirrors
//! `schema/route-bundle.schema.json`; structural checks that JSON Schema cannot express
//! (continuity, ordering) are done by `drive-core` when the bundle is loaded.
//!
//! Fixed-point coordinate contract (04 §3.1, team proposal, provisional):
//! - Each chunk has an origin `origin_e7 {lat, lon}` (1e-7 degrees) and two factors
//!   `k_lat_q16` / `k_lon_q16` = cm per 1e-7 degree in Q16 (u32). C++ computes them once
//!   (the longitude factor uses `cos(lat)`); Rust never computes them.
//! - `dx_cm = round_q16((lon_e7 - origin.lon) as i64 * k_lon_q16)`,
//!   `dy_cm = round_q16((lat_e7 - origin.lat) as i64 * k_lat_q16)`,
//!   `round_q16(v) = (v + (1 << 15)) >> 16` (arithmetic shift: exact halves go to +inf).
//! - `points_cm[i] = [dx, dy]` relative to the chunk origin (x = east, y = north).
//! - A chunk is valid for a fix inside the bounding box of its points plus a 1 km margin.
use crate::common::{RouteKey, U64Str};
use crate::horizon::Maneuver;
use serde::{Deserialize, Deserializer, Serialize};

/// `dc_route_commit` input: `{ req_id, expected_route_key, bundle }`.
///
/// `req_id` is mandatory (`null` / omitted is `E_SCHEMA`, 02 §2.1 #9): it must name either
/// (1) a `commit{req_id, route_key}` Rust emitted and has not settled (`undo-<req_id>` for
/// an undo) with `route_key == expected_route_key`, or (2) an `ok` candidate search of the
/// current plan while its commit window is still open (closed when the next plan starts,
/// when one of its candidates is committed, or when the vehicle starts moving →
/// `E_FORBIDDEN_WHILE_DRIVING`). Any other `req_id` is `E_STALE`.
///
/// `expected_route_key` is the guard: the current route's key, or `null` when no route is
/// committed yet (the first route of the session). It must equal the current route (`null` =
/// none), otherwise `E_STALE` and nothing is replaced. `bundle.route_key` (and every
/// `ManeuverId.route_key` in the bundle) carries `route_revision` "0" (`E_SCHEMA` otherwise):
/// Rust assigns the revision (current + 1, the first route: 1; never reset when
/// `trip_revision` advances) and returns the new key in `RouteCommitResult.route_key`. While
/// a route is current, the bundle's `trip_id` / `graph_version` must equal it, and
/// `trip_revision` must equal it or — only for a plan candidate whose plan has that
/// `trip_revision` — equal current + 1 (`E_STALE` otherwise).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCommitRequest {
    pub req_id: String,
    #[serde(deserialize_with = "crate::common::required_nullable")]
    pub expected_route_key: Option<RouteKey>,
    pub bundle: RouteBundle,
}

/// `RouteBundle = { route_key, eta_ms, length_cm, legs[], history_pois? }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteBundle {
    /// Key of this (new) route, with `route_revision` "0" (Rust assigns the revision in
    /// `dc_route_commit`; any other value is rejected → `E_SCHEMA`).
    #[serde(deserialize_with = "bundle_route_key")]
    pub route_key: RouteKey,
    /// Total travel time (ms) = `cum_time_ms` of the last edge of the last leg.
    pub eta_ms: u32,
    /// Total length (cm) = sum of the legs' `length_cm`.
    pub length_cm: u32,
    pub legs: Vec<Leg>,
    /// Optional history commentary POIs along the route corridor (05 §6.2, 04 §3.1).
    /// Text never crosses the boundary: only `content_ref` and `duration_ms`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history_pois: Vec<HistoryPoi>,
}

/// One history commentary POI placed along the route corridor (05 §6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPoi {
    pub poi_id: String,
    pub along_route_cm: u32,
    pub versions: Vec<HistoryVersion>,
}

/// Pre-packaged commentary version inside Kotlin's history pack (05 §6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryVersion {
    pub kind: HistoryVersionKind,
    pub content_ref: String,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryVersionKind {
    Short,
    Summary,
}

/// One leg. Distances exist both per leg (`seg_along_cm`, from the leg start) and for the
/// whole route (`along_route_start_cm` + leg value = `along_route_cm`, also used by
/// `Maneuver.anchor.along_route_cm`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Leg {
    /// Equal to the position in `legs`.
    pub leg_index: u32,
    /// `along_route_cm` of the leg's first point (sum of the previous legs' `length_cm`).
    pub along_route_start_cm: u32,
    /// Leg length (cm) = `seg_along_cm` of the last point of the last chunk.
    pub length_cm: u32,
    /// Shape of the leg, split into chunks (about 20 km each, 07 §1). Consecutive chunks
    /// share their boundary point (same `leg_shape_index`, same `seg_along_cm`).
    pub chunks: Vec<Chunk>,
    /// Edges covering the leg shape without gaps (`edges[0].from_pt = 0`,
    /// `edges[i].from_pt = edges[i-1].to_pt`, last `to_pt` = last shape index).
    pub edges: Vec<Edge>,
    /// Maneuvers of this leg in route order (`Maneuver` of 04 §2, normalized by C++).
    pub maneuvers: Vec<Maneuver>,
}

/// One chunk of a leg shape with its own origin and fixed-point factors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    /// `leg_shape_index` of `points_cm[0]`.
    pub first_shape_index: u32,
    pub origin_e7: OriginE7,
    /// cm per 1e-7 degree of latitude, Q16 (about 72,955 = 1.1132 cm). `1..=2^20`.
    #[serde(deserialize_with = "k_q16")]
    pub k_lat_q16: u32,
    /// cm per 1e-7 degree of longitude, Q16 (`k_lat_q16 * cos(lat)`, computed by C++).
    #[serde(deserialize_with = "k_q16")]
    pub k_lon_q16: u32,
    /// Points `[dx_cm, dy_cm]` relative to `origin_e7` (x east, y north). At least 2.
    pub points_cm: Vec<[i32; 2]>,
    /// Distance from the leg start to each point (cm, non-decreasing). Same length as
    /// `points_cm`.
    pub seg_along_cm: Vec<u32>,
    /// Heading of each segment `points_cm[i] -> points_cm[i+1]` in centidegrees clockwise
    /// from north (`0..=35999`), computed by C++ (Rust has no atan2). Length = points - 1.
    #[serde(deserialize_with = "bearings_cdeg")]
    pub bearing_cdeg: Vec<u16>,
}

/// Chunk origin in 1e-7 degrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginE7 {
    #[serde(deserialize_with = "lat_e7")]
    pub lat: i32,
    #[serde(deserialize_with = "lon_e7")]
    pub lon: i32,
}

/// One road edge of the route (Valhalla edge), by leg shape index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub edge_id: U64Str,
    /// Leg shape index where the edge starts / ends (`from_pt < to_pt`).
    pub from_pt: u32,
    pub to_pt: u32,
    /// `seg_along_cm[to_pt] - seg_along_cm[from_pt]`.
    pub length_cm: u32,
    /// Travel time from the route start to the end of this edge (ms, non-decreasing over
    /// the whole route). Remaining ETA = `eta_ms` - time at the matched position.
    pub cum_time_ms: u32,
    /// Bit set, see `EDGE_FLAG_*`. TODO(schema-v1): flag catalogue is provisional.
    pub flags: u32,
}

/// Provisional edge flags (not in docs yet).
pub const EDGE_FLAG_TUNNEL: u32 = 1 << 0;
pub const EDGE_FLAG_MOTORWAY: u32 = 1 << 1;
pub const EDGE_FLAG_TOLL: u32 = 1 << 2;
pub const EDGE_FLAG_FERRY: u32 = 1 << 3;

/// Upper bound accepted for `k_*_q16` (16 cm per 1e-7 degree, far above the real
/// 1.1132): keeps `(delta_e7 as i64) * k` well inside i64.
pub const K_Q16_MAX: u32 = 1 << 20;

fn bundle_route_key<'de, D: Deserializer<'de>>(d: D) -> Result<RouteKey, D::Error> {
    let k = RouteKey::deserialize(d)?;
    if k.route_revision.0 != 0 {
        return Err(serde::de::Error::custom(format!(
            "route_key.route_revision must be \"0\" (assigned by Rust), got {}",
            k.route_revision.0
        )));
    }
    Ok(k)
}

fn lat_e7<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    let v = i32::deserialize(d)?;
    if !(-crate::tick::LAT_E7_MAX..=crate::tick::LAT_E7_MAX).contains(&v) {
        return Err(serde::de::Error::custom(format!("lat out of range: {v}")));
    }
    Ok(v)
}

fn lon_e7<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    let v = i32::deserialize(d)?;
    if !(-crate::tick::LON_E7_MAX..=crate::tick::LON_E7_MAX).contains(&v) {
        return Err(serde::de::Error::custom(format!("lon out of range: {v}")));
    }
    Ok(v)
}

/// `dc_session_new` optional `resume: { route_bundle, route_key }` (docs 04 §3.2, PR #23):
/// rebuilds a session after `E_PANIC` without reassigning `route_revision` (so on-screen
/// `ManeuverId.route_key` stays stable) and without `req_id` matching.
///
/// Accepts `route_bundle` carrying either `route_revision: "0"` (original C++ bundle) or the
/// exact `resume.route_key.route_revision` (>= 1). `trip_id`, `trip_revision`, and
/// `graph_version` must match `resume.route_key`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionResume {
    pub route_bundle: RouteBundle,
    pub route_key: RouteKey,
}

impl<'de> Deserialize<'de> for SessionResume {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResumeBundleWire {
            route_key: RouteKey,
            eta_ms: u32,
            length_cm: u32,
            legs: Vec<Leg>,
            #[serde(default)]
            history_pois: Vec<HistoryPoi>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SessionResumeWire {
            route_bundle: ResumeBundleWire,
            route_key: RouteKey,
        }
        let w = SessionResumeWire::deserialize(d)?;
        if w.route_key.route_revision.0 == 0 {
            return Err(serde::de::Error::custom(
                "resume.route_key.route_revision must be >= 1",
            ));
        }
        let b_rk = &w.route_bundle.route_key;
        if b_rk.trip_id != w.route_key.trip_id
            || b_rk.trip_revision != w.route_key.trip_revision
            || b_rk.graph_version != w.route_key.graph_version
        {
            return Err(serde::de::Error::custom(
                "resume.route_bundle.route_key does not match resume.route_key",
            ));
        }
        if b_rk.route_revision.0 != 0 && b_rk.route_revision != w.route_key.route_revision {
            return Err(serde::de::Error::custom(format!(
                "resume.route_bundle.route_key.route_revision must be \"0\" or \"{}\", got \"{}\"",
                w.route_key.route_revision.0, b_rk.route_revision.0
            )));
        }
        Ok(SessionResume {
            route_bundle: RouteBundle {
                route_key: w.route_bundle.route_key,
                eta_ms: w.route_bundle.eta_ms,
                length_cm: w.route_bundle.length_cm,
                legs: w.route_bundle.legs,
                history_pois: w.route_bundle.history_pois,
            },
            route_key: w.route_key,
        })
    }
}

/// `dc_route_commit` output data: the new current route (with the revision Rust assigned)
/// and what was loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCommitResult {
    pub route_key: RouteKey,
    pub legs: u32,
    pub segments: u32,
    pub maneuvers: u32,
}

fn k_q16<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = u32::deserialize(d)?;
    if v == 0 || v > K_Q16_MAX {
        return Err(serde::de::Error::custom(format!(
            "k_*_q16 out of range (1..=2^20): {v}"
        )));
    }
    Ok(v)
}

fn bearings_cdeg<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u16>, D::Error> {
    let v = Vec::<u16>::deserialize(d)?;
    if let Some(b) = v.iter().find(|&&b| b >= crate::tick::BEARING_CDEG_FULL) {
        return Err(serde::de::Error::custom(format!(
            "bearing_cdeg out of range (0..=35999): {b}"
        )));
    }
    Ok(v)
}
