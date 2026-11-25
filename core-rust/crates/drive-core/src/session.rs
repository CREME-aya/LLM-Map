//! `dc_session` state. Single-threaded: called only from the NavigationSession actor.

use crate::align::{AlignConfig, AlignStats, Aligner, FixResult, Mode};
use crate::command_line::{self, Command, JpRef};
use crate::horizon::{arrived, HorizonFrames};
use crate::planner::{Effects, PlanSearch, SearchLimits, SearchPlanner};
use crate::route::LoadedRoute;
use contracts::horizon::{HorizonStatus, ManeuverHorizon};
use contracts::override_types::{RejectCode, ValidateOverrideRequest, ValidateOverrideResult};
use contracts::plan::{PlanCandidates, TripPlanSnapshot};
use contracts::route::{RouteCommitRequest, RouteCommitResult};
use contracts::tick::{
    DistanceClass, EventBody, Fix, LogEvent, SearchPriority, ThermalStatus, TickInput, TickOutput,
};
use contracts::{ErrorCode, RouteKey};
use serde::Deserialize;

/// `dc_session_new` config. TODO(abi-v1): config contents are not specified anywhere;
/// the provisional search limits (03 §3.1 / 07 §1) and the off-route thresholds (07 §1,
/// 04 §3.1: policy values, not per route) can be overridden, each defaulting to the 07 §1
/// initial value. Unknown fields are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionConfig {
    /// Native watchdog limit from each `start`, short / long distance (5 s / 10 s).
    pub watchdog_short_ms: u64,
    pub watchdog_long_ms: u64,
    /// Comparison budget from the plan start, short / long distance (3 s / 8 s).
    pub comparison_budget_short_ms: u64,
    pub comparison_budget_long_ms: u64,
    /// Lateral tolerance for "on the route" (cm; 3000 = 30 m, provisional).
    pub lateral_tol_cm: u32,
    /// Heading tolerance (degrees; 60, provisional), compared in centidegrees.
    pub heading_tol_deg: u16,
    /// Below this speed the bearing is not used (cm/s; 140 = 1.4 m/s).
    pub heading_min_speed_cm_s: u32,
    /// Consecutive valid off-route fixes to confirm off-route; also the number of
    /// consecutive low-accuracy fixes that make the position uncertain (3, provisional).
    pub confirm_fixes: u32,
    /// Fixes with `acc_cm` above this are not used (default: `lateral_tol_cm`).
    pub max_acc_cm: Option<u32>,
    /// Fix freshness limit (ms; 07 §1: 2.5 s).
    pub fix_stale_ms: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        let l = SearchLimits::default();
        let a = AlignConfig::default();
        Self {
            watchdog_short_ms: l.watchdog_short_ms,
            watchdog_long_ms: l.watchdog_long_ms,
            comparison_budget_short_ms: l.comparison_budget_short_ms,
            comparison_budget_long_ms: l.comparison_budget_long_ms,
            lateral_tol_cm: a.lateral_tol_cm,
            heading_tol_deg: a.heading_tol_deg,
            heading_min_speed_cm_s: a.heading_min_speed_cm_s,
            confirm_fixes: a.confirm_fixes,
            max_acc_cm: None,
            fix_stale_ms: a.fix_stale_ms,
        }
    }
}

impl SessionConfig {
    pub fn align(&self) -> AlignConfig {
        AlignConfig {
            lateral_tol_cm: self.lateral_tol_cm,
            heading_tol_deg: self.heading_tol_deg,
            heading_min_speed_cm_s: self.heading_min_speed_cm_s,
            confirm_fixes: self.confirm_fixes,
            max_acc_cm: self.max_acc_cm.unwrap_or(self.lateral_tol_cm),
            fix_stale_ms: self.fix_stale_ms,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.confirm_fixes == 0 {
            return Err("confirm_fixes must be >= 1".into());
        }
        if self.heading_tol_deg > 180 {
            return Err("heading_tol_deg must be <= 180".into());
        }
        Ok(())
    }

    fn limits(&self) -> SearchLimits {
        SearchLimits {
            watchdog_short_ms: self.watchdog_short_ms,
            watchdog_long_ms: self.watchdog_long_ms,
            comparison_budget_short_ms: self.comparison_budget_short_ms,
            comparison_budget_long_ms: self.comparison_budget_long_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreError {
    pub code: ErrorCode,
    pub msg: String,
}

impl CoreError {
    pub fn new(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
        }
    }
}

/// Thermal state collapsed in Rust: `critical | emergency | shutdown` → one level (#44 item 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalLevel {
    None,
    Light,
    Moderate,
    Severe,
    CriticalOrAbove,
}

impl From<ThermalStatus> for ThermalLevel {
    fn from(s: ThermalStatus) -> Self {
        match s {
            ThermalStatus::None => Self::None,
            ThermalStatus::Light => Self::Light,
            ThermalStatus::Moderate => Self::Moderate,
            ThermalStatus::Severe => Self::Severe,
            ThermalStatus::Critical | ThermalStatus::Emergency | ThermalStatus::Shutdown => {
                Self::CriticalOrAbove
            }
        }
    }
}

#[derive(Debug)]
pub struct Session {
    planner: SearchPlanner,
    last_now_mono_ms: Option<u64>,
    current_route_key: Option<RouteKey>,
    thermal: ThermalLevel,
    align_cfg: AlignConfig,
    /// Geometry of the current route (set by `route_commit`).
    route: Option<LoadedRoute>,
    aligner: Aligner,
    finished: bool,
    frames: HorizonFrames,
    /// `at_mono_ms` of the latest fix used (horizon `sampled_at_monotonic_ms`).
    last_fix_at: Option<u64>,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        let align_cfg = config.align();
        Self {
            planner: SearchPlanner::new(config.limits()),
            last_now_mono_ms: None,
            current_route_key: None,
            thermal: ThermalLevel::None,
            align_cfg,
            route: None,
            aligner: Aligner::new(align_cfg),
            finished: false,
            frames: HorizonFrames::default(),
            last_fix_at: None,
        }
    }

    pub fn from_json(config_json: &[u8]) -> Result<Self, CoreError> {
        let cfg: SessionConfig = serde_json::from_slice(config_json)
            .map_err(|e| CoreError::new(ErrorCode::Schema, format!("config: {e}")))?;
        cfg.validate()
            .map_err(|e| CoreError::new(ErrorCode::Schema, format!("config: {e}")))?;
        Ok(Self::new(cfg))
    }

    pub fn aligner(&self) -> &Aligner {
        &self.aligner
    }

    pub fn align_stats(&self) -> AlignStats {
        self.aligner.stats()
    }

    pub fn route(&self) -> Option<&LoadedRoute> {
        self.route.as_ref()
    }

    pub fn frame_seq(&self) -> u64 {
        self.frames.frame_seq()
    }

    /// `horizon.status` at `now` (`None` without route geometry).
    pub fn horizon_status(&self, now: u64) -> Option<HorizonStatus> {
        self.route.as_ref()?;
        Some(if self.finished {
            HorizonStatus::Finished
        } else if self.aligner.off_confirmed() {
            if self.planner.reroute_in_slot() {
                HorizonStatus::Rerouting
            } else {
                HorizonStatus::OffRoute
            }
        } else if self.aligner.tracking(now) {
            HorizonStatus::Tracking
        } else {
            HorizonStatus::Uncertain
        })
    }

    /// Remaining ETA on the current route at `now` (ms), only while tracking.
    pub fn remaining_eta_ms(&self, now: u64) -> Option<u32> {
        let r = self.route.as_ref()?;
        if self.horizon_status(now) != Some(HorizonStatus::Tracking) {
            return None;
        }
        self.aligner.progress_cm().map(|p| r.remaining_ms(p))
    }

    /// A reroute / ad-hoc explicit_plan result may be committed (03 §3.1 / 04 §3
    /// "status=tracking", read as "the session follows the vehicle with confidence"):
    /// not `uncertain` / `finished`. `off_route` / `rerouting` is the state a reroute was
    /// started for. Without route geometry there is no horizon to gate on.
    fn commit_allowed(&self, now: u64) -> bool {
        !matches!(
            self.horizon_status(now),
            Some(HorizonStatus::Uncertain | HorizonStatus::Finished)
        )
    }

    /// The remaining ETA the planner uses for the distance class of searches outside a
    /// plan: valid only while tracking the current route (otherwise invalidated → long).
    fn refresh_eta(&mut self, now: u64) {
        match (self.remaining_eta_ms(now), &self.current_route_key) {
            (Some(ms), Some(k)) => self.planner.set_route_eta(k.clone(), u64::from(ms) / 1000),
            _ => self.planner.invalidate_route_eta(),
        }
    }

    pub fn planner(&self) -> &SearchPlanner {
        &self.planner
    }

    pub fn thermal(&self) -> ThermalLevel {
        self.thermal
    }

    /// Test hook: replaces the current route key without geometry (stands in for a
    /// `dc_route_commit` in planner tests). Any loaded geometry is dropped.
    #[doc(hidden)]
    pub fn set_current_route_key(&mut self, key: RouteKey) {
        self.current_route_key = Some(key);
        self.route = None;
        self.aligner = Aligner::new(self.align_cfg);
        self.finished = false;
        self.frames.reset_route();
        self.last_fix_at = None;
    }

    /// `dc_route_commit` guard (team decision 2026-11-25): `route_key` is the one from
    /// `commit{req_id, route_key}`, i.e. the current route's key when that search started.
    /// If the current route is no longer that one (replaced after the commit was issued,
    /// or none), the late route is refused with `E_STALE` and nothing is replaced, so it
    /// never reaches the horizon or voice_cmds.
    pub fn check_route_commit(&self, route_key: &RouteKey) -> Result<(), CoreError> {
        self.check_route_guard(Some(route_key))
    }

    /// Same guard with `null` allowed: `None` passes only when no route is committed yet.
    pub fn check_route_guard(&self, route_key: Option<&RouteKey>) -> Result<(), CoreError> {
        if self.current_route_key.as_ref() == route_key {
            Ok(())
        } else {
            Err(CoreError::new(
                ErrorCode::Stale,
                "route_key no longer matches the current route",
            ))
        }
    }

    /// `dc_route_commit` body (04 §3.1): re-checks the guard `route_key` (`E_STALE`, nothing
    /// replaced), validates and indexes the bundle (`E_SCHEMA`), then makes it the current
    /// route. Alignment starts over (status `uncertain` until a fix matches; the first fix
    /// is located through the spatial index). The next `dc_tick` returns the new horizon.
    pub fn route_commit(
        &mut self,
        req: &RouteCommitRequest,
    ) -> Result<RouteCommitResult, CoreError> {
        self.check_route_guard(req.route_key.as_ref())?;
        let loaded = LoadedRoute::load(&req.bundle)
            .map_err(|e| CoreError::new(ErrorCode::Schema, format!("route_bundle: {e}")))?;
        let result = RouteCommitResult {
            route_key: loaded.route_key.clone(),
            legs: req.bundle.legs.len() as u32,
            segments: loaded.segs.len() as u32,
            maneuvers: loaded.maneuvers.len() as u32,
        };
        let now = self.last_now_mono_ms.unwrap_or(0);
        let mut fx = Effects::default();
        self.planner
            .on_route_changed(now, &loaded.route_key, &mut fx);
        self.planner.invalidate_route_eta();
        self.current_route_key = Some(loaded.route_key.clone());
        self.route = Some(loaded);
        self.aligner = Aligner::new(self.align_cfg);
        self.finished = false;
        self.frames.reset_route();
        self.last_fix_at = None;
        Ok(result)
    }

    /// `dc_plan_candidates` body (team decision 2026-11-25): fixes the plan's distance
    /// class (`begin_plan`), builds the ordered `CandidateRequest[]`, queues their searches
    /// and returns the first `start` (one search at a time; the rest start from `dc_tick`
    /// as each `search_finished` arrives). The plan ends with one `rank` in `planner_cmds`.
    /// Candidate searches use `route_key {trip_id, trip_revision, route_revision: 0,
    /// graph_version}` from the snapshot.
    /// Called by `dc_plan_candidates(s, {now_mono_ms, eta_estimate_s?, snapshot})`.
    /// TODO(abi-v1): route_revision 0 for a plan without a committed route is provisional;
    /// `budget.deadline_ms` is not used (its relation to the comparison budget is unspecified).
    pub fn plan_candidates(
        &mut self,
        now_mono_ms: u64,
        eta_estimate_s: Option<u64>,
        snapshot: &TripPlanSnapshot,
    ) -> PlanCandidates {
        let mut fx = Effects::default();
        let mut extra_log = Vec::new();
        let now = self.clamp_now(now_mono_ms, &mut extra_log);
        self.refresh_eta(now);
        let candidates = crate::candidates::candidate_requests(snapshot);
        let route_key = RouteKey {
            trip_id: snapshot.trip_id.clone(),
            trip_revision: snapshot.trip_revision,
            route_revision: contracts::U64Str(0),
            graph_version: snapshot.graph_version.clone(),
        };
        let searches = candidates
            .iter()
            .map(|c| PlanSearch {
                candidate_id: c.candidate_id.clone(),
                priority: c.priority,
            })
            .collect();
        let distance_class = self.planner.begin_plan_with(
            now,
            &snapshot.plan_request_id,
            eta_estimate_s,
            Some(route_key),
            searches,
            &mut fx,
        );
        let out = self.finish(now, fx, extra_log);
        PlanCandidates {
            plan_request_id: snapshot.plan_request_id.clone(),
            distance_class,
            candidates,
            planner_cmds: out.planner_cmds,
            next_wake_mono_ms: out.next_wake_mono_ms,
            log: out.log,
        }
    }

    /// Planner hook: fix the distance class of a plan_request without queueing candidate
    /// searches (replay tests drive the searches themselves). Same id → class kept.
    pub fn begin_plan(
        &mut self,
        now_mono_ms: u64,
        plan_request_id: &str,
        eta_estimate_s: Option<u64>,
    ) -> (DistanceClass, TickOutput) {
        let mut fx = Effects::default();
        let c = self
            .planner
            .begin_plan(now_mono_ms, plan_request_id, eta_estimate_s, &mut fx);
        (c, self.finish(now_mono_ms, fx, Vec::new()))
    }

    /// Internal hook for triggers that are not wired yet (avoid-chip look-ahead, explicit
    /// requests). Effects are returned as a TickOutput fragment so the deterministic replay
    /// tests can drive the planner. Reroutes come from the alignment inside `tick`.
    pub fn request_search(
        &mut self,
        now_mono_ms: u64,
        priority: SearchPriority,
    ) -> Result<TickOutput, CoreError> {
        let key = self
            .current_route_key
            .clone()
            .ok_or_else(|| CoreError::new(ErrorCode::Stale, "no committed route"))?;
        let mut fx = Effects::default();
        self.refresh_eta(now_mono_ms);
        self.planner
            .request(now_mono_ms, priority, key, None, &mut fx);
        Ok(self.finish(now_mono_ms, fx, Vec::new()))
    }

    /// Pure state transition: `TickInput → TickOutput`.
    pub fn tick(&mut self, input: TickInput) -> TickOutput {
        let mut fx = Effects::default();
        let mut extra_log: Vec<LogEvent> = Vec::new();

        // Time never goes backwards inside the core: clamp to the previous value and
        // record clock_regression (02 §2.1 / team decision 2026-11-24).
        let now = self.clamp_now(input.now_mono_ms, &mut extra_log);
        // Status and remaining ETA as of `now`, before this tick's events / fix.
        self.refresh_eta(now);
        let commit_allowed = self.commit_allowed(now);

        let mut prev_at: Option<u64> = None;
        for ev in input.events {
            // Events must be in at_mono_ms ascending order. Not re-sorted: an out-of-order
            // event is clamped to the previous value and clock_regression is recorded.
            let raw_at = ev.at_mono_ms;
            let at = match prev_at {
                Some(p) if raw_at < p => {
                    extra_log.push(log_at(
                        p,
                        "clock_regression",
                        Some(format!("event at_mono_ms {raw_at}<{p}")),
                    ));
                    p
                }
                _ => raw_at,
            };
            prev_at = Some(at);
            // TODO(abi-v1): events later than now_mono_ms are not checked (not specified).
            match ev.body {
                EventBody::SearchFinished {
                    req_id,
                    route_key,
                    outcome,
                    facts_ref,
                    ..
                } => {
                    self.planner.on_search_finished(
                        at,
                        &req_id,
                        &route_key,
                        outcome,
                        facts_ref.as_deref(),
                        self.current_route_key.as_ref(),
                        commit_allowed,
                        &mut fx,
                    );
                }
                EventBody::EngineReset {} => self.planner.on_engine_reset(at, &mut fx),
                EventBody::Thermal { status } => {
                    let lvl = ThermalLevel::from(status);
                    if lvl != self.thermal {
                        extra_log.push(log_at(at, "thermal_changed", Some(format!("{lvl:?}"))));
                        self.thermal = lvl;
                    }
                }
                // TODO(drive-core): voice scheduler (tts_*), ASR/consent (asr_*, override_ready,
                // user_tap), inference tier, audio focus, call state, low memory.
                _ => {}
            }
        }
        if let Some(fix) = input.fix {
            self.on_fix(now, fix, &mut fx, &mut extra_log);
        }
        self.planner.on_now(now, &mut fx);
        self.refresh_eta(now);
        self.finish(now, fx, extra_log)
    }

    /// On-route alignment of one fix (04 §3.1); confirms off-route and requests the reroute
    /// with this fix as the new origin.
    fn on_fix(&mut self, now: u64, fix: Fix, fx: &mut Effects, log: &mut Vec<LogEvent>) {
        let Some(route) = self.route.as_ref() else {
            return;
        };
        if self.finished {
            return;
        }
        let was_tracking = self.aligner.tracking(now);
        let at = fix.at_mono_ms;
        let r = self.aligner.on_fix(route, &fix);
        if r != FixResult::OutOfOrder {
            self.last_fix_at = Some(at);
        }
        if r != FixResult::OutOfOrder && (!was_tracking || !matches!(r, FixResult::OnRoute { .. }))
        {
            self.frames.observe_sample(None);
        }
        match r {
            FixResult::OnRoute { along_cm, mode, .. } => {
                if !was_tracking {
                    let how = if mode == Mode::Grid { "grid" } else { "local" };
                    log.push(log_at(
                        at,
                        "align_acquired",
                        Some(format!("mode={how} along_cm={along_cm}")),
                    ));
                }
                if self
                    .frames
                    .observe_sample(Some(along_cm))
                    .is_some_and(|p| arrived(route, p))
                {
                    self.finished = true;
                    log.push(log_at(at, "route_finished", None));
                }
            }
            FixResult::Ambiguous { parts } => {
                if was_tracking || parts > 1 {
                    log.push(log_at(
                        at,
                        "align_ambiguous",
                        Some(format!("parts={parts}")),
                    ));
                }
            }
            FixResult::Off { run, confirmed_now } => {
                if confirmed_now {
                    log.push(log_at(
                        at,
                        "off_route_confirmed",
                        Some(format!("run={run}")),
                    ));
                }
            }
            FixResult::LowAccuracy { run } => {
                if run == self.align_cfg.confirm_fixes {
                    log.push(log_at(at, "fix_low_accuracy", Some(format!("run={run}"))));
                }
            }
            FixResult::OutOfOrder => log.push(log_at(at, "fix_out_of_order", None)),
        }
        // Off-route invalidates the remaining ETA before the reroute starts (→ long).
        self.refresh_eta(now);
        if self.aligner.off_confirmed() && !self.planner.has_reroute() {
            if let Some(key) = self.current_route_key.clone() {
                self.planner
                    .request(now, SearchPriority::Reroute, key, Some(fix), fx);
            }
        }
    }

    /// Clamp `now_mono_ms` to the previous value (time never goes backwards in the core).
    fn clamp_now(&mut self, raw_now: u64, extra_log: &mut Vec<LogEvent>) -> u64 {
        let now = match self.last_now_mono_ms {
            Some(prev) if raw_now < prev => {
                extra_log.push(log_at(
                    prev,
                    "clock_regression",
                    Some(format!("now_mono_ms {raw_now}<{prev}")),
                ));
                prev
            }
            _ => raw_now,
        };
        self.last_now_mono_ms = Some(now);
        now
    }

    fn finish(&mut self, now: u64, fx: Effects, mut extra_log: Vec<LogEvent>) -> TickOutput {
        let mut log = fx.log;
        log.append(&mut extra_log);
        log.sort_by_key(|l| l.at_mono_ms); // stable: deterministic order
        let horizon = self.next_horizon(now);
        // The freshness limit can turn tracking into uncertain without new input.
        let stale_at = self.route.as_ref().and_then(|_| self.aligner.stale_at(now));
        let next_wake_mono_ms = match (self.planner.next_wake(), stale_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        TickOutput {
            horizon,
            voice_cmds: Vec::new(),
            ui_cmds: fx.ui_cmds,
            planner_cmds: fx.planner_cmds,
            next_wake_mono_ms,
            log,
        }
    }

    /// The horizon frame for `now`, only when its content changed.
    fn next_horizon(&mut self, now: u64) -> Option<ManeuverHorizon> {
        let status = self.horizon_status(now)?;
        let route = self.route.as_ref()?;
        let progress = self.aligner.progress_cm();
        let sampled = self.last_fix_at.unwrap_or(now);
        self.frames.next(
            route,
            status,
            progress,
            sampled,
            self.align_cfg.fix_stale_ms,
        )
    }

    /// `dc_validate_override` skeleton. Implemented: SYNTAX, NO_PENDING_TARGET, UNKNOWN_REF
    /// (no refs available yet). Everything else returns E_UNIMPLEMENTED.
    /// Note: `YES` from the SLM never confirms anything (consent = dict match in Rust or tap).
    pub fn validate_override(
        &mut self,
        req: ValidateOverrideRequest,
    ) -> Result<ValidateOverrideResult, CoreError> {
        let rejected = |code| Ok(ValidateOverrideResult::Rejected { code });
        let cmd = match command_line::parse(&req.code_line) {
            Ok(c) => c,
            Err(_) => return rejected(RejectCode::Syntax),
        };
        match cmd {
            // NO_PENDING_TARGET = YES / NO / CXL without a target (team decision #3).
            // TODO(drive-core): CXL target can also be a change confirmed within the last 5 s,
            // and pending offers should come from session state, not only the request field.
            Command::Yes | Command::No | Command::Cxl if req.pending_offer_id.is_none() => {
                rejected(RejectCode::NoPendingTarget)
            }
            // TODO(drive-core): refs come from ManeuverHorizon.refs (not produced yet) → always unknown.
            Command::Ovr(a) if matches!(a.jp, Some(JpRef::Next | JpRef::Last)) => {
                rejected(RejectCode::UnknownRef)
            }
            _ => Err(CoreError::new(
                ErrorCode::Unimplemented,
                "parsed; contract / Safety Gate / revision checks not implemented",
            )),
        }
    }
}

fn log_at(at: u64, what: &str, detail: Option<String>) -> LogEvent {
    LogEvent {
        at_mono_ms: at,
        what: what.to_owned(),
        req_id: None,
        detail,
    }
}
