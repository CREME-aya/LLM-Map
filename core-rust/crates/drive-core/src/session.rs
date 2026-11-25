//! `dc_session` state. Single-threaded: called only from the NavigationSession actor.

use crate::command_line::{self, Command, JpRef};
use crate::planner::{Effects, PlanSearch, SearchLimits, SearchPlanner};
use contracts::override_types::{RejectCode, ValidateOverrideRequest, ValidateOverrideResult};
use contracts::plan::{PlanCandidates, TripPlanSnapshot};
use contracts::tick::{
    DistanceClass, EventBody, LogEvent, SearchPriority, ThermalStatus, TickInput, TickOutput,
};
use contracts::{ErrorCode, RouteKey};
use serde::Deserialize;

/// `dc_session_new` config. TODO(abi-v1): config contents are not specified anywhere;
/// only the provisional search limits (03 §3.1 / 07 §1) can be overridden, each defaulting
/// to the 07 §1 initial value. Unknown fields are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionConfig {
    /// Native watchdog limit from each `start`, short / long distance (5 s / 10 s).
    pub watchdog_short_ms: u64,
    pub watchdog_long_ms: u64,
    /// Comparison budget from the plan start, short / long distance (3 s / 8 s).
    pub comparison_budget_short_ms: u64,
    pub comparison_budget_long_ms: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        let l = SearchLimits::default();
        Self {
            watchdog_short_ms: l.watchdog_short_ms,
            watchdog_long_ms: l.watchdog_long_ms,
            comparison_budget_short_ms: l.comparison_budget_short_ms,
            comparison_budget_long_ms: l.comparison_budget_long_ms,
        }
    }
}

impl SessionConfig {
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
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            planner: SearchPlanner::new(config.limits()),
            last_now_mono_ms: None,
            current_route_key: None,
            thermal: ThermalLevel::None,
        }
    }

    pub fn from_json(config_json: &[u8]) -> Result<Self, CoreError> {
        let cfg: SessionConfig = serde_json::from_slice(config_json)
            .map_err(|e| CoreError::new(ErrorCode::Schema, format!("config: {e}")))?;
        Ok(Self::new(cfg))
    }

    pub fn planner(&self) -> &SearchPlanner {
        &self.planner
    }

    pub fn thermal(&self) -> ThermalLevel {
        self.thermal
    }

    /// TODO(abi-v1): will be set by dc_route_commit (not implemented yet).
    pub fn set_current_route_key(&mut self, key: RouteKey) {
        self.current_route_key = Some(key);
    }

    /// `dc_route_commit` guard (team decision 2026-11-25): `route_key` is the one from
    /// `commit{req_id, route_key}`, i.e. the current route's key when that search started.
    /// If the current route is no longer that one (replaced after the commit was issued,
    /// or none), the late route is refused with `E_STALE` and nothing is replaced, so it
    /// never reaches the horizon or voice_cmds.
    /// TODO(abi-v1): loading the route body (operations normalization) is not implemented;
    /// `dc_route_commit` returns E_UNIMPLEMENTED after this check passes.
    pub fn check_route_commit(&self, route_key: &RouteKey) -> Result<(), CoreError> {
        match &self.current_route_key {
            Some(k) if k == route_key => Ok(()),
            _ => Err(CoreError::new(
                ErrorCode::Stale,
                "route_key no longer matches the current route",
            )),
        }
    }

    /// Remaining ETA on the current route (`None` = unknown / invalidated, e.g. off-route).
    /// Decides the distance class of searches outside a plan when they start (planner docs).
    /// Ignored without a committed route; a later route change makes it stale by route_key.
    /// TODO(drive-core): set from map-matching / the committed route's ETA instead.
    pub fn set_remaining_eta(&mut self, remaining_eta_s: Option<u64>) {
        match (&self.current_route_key, remaining_eta_s) {
            (Some(k), Some(s)) => self.planner.set_route_eta(k.clone(), s),
            _ => self.planner.invalidate_route_eta(),
        }
    }

    /// `dc_plan_candidates` body (team decision 2026-11-25): fixes the plan's distance
    /// class (`begin_plan`), builds the ordered `CandidateRequest[]`, queues their searches
    /// and returns the first `start` (one search at a time; the rest start from `dc_tick`
    /// as each `search_finished` arrives). The plan ends with one `rank` in `planner_cmds`.
    /// Candidate searches use `route_key {trip_id, trip_revision, route_revision: 0,
    /// graph_version}` from the snapshot.
    /// TODO(abi-v1): dc_plan_candidates(dc_in) takes no session today; the ABI change to
    /// pass the session (decided, separate work before Gate 0) will just call this.
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
        let out = self.finish(fx, extra_log);
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
        (c, self.finish(fx, Vec::new()))
    }

    /// Internal hook for triggers that are not wired yet (off-route detection from `fix`,
    /// avoid-chip look-ahead). Effects are returned as a TickOutput fragment so the
    /// deterministic replay tests can drive the planner.
    /// TODO(drive-core): call this from map-matching inside `tick` instead.
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
        self.planner.request(now_mono_ms, priority, key, &mut fx);
        Ok(self.finish(fx, Vec::new()))
    }

    /// Pure state transition: `TickInput → TickOutput`.
    pub fn tick(&mut self, input: TickInput) -> TickOutput {
        let mut fx = Effects::default();
        let mut extra_log: Vec<LogEvent> = Vec::new();

        // Time never goes backwards inside the core: clamp to the previous value and
        // record clock_regression (02 §2.1 / team decision 2026-11-24).
        let now = self.clamp_now(input.now_mono_ms, &mut extra_log);

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
        self.planner.on_now(now, &mut fx);
        self.finish(fx, extra_log)
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

    fn finish(&self, fx: Effects, mut extra_log: Vec<LogEvent>) -> TickOutput {
        let mut log = fx.log;
        log.append(&mut extra_log);
        log.sort_by_key(|l| l.at_mono_ms); // stable: deterministic order
        TickOutput {
            horizon: None, // TODO(drive-core): ManeuverHorizon normalization
            voice_cmds: Vec::new(),
            ui_cmds: fx.ui_cmds,
            planner_cmds: fx.planner_cmds,
            next_wake_mono_ms: self.planner.next_wake(),
            log,
        }
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
