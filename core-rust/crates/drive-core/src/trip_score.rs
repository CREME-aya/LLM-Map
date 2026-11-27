//! `dc_trip_score` — long-distance / trip-mode card (C01) trigger scoring (docs 02 §2.1 #8,
//! 05 §6.1). Integer-only (no `f32` / `f64`).
//!
//! All thresholds and weights are provisional defaults in [`TripScorePolicy`]:
//! - Route distance: `>= 150 km` → `+3`, else `>= 80 km` → `+2`, else `0`
//! - Predicted ETA: `>= 60 min` (`3600 s`, single provisional default from "60〜75 分以上") → `+2`
//! - Motorway ratio: `>= 500` permille (50%) → `+1`
//! - Destination kind: sightseeing / historic_site / castle / michinoeki / lodging → `+2`
//! - Outside daily driving area: `true` → `+2`
//! - Corridor historical spot density: `high` → `+1`
//! - Recent decline on a similar trip: `true` → `-3`
//! - Driving reroute (`is_driving_reroute == true`): `show_card = false` regardless of score.
//! - Otherwise: `show_card = score >= policy.threshold` (default `3`).
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::session::CoreError;
use contracts::trip_score::{
    HistoryDensity, TripScoreBreakdown, TripScoreRequest, TripScoreResult,
};
use contracts::ErrorCode;

/// Evaluate the 05 §6.1 trip score. Pure, deterministic, integer-only.
pub fn evaluate(req: &TripScoreRequest) -> Result<TripScoreResult, CoreError> {
    let p = &req.policy;
    if req.motorway_ratio_permille > 1_000 {
        return Err(CoreError::new(
            ErrorCode::Schema,
            "motorway_ratio_permille must be in 0..=1000",
        ));
    }
    if p.motorway_ratio_threshold_permille > 1_000 {
        return Err(CoreError::new(
            ErrorCode::Schema,
            "policy.motorway_ratio_threshold_permille must be in 0..=1000",
        ));
    }
    if p.distance_long_m < p.distance_medium_m {
        return Err(CoreError::new(
            ErrorCode::Schema,
            "policy.distance_long_m must be >= policy.distance_medium_m",
        ));
    }

    let distance = if req.distance_m >= p.distance_long_m {
        p.distance_long_pts
    } else if req.distance_m >= p.distance_medium_m {
        p.distance_medium_pts
    } else {
        0
    };
    let eta = if req.eta_s >= p.eta_threshold_s {
        p.eta_pts
    } else {
        0
    };
    let motorway = if req.motorway_ratio_permille >= p.motorway_ratio_threshold_permille {
        p.motorway_pts
    } else {
        0
    };
    let destination = if req.destination_kind.is_trip_destination() {
        p.destination_pts
    } else {
        0
    };
    let outside_daily_area = if req.outside_daily_area {
        p.outside_daily_area_pts
    } else {
        0
    };
    let corridor_history_density = if req.corridor_history_density == HistoryDensity::High {
        p.high_history_density_pts
    } else {
        0
    };
    let recently_declined_similar = if req.recently_declined_similar {
        p.recently_declined_similar_pts
    } else {
        0
    };

    let score = distance
        .saturating_add(eta)
        .saturating_add(motorway)
        .saturating_add(destination)
        .saturating_add(outside_daily_area)
        .saturating_add(corridor_history_density)
        .saturating_add(recently_declined_similar);

    let show_card = !req.is_driving_reroute && score >= p.threshold;

    Ok(TripScoreResult {
        score,
        threshold: p.threshold,
        show_card,
        suppressed_by_reroute: req.is_driving_reroute,
        breakdown: TripScoreBreakdown {
            distance,
            eta,
            motorway,
            destination,
            outside_daily_area,
            corridor_history_density,
            recently_declined_similar,
        },
    })
}
