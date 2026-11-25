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
//!
//! `eta_range_ms` (04 §2.2, provisional): durations from the current position. `typical`
//! is the remaining time to the maneuver (difference of `edges.cum_time_ms` between the
//! anchor and the matched position); `earliest = typical × 0.85` (rounded down) and
//! `latest = typical × 1.25` (rounded up), in integer per-mille (`EtaFactors`, set through
//! `SessionConfig`; 0.85 / 1.25 have no basis yet, Gate 2 / 3 calibrate them). The factors
//! apply to the remaining time, never to the time from the route start. `earliest` decides
//! deadlines and window starts (avoid-chip deadline and start condition,
//! `next_complex_window.start`), `latest` the window end; `typical` is for the emphasis
//! times and distances shown.
//! TODO(04 §2.1): a stop within a leg has no marker in the bundle.
//! Junction kinds (04 §2 `JunctionRef.kind`): `refs` point at difficult junctions only
//! (03 §4.1 `jp=next` / `jp=last` = 難所). The class comes from `junction::classify`
//! (names of `CandidateFacts.counts`); only `right_unprotected` becomes a `JunctionRef` /
//! `PassedRef` for now. Other maneuvers' only deadline is the P1 guidance deadline
//! (`next_p1_deadline_monotonic_ms`, 04 §3), which every maneuver anchor has.
//! TODO(04 §2 / §4.3, schema-v1): whether the other avoid-chip classes
//! (`right_signal_unknown_protection` / `right_signal_unknown` / `narrow`) are 難所 for
//! `jp=next` / `jp=last`; and a `JunctionRef` deadline without an `AvoidOffer` (only the
//! avoid-chip decision deadline, 20 s before the branch, is documented).
//!
//! `next_complex_window` (team decision 2026-11-26; use: 05 §6.2): only while tracking.
//! Its first maneuver is the first unpassed maneuver that is a complex trigger
//! (`junction::is_complex_trigger`: crossing oncoming, merge, narrow entry) or belongs to a
//! `group_id` of two or more maneuvers. `start` = that maneuver's `eta_range.earliest` −
//! preparation lead (04 §4.1: 30 s, 60 s when the edge approaching its anchor has the
//! motorway flag), `end` = `eta_range.latest` of the last maneuver of its group (itself
//! when ungrouped) + 5 s. A group is one window. Once its last maneuver is passed the
//! window stays until `end`, then the next one follows.
//! TODO(04 §2 / 05 §6.2): windows of consecutive triggers in different groups are not
//! merged; a roundabout's window ends at its entrance anchor's ETA, not at the exit.
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::junction::{classify, is_complex_trigger, JunctionClass};
use crate::route::LoadedRoute;
use contracts::horizon::{
    Arrow, AvoidOffer, ComplexWindow, EtaRange, HorizonEntry, HorizonStatus, JunctionRef, Maneuver,
    ManeuverHorizon, ManeuverId, PassedRef, Refs, UPCOMING_MAX,
};

/// Preparation lead before a complex window (04 §4.1 "準備" emphasis: 30 s on ordinary
/// roads, 60 s on motorways).
pub const COMPLEX_PREP_CITY_MS: u64 = 30_000;
pub const COMPLEX_PREP_MOTORWAY_MS: u64 = 60_000;
/// Margin after the last maneuver of a complex window (team decision 2026-11-26).
pub const COMPLEX_END_MARGIN_MS: u64 = 5_000;
/// Safety margin of the P3 available time (05 §6.2: 5 s).
pub const P3_SAFETY_MARGIN_MS: u64 = 5_000;

/// Per-mille factors of `eta_range_ms` (04 §2.2, provisional: 850 / 1250).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EtaFactors {
    /// `earliest = typical × earliest_permille / 1000`, rounded down (<= 1000).
    pub earliest_permille: u32,
    /// `latest = typical × latest_permille / 1000`, rounded up (>= 1000).
    pub latest_permille: u32,
}

impl Default for EtaFactors {
    fn default() -> Self {
        Self {
            earliest_permille: 850,
            latest_permille: 1_250,
        }
    }
}

impl EtaFactors {
    /// The range around a remaining time `typical` (ms).
    pub fn range(&self, typical: u32) -> EtaRange {
        let t = u64::from(typical);
        let earliest = t * u64::from(self.earliest_permille) / 1_000;
        let latest = (t * u64::from(self.latest_permille)).div_ceil(1_000);
        EtaRange {
            earliest: u32::try_from(earliest).unwrap_or(u32::MAX),
            typical,
            latest: u32::try_from(latest).unwrap_or(u32::MAX),
        }
    }
}

/// ETA range from the current position (`current` = route time at the progress) to
/// `along_cm` (see module docs).
pub(crate) fn eta_range(
    route: &LoadedRoute,
    along_cm: u32,
    current: u32,
    factors: EtaFactors,
) -> EtaRange {
    factors.range(route.time_at(along_cm).saturating_sub(current))
}

/// The next complex window ahead of `progress_cm` (see module docs) and the index of its
/// last maneuver.
fn complex_window(
    route: &LoadedRoute,
    passed_cm: Option<u32>,
    progress_cm: u32,
    sampled_at_mono_ms: u64,
    factors: EtaFactors,
) -> Option<(ComplexWindow, usize)> {
    let first = unpassed_index(route, passed_cm);
    let current = route.time_at(progress_cm);
    let i = (first..route.maneuvers.len())
        .find(|&i| route.grouped(i) || is_complex_trigger(&route.maneuvers[i]))?;
    let last = route.group_last(i);
    let along = |j: usize| route.maneuvers[j].anchor.along_route_cm;
    let prep = if route.motorway_approach(along(i)) {
        COMPLEX_PREP_MOTORWAY_MS
    } else {
        COMPLEX_PREP_CITY_MS
    };
    let start = sampled_at_mono_ms
        .saturating_add(u64::from(
            eta_range(route, along(i), current, factors).earliest,
        ))
        .saturating_sub(prep);
    let end = sampled_at_mono_ms
        .saturating_add(u64::from(
            eta_range(route, along(last), current, factors).latest,
        ))
        .saturating_add(COMPLEX_END_MARGIN_MS);
    Some((
        ComplexWindow {
            start_monotonic_ms: start,
            end_monotonic_ms: Some(end),
        },
        last,
    ))
}

/// P3 (history commentary) available time at `now` (05 §6.2, the canonical formula):
/// `min(next P1 deadline, complex window start) − estimated TTS preparation − 5 s`, as a
/// duration from `now` (ms, 0 when nothing is left; inside a window it is 0). `None` when
/// neither bound exists. `tts_prep_ms` is an input (provisional: no estimate is specified).
pub fn p3_available_ms(
    now: u64,
    next_p1_deadline_mono_ms: Option<u64>,
    window: Option<&ComplexWindow>,
    tts_prep_ms: u64,
) -> Option<u64> {
    let bound = [
        next_p1_deadline_mono_ms,
        window.map(|w| w.start_monotonic_ms),
    ]
    .into_iter()
    .flatten()
    .min()?;
    Some(
        bound
            .saturating_sub(now)
            .saturating_sub(tts_prep_ms)
            .saturating_sub(P3_SAFETY_MARGIN_MS),
    )
}

/// `p3_available_ms` from a horizon frame.
pub fn p3_available_in(h: &ManeuverHorizon, now: u64, tts_prep_ms: u64) -> Option<u64> {
    p3_available_ms(
        now,
        h.next_p1_deadline_monotonic_ms,
        h.next_complex_window.as_ref(),
        tts_prep_ms,
    )
}

/// The next `UPCOMING_MAX` maneuvers ahead of `progress_cm` (`None`: from the start,
/// without `from_vehicle_m`).
fn upcoming(
    route: &LoadedRoute,
    progress_cm: Option<u32>,
    passed_cm: Option<u32>,
    factors: EtaFactors,
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
                eta_range_ms: current_time.map(|current| eta_range(route, a, current, factors)),
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

/// The `JunctionRef` / `PassedRef` kind of `m` (see module docs: `right_unprotected` only).
fn junction_kind(m: &Maneuver) -> Option<JunctionClass> {
    let c = JunctionClass::RightUnprotected;
    classify(m).contains(&c).then_some(c)
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
        kind,
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
    /// The complex window last emitted and the index of its last maneuver (kept after that
    /// maneuver is passed until its `end`).
    window: Option<(ComplexWindow, usize)>,
    /// `window` is the kept one (its last maneuver is passed).
    window_kept: bool,
    /// Last content (frame fields zeroed) and the last tracking `upcoming`.
    last: Option<ManeuverHorizon>,
    frozen: Option<Vec<HorizonEntry>>,
    eta: EtaFactors,
}

impl HorizonFrames {
    pub fn new(eta: EtaFactors) -> Self {
        Self {
            eta,
            ..Self::default()
        }
    }

    pub fn eta_factors(&self) -> EtaFactors {
        self.eta
    }

    pub fn frame_seq(&self) -> u64 {
        self.frame_seq
    }

    pub fn refs(&self) -> Option<&Refs> {
        self.last.as_ref().map(|h| &h.refs)
    }

    /// When a complex window kept after its last maneuver ends (the frame changes then).
    pub fn window_expires_at(&self) -> Option<u64> {
        if !self.window_kept {
            return None;
        }
        self.window.as_ref()?.0.end_monotonic_ms?.checked_add(1)
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
        self.window = None;
        self.window_kept = false;
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

    /// The window to emit while tracking: the one whose last maneuver was just passed until
    /// its end, otherwise the next one ahead.
    fn next_window(
        &mut self,
        route: &LoadedRoute,
        progress_cm: u32,
        sampled_at_mono_ms: u64,
        now: u64,
    ) -> Option<ComplexWindow> {
        let first = unpassed_index(route, self.passed_cm);
        if let Some((w, last)) = self.window {
            if last < first && w.end_monotonic_ms.is_some_and(|end| now <= end) {
                self.window_kept = true;
                return Some(w);
            }
        }
        self.window_kept = false;
        self.window = complex_window(
            route,
            self.passed_cm,
            progress_cm,
            sampled_at_mono_ms,
            self.eta,
        );
        self.window.map(|(w, _)| w)
    }

    /// Index of the first maneuver not passed yet.
    pub(crate) fn first_unpassed(&self, route: &LoadedRoute) -> usize {
        unpassed_index(route, self.passed_cm)
    }

    /// Returns the frame to emit when the content changed. `offer` is the avoid chip and
    /// the maneuver it belongs to: carried on that `HorizonEntry` while tracking only
    /// (`uncertain` suppresses proposals, so the frozen entries never carry it).
    #[allow(clippy::too_many_arguments)]
    pub fn next(
        &mut self,
        route: &LoadedRoute,
        status: HorizonStatus,
        progress_cm: Option<u32>,
        sampled_at_mono_ms: u64,
        fix_stale_ms: u64,
        now: u64,
        offer: Option<(&ManeuverId, &AvoidOffer)>,
    ) -> Option<ManeuverHorizon> {
        let (matched_progress_cm, upcoming) = match status {
            HorizonStatus::Tracking => {
                let mut u = upcoming(route, progress_cm, self.passed_cm, self.eta);
                self.frozen = Some(
                    u.iter()
                        .cloned()
                        .map(|mut entry| {
                            entry.eta_range_ms = None;
                            entry
                        })
                        .collect(),
                );
                if let Some((id, o)) = offer {
                    for e in u.iter_mut().filter(|e| e.maneuver.id == *id) {
                        e.avoid_offer = Some(o.clone());
                    }
                }
                (progress_cm, u)
            }
            HorizonStatus::Uncertain => (
                None,
                self.frozen
                    .clone()
                    .unwrap_or_else(|| upcoming(route, None, self.passed_cm, self.eta)),
            ),
            HorizonStatus::OffRoute | HorizonStatus::Rerouting | HorizonStatus::Finished => {
                (None, Vec::new())
            }
        };
        let tracking = status == HorizonStatus::Tracking;
        let next_p1_deadline_monotonic_ms = matched_progress_cm
            .and_then(|p| route.next_p1_deadline(self.passed_cm, p, sampled_at_mono_ms));
        let next_complex_window = match matched_progress_cm {
            Some(p) if tracking => self.next_window(route, p, sampled_at_mono_ms, now),
            _ => {
                self.window = None;
                self.window_kept = false;
                None
            }
        };
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
            next_complex_window,
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
