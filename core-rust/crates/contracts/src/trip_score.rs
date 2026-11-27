//! `dc_trip_score` input/output contract (`schema/trip-score.schema.json`; 02 §2.1 #8,
//! 05 §6.1). Integer-only (no floating point).

use serde::{Deserialize, Serialize};

fn permille_u16<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    let v = u16::deserialize(d)?;
    if v > 1_000 {
        return Err(serde::de::Error::custom(format!(
            "permille must be in 0..=1000, got {v}"
        )));
    }
    Ok(v)
}

/// Destination kind (05 §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationKind {
    Sightseeing,
    HistoricSite,
    Castle,
    Michinoeki,
    Lodging,
    Other,
}

impl DestinationKind {
    pub fn is_trip_destination(self) -> bool {
        !matches!(self, Self::Other)
    }
}

/// Corridor historical spot density in the local pack (05 §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDensity {
    Low,
    Normal,
    High,
}

/// Provisional trip-score thresholds and point table (05 §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TripScorePolicy {
    /// Minimum score to show the C01 trip-mode card (05 §6.1: 3, provisional).
    pub threshold: i32,
    /// Medium distance threshold in meters (05 §6.1: 80 km = 80_000 m).
    pub distance_medium_m: u64,
    /// Long distance threshold in meters (05 §6.1: 150 km = 150_000 m).
    pub distance_long_m: u64,
    pub distance_medium_pts: i32,
    pub distance_long_pts: i32,
    /// Predicted ETA threshold in seconds (05 §6.1: "60〜75 分以上"; provisional single
    /// default 3600 s = 60 min).
    pub eta_threshold_s: u64,
    pub eta_pts: i32,
    /// Motorway share threshold in per-mille (05 §6.1: 50% = 500).
    #[serde(deserialize_with = "permille_u16")]
    pub motorway_ratio_threshold_permille: u16,
    pub motorway_pts: i32,
    pub destination_pts: i32,
    pub outside_daily_area_pts: i32,
    pub high_history_density_pts: i32,
    /// Deduction when user recently declined trip mode on a similar trip (05 §6.1: -3).
    pub recently_declined_similar_pts: i32,
}

impl Default for TripScorePolicy {
    fn default() -> Self {
        Self {
            threshold: 3,
            distance_medium_m: 80_000,
            distance_long_m: 150_000,
            distance_medium_pts: 2,
            distance_long_pts: 3,
            eta_threshold_s: 3_600,
            eta_pts: 2,
            motorway_ratio_threshold_permille: 500,
            motorway_pts: 1,
            destination_pts: 2,
            outside_daily_area_pts: 2,
            high_history_density_pts: 1,
            recently_declined_similar_pts: -3,
        }
    }
}

/// Request of `dc_trip_score` (02 §2.1 #8, 05 §6.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TripScoreRequest {
    pub distance_m: u64,
    pub eta_s: u64,
    #[serde(deserialize_with = "permille_u16")]
    pub motorway_ratio_permille: u16,
    pub destination_kind: DestinationKind,
    pub outside_daily_area: bool,
    pub corridor_history_density: HistoryDensity,
    pub recently_declined_similar: bool,
    /// True when re-routing while driving; 05 §6.1 suppresses the card on driving reroutes.
    pub is_driving_reroute: bool,
    #[serde(default)]
    pub policy: TripScorePolicy,
}

/// Per-factor integer score breakdown (05 §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TripScoreBreakdown {
    pub distance: i32,
    pub eta: i32,
    pub motorway: i32,
    pub destination: i32,
    pub outside_daily_area: i32,
    pub corridor_history_density: i32,
    pub recently_declined_similar: i32,
}

/// Output of `dc_trip_score` (02 §2.1 #8, 05 §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TripScoreResult {
    pub score: i32,
    pub threshold: i32,
    pub show_card: bool,
    pub suppressed_by_reroute: bool,
    pub breakdown: TripScoreBreakdown,
}
