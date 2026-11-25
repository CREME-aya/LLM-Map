//! `ManeuverHorizon` generation (docs 04 §2 / §3). Integer-only (see `geo`).
//!
//! - `upcoming`: the next 3 maneuvers in route order that have not passed on two samples.
//!   The first entry carries `from_vehicle_m`, the others `seg_from_prev_m`.
//! - `tracking`: distances follow the progress. `uncertain`: distance updates and new
//!   proposals are suppressed, so the last tracking `upcoming` is kept as is (before any
//!   match: the first 3 maneuvers without `from_vehicle_m`). `off_route` / `rerouting`:
//!   the old guidance is withheld (`upcoming` empty). `finished`: empty.
//! - `frame_seq` grows by one only when the content changes (everything except
//!   `frame_seq`, `sampled_at_monotonic_ms` and `valid_until_monotonic_ms`).
//!
//! Passage requires two consecutive confident projections at or beyond an anchor.
//! TODO(04 §2): the bundle carries a single edge timeline. Until the producer supplies
//! independent bounds, all three ETAs describe that one deterministic projection.
//! TODO(04 §2.1): a stop within a leg has no marker in the bundle.
//! TODO(04 §2, §4.3): general junction kinds and their decision deadlines are unspecified.
//! Only identify documented, evidenced difficult junctions. A known avoidance decision
//! deadline can supply the next reference; P1 speech time is a separate deadline.
//! TODO(04 §2): complex-window start/end rules are unspecified.
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::route::LoadedRoute;
use contracts::horizon::{
    Arrow, EtaRange, EvidenceKind, HorizonEntry, HorizonStatus, JunctionRef, Maneuver,
    ManeuverHorizon, PassedRef, Refs, UPCOMING_MAX,
};

/// The next `UPCOMING_MAX` maneuvers ahead of `progress_cm` (`None`: from the start,
/// without `from_vehicle_m`).
fn upcoming(
    route: &LoadedRoute,
    progress_cm: Option<u32>,
    passed_cm: Option<u32>,
) -> Vec<HorizonEntry> {
    let from = progress_cm.unwrap_or(0);
    let mut prev_along: Option<u32> = None;
    let first = unpassed_index(route, passed_cm);
    let current_time = progress_cm.map(|p| route.time_at(p));
    route
        .maneuvers
        .iter()
        .enumerate()
        .skip(first)
        .take(UPCOMING_MAX)
        .map(|(index, m)| {
            let a = m.anchor.along_route_cm;
            let (from_vehicle_m, seg_from_prev_m) = match prev_along {
                None => (progress_cm.map(|_| a.saturating_sub(from) / 100), None),
                Some(p) => (None, Some((a - p) / 100)),
            };
            prev_along = Some(a);
            HorizonEntry {
                maneuver: m.clone(),
                group_id: route.group_head(index).map(|head| head.id.clone()),
                from_vehicle_m,
                seg_from_prev_m,
                eta_range_ms: current_time.map(|current| {
                    let typical = route.time_at(a).saturating_sub(current);
                    EtaRange {
                        earliest: typical,
                        typical,
                        latest: typical,
                    }
                }),
                // All Arrow values are actions; information items (SA/PA, passed IC) are
                // not modelled yet.
                action_required: true,
                avoid_offer: None,
            }
        })
        .collect()
}

fn unpassed_index(route: &LoadedRoute, passed: Option<u32>) -> usize {
    passed.map_or(0, |p| {
        route
            .maneuvers
            .partition_point(|m| m.anchor.along_route_cm <= p)
    })
}

fn junction_kind(m: &Maneuver) -> Option<&'static str> {
    let confirmed = |e: &contracts::horizon::Evidence, value| {
        e.kind == EvidenceKind::Confirmed
            && e.value.as_ref().and_then(|v| v.as_str()) == Some(value)
    };
    let right = matches!(
        m.arrow,
        Arrow::Right | Arrow::SlightRight | Arrow::SharpRight
    );
    (right
        && confirmed(&m.crossing, "across_oncoming")
        && (confirmed(&m.approach_signal, "absent")
            || confirmed(&m.right_turn_protection, "unprotected")))
    .then_some("right_unprotected")
}

fn ref_id(m: &Maneuver) -> String {
    format!("j{}-{}", m.id.leg_index, m.id.ordinal)
}

fn next_ref(upcoming: &[HorizonEntry]) -> Option<JunctionRef> {
    let (entry, kind) = upcoming
        .iter()
        .find_map(|e| junction_kind(&e.maneuver).map(|k| (e, k)))?;
    Some(JunctionRef {
        ref_: ref_id(&entry.maneuver),
        maneuver_id: entry.maneuver.id.clone(),
        kind: kind.into(),
        deadline_mono_ms: entry.avoid_offer.as_ref()?.decision_deadline_monotonic_ms,
    })
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
    passed_cm: Option<u32>,
    previous_sample: Option<u32>,
    last_junction: Option<PassedRef>,
    /// Last content (frame fields zeroed) and the last tracking `upcoming`.
    last: Option<ManeuverHorizon>,
    frozen: Option<Vec<HorizonEntry>>,
}

impl HorizonFrames {
    pub fn frame_seq(&self) -> u64 {
        self.frame_seq
    }

    pub fn refs(&self) -> Option<&Refs> {
        self.last.as_ref().map(|h| &h.refs)
    }

    pub fn last_ref_expires_at(&self) -> Option<u64> {
        self.refs()?
            .last
            .as_ref()?
            .passed_at_mono_ms
            .checked_add(180_001)
    }

    /// A new route was committed: forget the content of the old one (frame_seq keeps
    /// growing so frames of different routes never share a number).
    pub fn reset_route(&mut self) {
        self.passed_cm = None;
        self.previous_sample = None;
        self.last_junction = None;
        self.last = None;
        self.frozen = None;
    }

    pub fn observe_sample(
        &mut self,
        route: &LoadedRoute,
        at: u64,
        sample: Option<u32>,
    ) -> Option<u32> {
        if let (Some(first), Some(second)) = (self.previous_sample, sample) {
            let confirmed = first.min(second);
            let start = unpassed_index(route, self.passed_cm);
            let end = unpassed_index(route, Some(confirmed)).max(start);
            if let Some(m) = route.maneuvers[start..end]
                .iter()
                .rev()
                .find(|m| junction_kind(m).is_some())
            {
                self.last_junction = Some(PassedRef {
                    ref_: ref_id(m),
                    maneuver_id: m.id.clone(),
                    passed_at_mono_ms: at,
                    passed_along_cm: m.anchor.along_route_cm,
                });
            }
            self.passed_cm = Some(self.passed_cm.map_or(confirmed, |old| old.max(confirmed)));
        }
        self.previous_sample = sample;
        self.passed_cm
    }

    /// Returns the frame to emit when the content changed.
    pub fn next(
        &mut self,
        route: &LoadedRoute,
        status: HorizonStatus,
        progress_cm: Option<u32>,
        sampled_at_mono_ms: u64,
        fix_stale_ms: u64,
        now: u64,
    ) -> Option<ManeuverHorizon> {
        let (matched_progress_cm, upcoming) = match status {
            HorizonStatus::Tracking => {
                let u = upcoming(route, progress_cm, self.passed_cm);
                self.frozen = Some(
                    u.iter()
                        .cloned()
                        .map(|mut entry| {
                            entry.eta_range_ms = None;
                            entry
                        })
                        .collect(),
                );
                (progress_cm, u)
            }
            HorizonStatus::Uncertain => (
                None,
                self.frozen
                    .clone()
                    .unwrap_or_else(|| upcoming(route, None, self.passed_cm)),
            ),
            HorizonStatus::OffRoute | HorizonStatus::Rerouting | HorizonStatus::Finished => {
                (None, Vec::new())
            }
        };
        let tracking = status == HorizonStatus::Tracking;
        let next_p1_deadline_monotonic_ms =
            upcoming.first().filter(|_| tracking).and_then(|entry| {
                let lead = if route.motorway_approach(entry.maneuver.anchor.along_route_cm) {
                    20_000
                } else {
                    8_000
                };
                let arrival =
                    sampled_at_mono_ms.saturating_add(u64::from(entry.eta_range_ms?.typical));
                Some(arrival.saturating_sub(lead))
            });
        let refs = Refs {
            next: if tracking { next_ref(&upcoming) } else { None },
            last: self
                .last_junction
                .as_ref()
                .filter(|r| {
                    matched_progress_cm.is_some_and(|p| p.abs_diff(r.passed_along_cm) <= 150_000)
                        && now.saturating_sub(r.passed_at_mono_ms) <= 180_000
                })
                .cloned(),
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
            next_p1_deadline_monotonic_ms,
            next_complex_window: None,
            refs,
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
