//! `exchange-v1` contract (`schema/exchange.schema.json`; docs 06 §4.2, PR #22).
//!
//! Shared between Android (Rust `dc_exchange_validate`) and Web (Zod). Integer-only:
//! coordinates are `lat_e7` / `lon_e7`, OSM/Valhalla IDs are `U64Str`.
use crate::common::U64Str;
use crate::override_types::{Detour, SchemaV1};
use crate::rank::{L2_PENALTY_PERMILLE_MAX, L2_PENALTY_PERMILLE_MIN};
use crate::tick::{LAT_E7_MAX, LON_E7_MAX};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

pub const PRESET_NAME_MAX_CHARS: usize = 40;
pub const PRESET_CHIPS_MAX: usize = 8;
pub const TRIP_PLAN_VIAS_MAX: usize = 5;
pub const PLACE_NAME_MAX_CHARS: usize = 60;
pub const DIARY_HISTORY_MAX: usize = 200;
pub const DIARY_TRACK_POINTS_MAX: usize = 20_000;

fn lat_e7<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    let v = i32::deserialize(d)?;
    if !(-LAT_E7_MAX..=LAT_E7_MAX).contains(&v) {
        return Err(serde::de::Error::custom(format!(
            "lat_e7 out of range: {v}"
        )));
    }
    Ok(v)
}

fn lon_e7<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    let v = i32::deserialize(d)?;
    if !(-LON_E7_MAX..=LON_E7_MAX).contains(&v) {
        return Err(serde::de::Error::custom(format!(
            "lon_e7 out of range: {v}"
        )));
    }
    Ok(v)
}

fn non_empty_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    if s.is_empty() {
        return Err(serde::de::Error::custom("must not be empty"));
    }
    Ok(s)
}

fn opt_non_empty_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) if !s.is_empty() => Ok(Some(s)),
        Some(_) => Err(serde::de::Error::custom("must not be empty")),
    }
}

fn preset_name<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    let n = s.chars().count();
    if n == 0 || n > PRESET_NAME_MAX_CHARS {
        return Err(serde::de::Error::custom(format!(
            "preset name must be 1..={PRESET_NAME_MAX_CHARS} chars, got {n}"
        )));
    }
    Ok(s)
}

fn opt_place_name<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) if s.chars().count() <= PLACE_NAME_MAX_CHARS => Ok(Some(s)),
        Some(s) => Err(serde::de::Error::custom(format!(
            "place name must be <= {PLACE_NAME_MAX_CHARS} chars, got {}",
            s.chars().count()
        ))),
    }
}

fn chips_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let v = Vec::<String>::deserialize(d)?;
    if v.len() > PRESET_CHIPS_MAX {
        return Err(serde::de::Error::custom(format!(
            "chips must have <= {PRESET_CHIPS_MAX} items, got {}",
            v.len()
        )));
    }
    if v.iter().any(|s| s.is_empty()) {
        return Err(serde::de::Error::custom("chip id must not be empty"));
    }
    Ok(v)
}

fn hard_soft_map<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, HardSoft>, D::Error> {
    let m = BTreeMap::<String, HardSoft>::deserialize(d)?;
    if m.keys().any(|k| k.is_empty()) {
        return Err(serde::de::Error::custom("hard_soft key must not be empty"));
    }
    Ok(m)
}

fn weight_permille<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = u64::deserialize(d)?;
    if !(L2_PENALTY_PERMILLE_MIN..=L2_PENALTY_PERMILLE_MAX).contains(&v) {
        return Err(serde::de::Error::custom(format!(
            "weight permille must be in {L2_PENALTY_PERMILLE_MIN}..={L2_PENALTY_PERMILLE_MAX}, got {v}"
        )));
    }
    Ok(v)
}

fn vias_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Place>, D::Error> {
    let v = Vec::<Place>::deserialize(d)?;
    if v.len() > TRIP_PLAN_VIAS_MAX {
        return Err(serde::de::Error::custom(format!(
            "vias must have <= {TRIP_PLAN_VIAS_MAX} items, got {}",
            v.len()
        )));
    }
    Ok(v)
}

fn history_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<DiaryHistoryEntry>, D::Error> {
    let v = Vec::<DiaryHistoryEntry>::deserialize(d)?;
    if v.len() > DIARY_HISTORY_MAX {
        return Err(serde::de::Error::custom(format!(
            "history must have <= {DIARY_HISTORY_MAX} items, got {}",
            v.len()
        )));
    }
    Ok(v)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardSoft {
    Hard,
    Soft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeightsPermille {
    #[serde(deserialize_with = "weight_permille")]
    pub right_turn: u64,
    #[serde(deserialize_with = "weight_permille")]
    pub narrow_road: u64,
    #[serde(deserialize_with = "weight_permille")]
    pub merge: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    #[serde(deserialize_with = "lat_e7")]
    pub lat_e7: i32,
    #[serde(deserialize_with = "lon_e7")]
    pub lon_e7: i32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "opt_place_name"
    )]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_entry_id: Option<U64Str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    Off,
    On,
}

/// `[lat_e7, lon_e7, t_offset_s | null]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackPoint(
    #[serde(deserialize_with = "lat_e7")] pub i32,
    #[serde(deserialize_with = "lon_e7")] pub i32,
    pub Option<u64>,
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackSegment {
    pub points: Vec<TrackPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiaryTrack {
    pub segments: Vec<TrackSegment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiaryAvoided {
    pub right_unprotected: u64,
    pub narrow_m: u64,
    pub merges: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiaryHistoryOutcome {
    Completed,
    Interrupted,
    Saved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiaryHistoryEntry {
    #[serde(deserialize_with = "non_empty_string")]
    pub poi_id: String,
    pub outcome: DiaryHistoryOutcome,
}

/// Top-level `exchange-v1` document (`kind: intent_preset | trip_plan | drive_diary`,
/// `schema_version: 1`, `additionalProperties: false` on every level).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExchangeDocument {
    IntentPreset {
        schema_version: SchemaV1,
        #[serde(deserialize_with = "non_empty_string")]
        preset_id: String,
        #[serde(deserialize_with = "preset_name")]
        name: String,
        #[serde(deserialize_with = "chips_vec")]
        chips: Vec<String>,
        #[serde(deserialize_with = "hard_soft_map")]
        hard_soft: BTreeMap<String, HardSoft>,
        detour: Detour,
        weights_permille: WeightsPermille,
    },
    TripPlan {
        schema_version: SchemaV1,
        origin: Place,
        #[serde(deserialize_with = "vias_vec")]
        vias: Vec<Place>,
        destination: Place,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "opt_non_empty_string"
        )]
        preset_ref: Option<String>,
        history_mode: HistoryMode,
    },
    DriveDiary {
        schema_version: SchemaV1,
        #[serde(deserialize_with = "non_empty_string")]
        trip_id: String,
        started_at_unix_s: u64,
        track: DiaryTrack,
        avoided: DiaryAvoided,
        #[serde(deserialize_with = "history_vec")]
        history: Vec<DiaryHistoryEntry>,
    },
}
