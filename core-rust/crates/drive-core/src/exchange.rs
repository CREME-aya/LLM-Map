//! `dc_exchange_validate` (docs 06 §4.2, PR #22): strict validation of `exchange-v1` JSON
//! payloads (`IntentPreset` / `TripPlan` / `DriveDiary`). Integer-only (no floating point).
//!
//! Enforces both schema-level constraints (mirroring `schema/exchange.schema.json`) and
//! cross-field / cross-segment semantic rules shared with Web Zod:
//! - `IntentPreset`: every key in `hard_soft` must be present in `chips`; `weights_permille`
//!   axes in `250..=4000`; `name` <= 40 Unicode chars; `chips` <= 8.
//! - `TripPlan`: `vias` <= 5; WGS84 `lat_e7` / `lon_e7`; `Place.name` <= 60 Unicode chars.
//! - `DriveDiary`: total track points across all `track.segments` <= 20,000; non-null
//!   `t_offset_s` values monotonically non-decreasing across all segments (same second
//!   allowed; `null` skipped); `history` <= 200.
//!
//! Note: the 16 MB expanded-size limit for `.drivediary` zip files is enforced by Kotlin /
//! Web before passing the extracted JSON bytes to Rust.
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::session::CoreError;
use contracts::exchange::{
    ExchangeDocument, Place, DIARY_HISTORY_MAX, DIARY_TRACK_POINTS_MAX, PLACE_NAME_MAX_CHARS,
    PRESET_CHIPS_MAX, PRESET_NAME_MAX_CHARS, TRIP_PLAN_VIAS_MAX,
};
use contracts::rank::{L2_PENALTY_PERMILLE_MAX, L2_PENALTY_PERMILLE_MIN};
use contracts::tick::{LAT_E7_MAX, LON_E7_MAX};
use contracts::ErrorCode;

fn err(msg: impl Into<String>) -> CoreError {
    CoreError::new(ErrorCode::Schema, msg)
}

fn validate_coords(lat_e7: i32, lon_e7: i32) -> Result<(), CoreError> {
    if !(-LAT_E7_MAX..=LAT_E7_MAX).contains(&lat_e7) {
        return Err(err(format!("lat_e7 out of range: {lat_e7}")));
    }
    if !(-LON_E7_MAX..=LON_E7_MAX).contains(&lon_e7) {
        return Err(err(format!("lon_e7 out of range: {lon_e7}")));
    }
    Ok(())
}

fn validate_place(place: &Place, label: &str) -> Result<(), CoreError> {
    validate_coords(place.lat_e7, place.lon_e7)?;
    if let Some(name) = &place.name {
        let chars = name.chars().count();
        if chars > PLACE_NAME_MAX_CHARS {
            return Err(err(format!(
                "{label}.name exceeds {PLACE_NAME_MAX_CHARS} chars: {chars}"
            )));
        }
    }
    Ok(())
}

/// Validates a parsed `exchange-v1` document (06 §4.2).
pub fn validate(doc: &ExchangeDocument) -> Result<(), CoreError> {
    match doc {
        ExchangeDocument::IntentPreset {
            preset_id,
            name,
            chips,
            hard_soft,
            weights_permille,
            ..
        } => {
            if preset_id.is_empty() {
                return Err(err("preset_id must not be empty"));
            }
            let name_chars = name.chars().count();
            if name_chars == 0 || name_chars > PRESET_NAME_MAX_CHARS {
                return Err(err(format!(
                    "name must be 1..={PRESET_NAME_MAX_CHARS} chars, got {name_chars}"
                )));
            }
            if chips.len() > PRESET_CHIPS_MAX {
                return Err(err(format!(
                    "chips must have <= {PRESET_CHIPS_MAX} items, got {}",
                    chips.len()
                )));
            }
            if chips.iter().any(|c| c.is_empty()) {
                return Err(err("chip id must not be empty"));
            }
            for key in hard_soft.keys() {
                if !chips.iter().any(|c| c == key) {
                    return Err(err(format!("hard_soft key `{key}` is not listed in chips")));
                }
            }
            let range = L2_PENALTY_PERMILLE_MIN..=L2_PENALTY_PERMILLE_MAX;
            for (axis, v) in [
                ("right_turn", weights_permille.right_turn),
                ("narrow_road", weights_permille.narrow_road),
                ("merge", weights_permille.merge),
            ] {
                if !range.contains(&v) {
                    return Err(err(format!(
                        "weights_permille.{axis} must be in {L2_PENALTY_PERMILLE_MIN}..={L2_PENALTY_PERMILLE_MAX}, got {v}"
                    )));
                }
            }
            Ok(())
        }
        ExchangeDocument::TripPlan {
            origin,
            vias,
            destination,
            preset_ref,
            ..
        } => {
            validate_place(origin, "origin")?;
            if vias.len() > TRIP_PLAN_VIAS_MAX {
                return Err(err(format!(
                    "vias must have <= {TRIP_PLAN_VIAS_MAX} items, got {}",
                    vias.len()
                )));
            }
            for (i, via) in vias.iter().enumerate() {
                validate_place(via, &format!("vias[{i}]"))?;
            }
            validate_place(destination, "destination")?;
            if let Some(p) = preset_ref {
                if p.is_empty() {
                    return Err(err("preset_ref must not be empty"));
                }
            }
            Ok(())
        }
        ExchangeDocument::DriveDiary {
            trip_id,
            track,
            history,
            ..
        } => {
            if trip_id.is_empty() {
                return Err(err("trip_id must not be empty"));
            }
            if history.len() > DIARY_HISTORY_MAX {
                return Err(err(format!(
                    "history must have <= {DIARY_HISTORY_MAX} items, got {}",
                    history.len()
                )));
            }
            for (i, h) in history.iter().enumerate() {
                if h.poi_id.is_empty() {
                    return Err(err(format!("history[{i}].poi_id must not be empty")));
                }
            }
            let mut total_points: usize = 0;
            let mut prev_t: Option<u64> = None;
            for (si, seg) in track.segments.iter().enumerate() {
                total_points = total_points.saturating_add(seg.points.len());
                if total_points > DIARY_TRACK_POINTS_MAX {
                    return Err(err(format!(
                        "track total points exceeds {DIARY_TRACK_POINTS_MAX}"
                    )));
                }
                for (pi, pt) in seg.points.iter().enumerate() {
                    validate_coords(pt.0, pt.1)?;
                    if let Some(t) = pt.2 {
                        if let Some(prev) = prev_t {
                            if t < prev {
                                return Err(err(format!(
                                    "track.segments[{si}].points[{pi}] t_offset_s regressed: {t} < {prev}"
                                )));
                            }
                        }
                        prev_t = Some(t);
                    }
                }
            }
            Ok(())
        }
    }
}

/// Parses and validates raw UTF-8 `exchange-v1` JSON bytes (entry point for
/// `dc_exchange_validate`).
pub fn validate_json(input: &[u8]) -> Result<ExchangeDocument, CoreError> {
    let doc: ExchangeDocument =
        serde_json::from_slice(input).map_err(|e| err(format!("exchange_validate: {e}")))?;
    validate(&doc)?;
    Ok(doc)
}
