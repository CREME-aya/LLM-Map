//! On-route alignment (docs 04 §3.1): Rust aligns the vehicle on the committed route only
//! (no road network). Integer-only (see `geo`).
//!
//! Per fix:
//! 1. `acc_cm > max_acc_cm` → low-accuracy fix: not used, the off-route run is reset to 0;
//!    `confirm_fixes` of them in a row → uncertain.
//! 2. Tracking (previous match within `fix_stale_ms`) → search only the segments around the
//!    previous progress (`local` window, sized from the elapsed time).
//! 3. Lost (no previous match, stale, ambiguous) or nothing found locally → query the
//!    per-chunk grid (`grid`, re-acquisition, e.g. after a tunnel).
//! 4. A candidate is a segment within `lateral_tol_cm` whose bearing is within
//!    `heading_tol` of the fix bearing (bearing only used at `speed_cm_s >= 140`).
//!    Near chunk boundaries the fix is converted with each chunk whose valid range holds
//!    it; the smallest lateral distance wins (tie: smallest along).
//! 5. On re-acquisition, candidates on distinct parts of the route (gap > `AMBIGUITY_GAP_CM`
//!    along the route) cannot be told apart → ambiguous (uncertain), stays lost.
//! 6. No candidate → off-route run + 1; `confirm_fixes` in a row → off-route confirmed.
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::geo::{bearing_diff_cdeg, lerp_u64, project};
use crate::route::LoadedRoute;
use contracts::tick::Fix;

/// Alignment thresholds (07 §1, provisional; passed through `dc_session_new`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignConfig {
    pub lateral_tol_cm: u32,
    pub heading_tol_deg: u16,
    /// Below this speed the bearing is not used (1.4 m/s).
    pub heading_min_speed_cm_s: u32,
    pub confirm_fixes: u32,
    pub max_acc_cm: u32,
    /// Fix freshness limit (07 §1: 2.5 s); older → uncertain.
    pub fix_stale_ms: u64,
}

impl Default for AlignConfig {
    fn default() -> Self {
        Self {
            lateral_tol_cm: 3_000,
            heading_tol_deg: 60,
            heading_min_speed_cm_s: 140,
            confirm_fixes: 3,
            max_acc_cm: 3_000,
            fix_stale_ms: 2_500,
        }
    }
}

/// Fastest plausible speed for sizing the local window (180 km/h).
pub const MAX_SPEED_CM_S: u64 = 5_000;
/// Local window slack ahead of the speed-based reach (200 m).
pub const LOCAL_AHEAD_SLACK_CM: u64 = 20_000;
/// Local window behind the previous progress (50 m).
pub const LOCAL_BEHIND_CM: u64 = 5_000;
/// Candidates further apart than this along the route are distinct route parts (100 m).
pub const AMBIGUITY_GAP_CM: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Local,
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub seg: u32,
    pub along_cm: u32,
    pub lateral_cm: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixResult {
    OnRoute {
        along_cm: u32,
        lateral_cm: u64,
        mode: Mode,
    },
    Ambiguous {
        parts: usize,
    },
    Off {
        run: u32,
        confirmed_now: bool,
    },
    LowAccuracy {
        run: u32,
    },
    /// Older than the last fix: ignored.
    OutOfOrder,
}

/// Segments examined per mode (07 §2.4: normal running vs re-acquisition).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AlignStats {
    pub local_fixes: u64,
    pub local_segs: u64,
    pub local_segs_max: u64,
    pub grid_fixes: u64,
    pub grid_segs: u64,
    pub grid_segs_max: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Matched {
    seg: u32,
    along_cm: u32,
    at_mono_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Aligner {
    cfg: AlignConfig,
    last: Option<Matched>,
    last_fix_at: Option<u64>,
    off_run: u32,
    low_acc_run: u32,
    off_confirmed: bool,
    ambiguous: bool,
    stats: AlignStats,
}

impl Aligner {
    pub fn new(cfg: AlignConfig) -> Self {
        Self {
            cfg,
            last: None,
            last_fix_at: None,
            off_run: 0,
            low_acc_run: 0,
            off_confirmed: false,
            ambiguous: false,
            stats: AlignStats::default(),
        }
    }

    pub fn config(&self) -> &AlignConfig {
        &self.cfg
    }

    pub fn stats(&self) -> AlignStats {
        self.stats
    }

    /// Consecutive valid fixes outside the route (reset by a match or a low-accuracy fix).
    pub fn off_run(&self) -> u32 {
        self.off_run
    }

    pub fn low_accuracy_run(&self) -> u32 {
        self.low_acc_run
    }

    pub fn off_confirmed(&self) -> bool {
        self.off_confirmed
    }

    /// Progress of the last match (`along_route_cm`), even if stale.
    pub fn progress_cm(&self) -> Option<u32> {
        self.last.map(|m| m.along_cm)
    }

    /// Confidently on the route at `now`: a fresh match, not ambiguous, not after
    /// `confirm_fixes` low-accuracy fixes, not off-route.
    pub fn tracking(&self, now: u64) -> bool {
        !self.off_confirmed
            && !self.ambiguous
            && self.low_acc_run < self.cfg.confirm_fixes
            && self
                .last
                .is_some_and(|m| now.saturating_sub(m.at_mono_ms) <= self.cfg.fix_stale_ms)
    }

    /// Time at which `tracking` turns false without new input (freshness limit).
    pub fn stale_at(&self, now: u64) -> Option<u64> {
        if !self.tracking(now) {
            return None;
        }
        self.last
            .map(|m| m.at_mono_ms.saturating_add(self.cfg.fix_stale_ms + 1))
    }

    fn heading_ok(&self, fix: &Fix, seg_bearing: u16) -> bool {
        match (fix.bearing_cdeg, fix.speed_cm_s) {
            (Some(b), Some(v)) if v >= self.cfg.heading_min_speed_cm_s => {
                u32::from(bearing_diff_cdeg(b, seg_bearing))
                    <= u32::from(self.cfg.heading_tol_deg) * 100
            }
            _ => true,
        }
    }

    /// Evaluates the given segments; returns the candidates within tolerance.
    fn evaluate(
        &self,
        route: &LoadedRoute,
        fix: &Fix,
        segs: impl Iterator<Item = u32>,
        examined: &mut u64,
    ) -> Vec<Candidate> {
        let mut local: Vec<Option<Option<(i64, i64)>>> = vec![None; route.chunks.len()];
        let mut out = Vec::new();
        for si in segs {
            let s = &route.segs[si as usize];
            let ci = s.chunk as usize;
            // Fix in this chunk's plane, only if inside the chunk's valid range.
            let p = *local[ci].get_or_insert_with(|| {
                let c = &route.chunks[ci];
                let p = c.frame.to_local(fix.lat_e7, fix.lon_e7);
                c.contains(p).then_some(p)
            });
            let Some(p) = p else { continue };
            *examined += 1;
            let pr = project(p, s.a, s.b);
            if pr.lateral_cm > u64::from(self.cfg.lateral_tol_cm)
                || !self.heading_ok(fix, s.bearing_cdeg)
            {
                continue;
            }
            let along = lerp_u64(
                u64::from(s.along_a),
                u64::from(s.along_b),
                pr.along_cm,
                pr.seg_len_cm,
            ) as u32;
            out.push(Candidate {
                seg: si,
                along_cm: along,
                lateral_cm: pr.lateral_cm,
            });
        }
        out
    }

    /// Segments around the previous progress.
    fn local_window(&self, route: &LoadedRoute, m: Matched, at: u64) -> std::ops::Range<u32> {
        let dt = at.saturating_sub(m.at_mono_ms);
        let ahead = MAX_SPEED_CM_S * dt / 1000 + LOCAL_AHEAD_SLACK_CM;
        let lo_along = u64::from(m.along_cm).saturating_sub(LOCAL_BEHIND_CM);
        let hi_along = u64::from(m.along_cm) + ahead;
        let mut lo = m.seg as usize;
        while lo > 0 && u64::from(route.segs[lo - 1].along_b) >= lo_along {
            lo -= 1;
        }
        let mut hi = m.seg as usize + 1;
        while hi < route.segs.len() && u64::from(route.segs[hi].along_a) <= hi_along {
            hi += 1;
        }
        lo as u32..hi as u32
    }

    fn grid_candidates(
        &self,
        route: &LoadedRoute,
        fix: &Fix,
        examined: &mut u64,
    ) -> Vec<Candidate> {
        let r = i64::from(self.cfg.lateral_tol_cm);
        let mut ids: Vec<u32> = Vec::new();
        for c in &route.chunks {
            let p = c.frame.to_local(fix.lat_e7, fix.lon_e7);
            if c.contains(p) {
                ids.extend(c.query(p, r));
            }
        }
        ids.sort_unstable();
        ids.dedup();
        self.evaluate(route, fix, ids.into_iter(), examined)
    }

    /// Best candidate: smallest lateral distance, then smallest along.
    fn best(c: &[Candidate]) -> Option<Candidate> {
        c.iter().copied().min_by_key(|c| (c.lateral_cm, c.along_cm))
    }

    /// Number of distinct route parts among the candidates.
    fn parts(c: &[Candidate]) -> usize {
        let mut along: Vec<u32> = c.iter().map(|c| c.along_cm).collect();
        along.sort_unstable();
        1 + along
            .windows(2)
            .filter(|w| w[1] - w[0] > AMBIGUITY_GAP_CM)
            .count()
    }

    pub fn on_fix(&mut self, route: &LoadedRoute, fix: &Fix) -> FixResult {
        if self.last_fix_at.is_some_and(|t| fix.at_mono_ms < t) {
            return FixResult::OutOfOrder;
        }
        self.last_fix_at = Some(fix.at_mono_ms);
        if fix.acc_cm > self.cfg.max_acc_cm {
            // Not evidence either way: the off-route run must be consecutive valid fixes.
            self.off_run = 0;
            self.low_acc_run = self.low_acc_run.saturating_add(1);
            return FixResult::LowAccuracy {
                run: self.low_acc_run,
            };
        }
        self.low_acc_run = 0;
        let fresh = self
            .last
            .filter(|m| fix.at_mono_ms.saturating_sub(m.at_mono_ms) <= self.cfg.fix_stale_ms)
            .filter(|_| !self.ambiguous);
        let mut found: Option<(Candidate, Mode)> = None;
        if let Some(m) = fresh {
            let mut n = 0;
            let w = self.local_window(route, m, fix.at_mono_ms);
            let c = self.evaluate(route, fix, w, &mut n);
            self.stats.local_fixes += 1;
            self.stats.local_segs += n;
            self.stats.local_segs_max = self.stats.local_segs_max.max(n);
            if let Some(b) = Self::best(&c) {
                // Short backward jitter never moves the progress back (04 §3).
                let along = b.along_cm.max(m.along_cm);
                found = Some((
                    Candidate {
                        along_cm: along,
                        ..b
                    },
                    Mode::Local,
                ));
            }
        }
        if found.is_none() {
            let mut n = 0;
            let c = self.grid_candidates(route, fix, &mut n);
            self.stats.grid_fixes += 1;
            self.stats.grid_segs += n;
            self.stats.grid_segs_max = self.stats.grid_segs_max.max(n);
            let parts = Self::parts(&c);
            if !c.is_empty() && parts > 1 {
                self.off_run = 0;
                self.off_confirmed = false;
                self.ambiguous = true;
                self.last = None;
                return FixResult::Ambiguous { parts };
            }
            found = Self::best(&c).map(|b| (b, Mode::Grid));
        }
        match found {
            Some((c, mode)) => {
                self.off_run = 0;
                self.off_confirmed = false;
                self.ambiguous = false;
                self.last = Some(Matched {
                    seg: c.seg,
                    along_cm: c.along_cm,
                    at_mono_ms: fix.at_mono_ms,
                });
                FixResult::OnRoute {
                    along_cm: c.along_cm,
                    lateral_cm: c.lateral_cm,
                    mode,
                }
            }
            None => {
                self.off_run = self.off_run.saturating_add(1);
                let confirmed_now = !self.off_confirmed && self.off_run >= self.cfg.confirm_fixes;
                if confirmed_now {
                    self.off_confirmed = true;
                }
                FixResult::Off {
                    run: self.off_run,
                    confirmed_now,
                }
            }
        }
    }
}
