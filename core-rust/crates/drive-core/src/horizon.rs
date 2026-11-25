//! `ManeuverHorizon` generation (docs 04 §2 / §3). Integer-only (see `geo`).
//!
//! - `upcoming`: the next 3 maneuvers in route order whose anchor lies ahead of the
//!   matched progress (`anchor.along_route_cm > progress`; passed = at or behind it).
//!   The first entry carries `from_vehicle_m`, the others `seg_from_prev_m`.
//! - `tracking`: distances follow the progress. `uncertain`: distance updates and new
//!   proposals are suppressed, so the last tracking `upcoming` is kept as is (before any
//!   match: the first 3 maneuvers without `from_vehicle_m`). `off_route` / `rerouting`:
//!   the old guidance is withheld (`upcoming` empty). `finished`: empty.
//! - `frame_seq` grows by one only when the content changes (everything except
//!   `frame_seq`, `sampled_at_monotonic_ms` and `valid_until_monotonic_ms`).
//!
//! TODO(drive-core): passing needs 2 consecutive high-confidence samples (04 §3) — today a
//! single on-route fix at or past the anchor passes it; `group_id` (04 §2.1 item 4),
//! `eta_range_ms`, `next_p1_deadline_monotonic_ms`, `next_complex_window` and `refs` are
//! not produced yet.
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::route::LoadedRoute;
use contracts::horizon::{Arrow, HorizonEntry, HorizonStatus, ManeuverHorizon, UPCOMING_MAX};

/// The next `UPCOMING_MAX` maneuvers ahead of `progress_cm` (`None`: from the start,
/// without `from_vehicle_m`).
pub fn upcoming(route: &LoadedRoute, progress_cm: Option<u32>) -> Vec<HorizonEntry> {
    let from = progress_cm.unwrap_or(0);
    let mut prev_along: Option<u32> = None;
    route
        .maneuvers
        .iter()
        .filter(|m| match progress_cm {
            Some(p) => m.anchor.along_route_cm > p,
            None => true,
        })
        .take(UPCOMING_MAX)
        .map(|m| {
            let a = m.anchor.along_route_cm;
            let (from_vehicle_m, seg_from_prev_m) = match prev_along {
                None => (progress_cm.map(|_| (a - from) / 100), None),
                Some(p) => (None, Some((a - p) / 100)),
            };
            prev_along = Some(a);
            HorizonEntry {
                maneuver: m.clone(),
                group_id: None,
                from_vehicle_m,
                seg_from_prev_m,
                eta_range_ms: None,
                // All Arrow values are actions; information items (SA/PA, passed IC) are
                // not modelled yet.
                action_required: true,
                avoid_offer: None,
            }
        })
        .collect()
}

/// `progress_cm` reached the destination: the route end, or an `arrive` maneuver.
pub fn arrived(route: &LoadedRoute, progress_cm: u32) -> bool {
    progress_cm >= route.length_cm
        || route
            .maneuvers
            .last()
            .is_some_and(|m| m.arrow == Arrow::Arrive && progress_cm >= m.anchor.along_route_cm)
}

/// Builds and de-duplicates horizon frames.
#[derive(Debug, Clone, Default)]
pub struct HorizonFrames {
    frame_seq: u64,
    /// Last content (frame fields zeroed) and the last tracking `upcoming`.
    last: Option<ManeuverHorizon>,
    frozen: Option<Vec<HorizonEntry>>,
}

impl HorizonFrames {
    pub fn frame_seq(&self) -> u64 {
        self.frame_seq
    }

    /// A new route was committed: forget the content of the old one (frame_seq keeps
    /// growing so frames of different routes never share a number).
    pub fn reset_route(&mut self) {
        self.last = None;
        self.frozen = None;
    }

    /// Returns the frame to emit when the content changed.
    pub fn next(
        &mut self,
        route: &LoadedRoute,
        status: HorizonStatus,
        progress_cm: Option<u32>,
        sampled_at_mono_ms: u64,
        fix_stale_ms: u64,
    ) -> Option<ManeuverHorizon> {
        let (matched_progress_cm, upcoming) = match status {
            HorizonStatus::Tracking => {
                let u = upcoming(route, progress_cm);
                self.frozen = Some(u.clone());
                (progress_cm, u)
            }
            HorizonStatus::Uncertain => (
                None,
                self.frozen.clone().unwrap_or_else(|| upcoming(route, None)),
            ),
            HorizonStatus::OffRoute | HorizonStatus::Rerouting | HorizonStatus::Finished => {
                (None, Vec::new())
            }
        };
        let content = ManeuverHorizon {
            schema_version: 1,
            route_key: route.route_key.clone(),
            frame_seq: 0,
            sampled_at_monotonic_ms: 0,
            valid_until_monotonic_ms: 0,
            status,
            matched_progress_cm,
            upcoming,
            next_p1_deadline_monotonic_ms: None,
            next_complex_window: None,
            refs: None,
        };
        if self.last.as_ref() == Some(&content) {
            return None;
        }
        self.frame_seq += 1;
        self.last = Some(content.clone());
        Some(ManeuverHorizon {
            frame_seq: self.frame_seq,
            sampled_at_monotonic_ms: sampled_at_mono_ms,
            valid_until_monotonic_ms: sampled_at_mono_ms.saturating_add(fix_stale_ms),
            ..content
        })
    }
}
