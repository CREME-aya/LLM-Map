//! `dc_session` state. Single-threaded: called only from the NavigationSession actor.

use crate::align::{AlignConfig, AlignStats, Aligner, FixResult, Mode};
use crate::avoid::{
    self, AvoidConfig, AvoidState, AvoidTarget, Consent, DetourPolicy, UndoReturn, UndoState,
    UndoWindow,
};
use crate::command_line::{self, Command, JpRef};
use crate::history::{self, HistoryConfig, HistoryScheduler, PoiState, StepContext};
use crate::horizon::{arrived, eta_range, EtaFactors, HorizonFrames};
use crate::planner::{
    CommitGate, Effects, PlanCommitState, PlanSearch, SearchLimits, SearchPlanner,
};
use crate::route::LoadedRoute;
use contracts::facts::{AvoidDeclineReason, AvoidOfferEvalRequest, AvoidOfferEvalResult};
use contracts::horizon::{HorizonStatus, ManeuverHorizon, ManeuverId, UPCOMING_MAX};
use contracts::override_types::{RejectCode, ValidateOverrideRequest, ValidateOverrideResult};
use contracts::plan::{PlanCandidates, TripPlanSnapshot};
use contracts::route::{RouteCommitRequest, RouteCommitResult};
use contracts::tick::{
    DismissReason, DistanceClass, EventBody, Fix, LogEvent, PlannerCmd, SaveForLaterReason,
    SearchPriority, TapAction, TextClass, ThermalStatus, TickInput, TickOutput, UiCmd, VoiceCmd,
    VoicePriority,
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
    /// How long a reroute / explicit-plan ok returned while `uncertain` is held for a
    /// later commit (ms; 10 s, provisional: 04 §3 leaves it to Gate 7).
    pub commit_hold_ms: u64,
    /// `eta_range_ms` factors in per-mille of the remaining time (04 §2.2, provisional:
    /// earliest 850 = × 0.85, latest 1250 = × 1.25; no basis yet, Gate 2 / 3 calibrate).
    pub eta_earliest_permille: u32,
    pub eta_latest_permille: u32,
    /// Driving-time avoid-chip look-ahead (04 §4.3 item 6 / 03 §3.1: off by default until
    /// the stop latency is measured).
    pub avoid_chip_lookahead: bool,
    /// Avoid-chip compute budget (ms; 07 §1: 1.5 s, provisional).
    pub avoid_budget_ms: u64,
    /// Avoid-chip decision deadline before the branch (ms; 07 §1: 20 s), from `earliest`.
    pub avoid_deadline_before_ms: u64,
    /// "到達約 120 秒前" avoid-chip trigger (ms; 04 §4.3), compared with `earliest`.
    pub avoid_trigger_eta_ms: u64,
    /// "戻して" window after consent (ms; 04 §4.3 item 3: 5 s).
    pub avoid_undo_window_ms: u64,
    /// Soft-mode / drive-start speed threshold (cm/s; 07 §1: 8 km/h = 222 cm/s, provisional).
    pub drive_start_speed_cm_s: u32,
    /// Continuous duration at `>= drive_start_speed_cm_s` before drive-start closes the plan
    /// commit window (ms; 07 §1: 3 s = 3000 ms, provisional).
    pub drive_start_hold_ms: u64,
    /// P3 history commentary corridor far bound ahead of POI (cm; 05 §6.2: 2 km, provisional).
    pub p3_corridor_far_cm: u32,
    /// P3 history commentary corridor near bound ahead of POI (cm; 05 §6.2: 0.5 km, provisional).
    pub p3_corridor_near_cm: u32,
    /// P3 max duration in `Pending` before expiring (ms; 05 §6.2: 30 s, provisional).
    pub p3_pending_timeout_ms: u64,
    /// P3 minimum interval between utterance starts (ms; 05 §6.2: 5 min, provisional).
    pub p3_min_interval_ms: u64,
    /// Estimated TTS preparation time for P3 available window (ms; 05 §6.2: 800 ms, provisional).
    pub p3_tts_prep_ms: u64,
    /// Safety margin before P1 deadline / complex window (ms; 05 §6.2: 5 s, provisional).
    pub p3_safety_margin_ms: u64,
    /// Window after `Completed` / `Interrupted` for L3 `BMK` (ms; 05 §6.2: 60 s, provisional).
    pub p3_bmk_window_ms: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        let l = SearchLimits::default();
        let a = AlignConfig::default();
        let e = EtaFactors::default();
        let v = AvoidConfig::default();
        let h = HistoryConfig::default();
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
            commit_hold_ms: l.commit_hold_ms,
            eta_earliest_permille: e.earliest_permille,
            eta_latest_permille: e.latest_permille,
            avoid_chip_lookahead: v.enabled,
            avoid_budget_ms: v.budget_ms,
            avoid_deadline_before_ms: v.deadline_before_ms,
            avoid_trigger_eta_ms: v.trigger_eta_ms,
            avoid_undo_window_ms: v.undo_window_ms,
            drive_start_speed_cm_s: 222,
            drive_start_hold_ms: 3_000,
            p3_corridor_far_cm: h.corridor_far_cm,
            p3_corridor_near_cm: h.corridor_near_cm,
            p3_pending_timeout_ms: h.pending_timeout_ms,
            p3_min_interval_ms: h.min_interval_ms,
            p3_tts_prep_ms: h.tts_prep_ms,
            p3_safety_margin_ms: h.safety_margin_ms,
            p3_bmk_window_ms: h.bmk_window_ms,
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
        if self.eta_earliest_permille > 1_000 || self.eta_latest_permille < 1_000 {
            return Err("eta factors need earliest_permille <= 1000 <= latest_permille".into());
        }
        if self.p3_corridor_far_cm < self.p3_corridor_near_cm {
            return Err("p3_corridor_far_cm must be >= p3_corridor_near_cm".into());
        }
        Ok(())
    }

    pub fn history(&self) -> HistoryConfig {
        HistoryConfig {
            corridor_far_cm: self.p3_corridor_far_cm,
            corridor_near_cm: self.p3_corridor_near_cm,
            pending_timeout_ms: self.p3_pending_timeout_ms,
            min_interval_ms: self.p3_min_interval_ms,
            tts_prep_ms: self.p3_tts_prep_ms,
            safety_margin_ms: self.p3_safety_margin_ms,
            bmk_window_ms: self.p3_bmk_window_ms,
        }
    }

    pub fn eta_factors(&self) -> EtaFactors {
        EtaFactors {
            earliest_permille: self.eta_earliest_permille,
            latest_permille: self.eta_latest_permille,
        }
    }

    pub fn avoid(&self) -> AvoidConfig {
        AvoidConfig {
            enabled: self.avoid_chip_lookahead,
            budget_ms: self.avoid_budget_ms,
            deadline_before_ms: self.avoid_deadline_before_ms,
            trigger_eta_ms: self.avoid_trigger_eta_ms,
            undo_window_ms: self.avoid_undo_window_ms,
        }
    }

    fn limits(&self) -> SearchLimits {
        SearchLimits {
            watchdog_short_ms: self.watchdog_short_ms,
            watchdog_long_ms: self.watchdog_long_ms,
            comparison_budget_short_ms: self.comparison_budget_short_ms,
            comparison_budget_long_ms: self.comparison_budget_long_ms,
            commit_hold_ms: self.commit_hold_ms,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommitSource {
    /// Rust emitted `planner_cmds: commit{req_id, route_key}` (reroute / avoid_chip / undo /
    /// ad-hoc explicit_plan).
    Issued,
    /// User selected an `ok` candidate of the current plan (`Plan.ok_searches`).
    PlanCandidate {
        trip_id: String,
        trip_revision: contracts::U64Str,
        graph_version: String,
    },
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
    avoid_cfg: AvoidConfig,
    detour: DetourPolicy,
    avoid: AvoidState,
    /// Maneuvers of the current route an avoid chip was tried for (once each).
    avoid_tried: Vec<ManeuverId>,
    /// "戻して" state of the last consent (03 §3.1).
    undo: UndoState,
    /// UiCmds raised outside `dc_tick` (a route commit removing an offer), sent with the
    /// next tick.
    pending_ui: Vec<UiCmd>,
    /// Voice / planner commands raised outside `dc_tick` (`CXL` through
    /// `dc_validate_override`), sent with the next tick.
    pending_voice: Vec<VoiceCmd>,
    pending_planner: Vec<PlannerCmd>,
    pending_log: Vec<LogEvent>,
    /// `commit{req_id, route_key}` Rust emitted and `dc_route_commit` has not settled yet
    /// (provisional req_id check, proposed to the team). Cleared when a route is loaded:
    /// every one of them was issued against the route that was current until then.
    issued_commits: Vec<(String, RouteKey)>,
    /// The undo `commit` issued, its route not loaded yet (#22).
    undo_return: Option<UndoReturn>,
    /// Soft-mode / drive-start speed threshold (cm/s; 07 §1: 222 = 8 km/h, provisional).
    drive_start_speed_cm_s: u32,
    /// Continuous duration at `>= drive_start_speed_cm_s` to trigger drive-start (ms; 3000).
    drive_start_hold_ms: u64,
    /// `fix.at_mono_ms` when the current unbroken run of valid fast fixes started.
    drive_fast_since_mono_ms: Option<u64>,
    /// Latest `fix.at_mono_ms` seen by drive-start tracking (out-of-order fixes ignored).
    drive_last_fix_at_mono_ms: Option<u64>,
    /// P3 history commentary scheduler (05 §6.2).
    history: HistoryScheduler,
    /// Latest on-route fix satisfied `lateral_cm <= lateral_tol_cm` (reset on off/low-acc/route change).
    last_fix_lateral_ok: bool,
    /// Latest on-route fix had `acc_cm <= max_acc_cm` and usable bearing (speed >= heading_min_speed_cm_s).
    last_fix_acc_and_bearing_ok: bool,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        let align_cfg = config.align();
        let history_cfg = config.history();
        Self {
            planner: SearchPlanner::new(config.limits()),
            last_now_mono_ms: None,
            current_route_key: None,
            thermal: ThermalLevel::None,
            align_cfg,
            route: None,
            aligner: Aligner::new(align_cfg),
            finished: false,
            frames: HorizonFrames::new(config.eta_factors()),
            last_fix_at: None,
            avoid_cfg: config.avoid(),
            detour: DetourPolicy::default(),
            avoid: AvoidState::Idle,
            avoid_tried: Vec::new(),
            undo: UndoState::None,
            pending_ui: Vec::new(),
            pending_voice: Vec::new(),
            pending_planner: Vec::new(),
            pending_log: Vec::new(),
            issued_commits: Vec::new(),
            undo_return: None,
            drive_start_speed_cm_s: config.drive_start_speed_cm_s,
            drive_start_hold_ms: config.drive_start_hold_ms,
            drive_fast_since_mono_ms: None,
            drive_last_fix_at_mono_ms: None,
            history: HistoryScheduler::new(history_cfg),
            last_fix_lateral_ok: false,
            last_fix_acc_and_bearing_ok: false,
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

    /// Reroute and ad-hoc explicit plan results may commit while tracking, off route,
    /// or rerouting (03 §3.1 / 04 §3). Without geometry there is no horizon to gate on.
    /// `uncertain` holds a result (planner `release_held`), `finished` drops it.
    fn commit_gate(&self, now: u64) -> CommitGate {
        match self.horizon_status(now) {
            Some(HorizonStatus::Uncertain) => CommitGate::Hold,
            Some(HorizonStatus::Finished) => CommitGate::Denied,
            _ => CommitGate::Allowed,
        }
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

    pub fn history_consent(&self) -> bool {
        self.history.consent()
    }

    pub fn history_poi_state(&self, poi_id: &str) -> Option<&PoiState> {
        self.history.poi_state(poi_id)
    }

    pub fn history_has_spoken(&self, poi_id: &str) -> bool {
        self.history.has_spoken(poi_id)
    }

    pub fn history_spoken_count(&self) -> usize {
        self.history.spoken_count()
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
        self.last_fix_lateral_ok = false;
        self.last_fix_acc_and_bearing_ok = false;
        self.reset_avoid();
        self.undo = UndoState::None;
        self.issued_commits.clear();
        self.undo_return = None;
        let now = self.last_now_mono_ms.unwrap_or(0);
        self.history.on_route_changed(
            now,
            &[],
            &mut self.pending_voice,
            &mut self.pending_ui,
            &mut self.pending_log,
        );
    }

    /// Test hook: records `commit{req_id, route_key}` as if a `dc_tick` had emitted it, so a
    /// test can replace the current route through `route_commit` without driving a search.
    #[doc(hidden)]
    pub fn issue_commit_for_test(&mut self, req_id: &str, route_key: RouteKey) {
        self.issued_commits.push((req_id.to_owned(), route_key));
    }

    /// Test hook: records an `ok` plan candidate on the current plan (or a synthetic ranked
    /// plan) so non-plan tests can load a first route through `route_commit` without driving
    /// a full `plan_candidates` cycle or advancing `next_req_seq`.
    #[doc(hidden)]
    pub fn record_plan_ok_for_test(&mut self, req_id: &str, route_key: RouteKey) {
        self.planner.record_plan_ok_for_test(req_id, route_key);
    }

    /// `commit` req_ids Rust emitted that no `dc_route_commit` has settled yet.
    pub fn issued_commits(&self) -> impl Iterator<Item = &str> {
        self.issued_commits.iter().map(|(r, _)| r.as_str())
    }

    /// The avoid chip state (04 §4.3).
    pub fn avoid_state(&self) -> &AvoidState {
        &self.avoid
    }

    /// The "戻して" state of the last consent (03 §3.1).
    pub fn undo_state(&self) -> &UndoState {
        &self.undo
    }

    /// A new route: the chip of the old one is gone (`dismiss{route_changed}` with the next
    /// tick when it was shown) and every maneuver may be tried again.
    fn reset_avoid(&mut self) {
        if let Some((_, offer)) = self.avoid.offer() {
            self.pending_ui.push(UiCmd::Dismiss {
                target_id: offer.offer_id.clone(),
                reason: DismissReason::RouteChanged,
            });
        }
        self.avoid = AvoidState::Idle;
        self.avoid_tried.clear();
    }

    /// `dc_route_commit` guard (team decision 2026-11-25): `route_key` is the one from
    /// `commit{req_id, route_key}`, i.e. the current route's key when that search started.
    /// If the current route is no longer that one (replaced after the commit was issued,
    /// or none), the late route is refused with `E_STALE` and nothing is replaced, so it
    /// never reaches the horizon or voice_cmds.
    /// Only the `route_key` half of the guard (see [`Self::check_route_guard`] for the
    /// `req_id` half).
    pub fn check_route_commit(&self, route_key: &RouteKey) -> Result<(), CoreError> {
        self.check_route_key(Some(route_key))
    }

    fn check_route_key(&self, route_key: Option<&RouteKey>) -> Result<(), CoreError> {
        if self.current_route_key.as_ref() == route_key {
            Ok(())
        } else {
            Err(CoreError::new(
                ErrorCode::Stale,
                "route_key no longer matches the current route",
            ))
        }
    }

    fn classify_commit_guard(
        &self,
        req_id: &str,
        route_key: Option<&RouteKey>,
    ) -> Result<CommitSource, CoreError> {
        self.check_route_key(route_key)?;
        if let Some(k) = route_key {
            if self
                .issued_commits
                .iter()
                .any(|(r, rk)| r == req_id && rk == k)
            {
                return Ok(CommitSource::Issued);
            }
        }
        if let Some(plan) = self.planner.plan() {
            if plan.ok_searches.iter().any(|s| s.req_id == req_id) {
                return match plan.commit_state {
                    PlanCommitState::Open => {
                        let Some(plan_rk) = plan.route_key.as_ref() else {
                            return Err(CoreError::new(
                                ErrorCode::Stale,
                                "plan has no route_key for candidate commit",
                            ));
                        };
                        Ok(CommitSource::PlanCandidate {
                            trip_id: plan_rk.trip_id.clone(),
                            trip_revision: plan.trip_revision.unwrap_or(plan_rk.trip_revision),
                            graph_version: plan_rk.graph_version.clone(),
                        })
                    }
                    PlanCommitState::ClosedDriving => Err(CoreError::new(
                        ErrorCode::ForbiddenWhileDriving,
                        "committing a plan candidate is forbidden while driving",
                    )),
                    PlanCommitState::Closed => Err(CoreError::new(
                        ErrorCode::Stale,
                        "plan candidate commit window is already closed",
                    )),
                };
            }
        }
        Err(CoreError::new(
            ErrorCode::Stale,
            "req_id does not name an issued commit or an ok candidate of the current plan",
        ))
    }

    /// The full `dc_route_commit` guard (02 §2.1 #9): `expected_route_key` equals the
    /// current route (`None` only when no route is committed yet), and `req_id` names either
    /// an unsettled `commit{req_id, route_key}` Rust issued for that `route_key`, or an `ok`
    /// candidate search of the current plan while its commit window is open
    /// (`E_FORBIDDEN_WHILE_DRIVING` if closed by driving, `E_STALE` otherwise).
    pub fn check_route_guard(
        &self,
        req_id: &str,
        route_key: Option<&RouteKey>,
    ) -> Result<(), CoreError> {
        self.classify_commit_guard(req_id, route_key).map(|_| ())
    }

    /// `dc_route_commit` body (02 §2.1 #9, 04 §3.1): re-checks the guard
    /// (`expected_route_key` + `req_id` → `E_STALE` / `E_FORBIDDEN_WHILE_DRIVING`, nothing
    /// replaced), validates and indexes the bundle (`E_SCHEMA`), then makes it the current
    /// route. Alignment starts over (status `uncertain` until a fix matches; the first fix
    /// is located through the spatial index). The next `dc_tick` returns the new horizon.
    ///
    /// Revision rule: the bundle always carries `route_revision` "0" (`E_SCHEMA` otherwise)
    /// and Rust assigns current + 1 (the first route of the session: 1; never reset when
    /// `trip_revision` advances). The assigned key replaces the bundle's `route_key` and
    /// every `ManeuverId.route_key` in it, and is returned in the result. While a route is
    /// current, the bundle's `trip_id` and `graph_version` must equal the current route's
    /// (06 §3: pinned while driving), and `trip_revision` must equal current or — only for a
    /// plan candidate whose plan has that `trip_revision` — equal current + 1 (`E_STALE`
    /// otherwise). Order of checks: guard (`E_STALE` / `E_FORBIDDEN_WHILE_DRIVING`), revision
    /// "0" (`E_SCHEMA`), trip_id / trip_revision / graph_version (`E_STALE`), bundle body
    /// (`E_SCHEMA`); nothing is replaced on any error.
    pub fn route_commit(
        &mut self,
        req: &RouteCommitRequest,
    ) -> Result<RouteCommitResult, CoreError> {
        let source = self.classify_commit_guard(&req.req_id, req.expected_route_key.as_ref())?;
        let bundle_key = &req.bundle.route_key;
        if bundle_key.route_revision.0 != 0 {
            return Err(CoreError::new(
                ErrorCode::Schema,
                format!(
                    "bundle route_revision must be \"0\" (Rust assigns it), got {}",
                    bundle_key.route_revision.0
                ),
            ));
        }
        if let CommitSource::PlanCandidate {
            trip_id,
            trip_revision,
            graph_version,
        } = &source
        {
            if bundle_key.trip_id != *trip_id {
                return Err(CoreError::new(
                    ErrorCode::Stale,
                    format!("bundle trip_id {} != plan {}", bundle_key.trip_id, trip_id),
                ));
            }
            if bundle_key.trip_revision != *trip_revision {
                return Err(CoreError::new(
                    ErrorCode::Stale,
                    format!(
                        "bundle trip_revision {} != plan {}",
                        bundle_key.trip_revision.0, trip_revision.0
                    ),
                ));
            }
            if bundle_key.graph_version != *graph_version {
                return Err(CoreError::new(
                    ErrorCode::Stale,
                    format!(
                        "bundle graph_version {} != plan {}",
                        bundle_key.graph_version, graph_version
                    ),
                ));
            }
        }
        let revision = match &self.current_route_key {
            None => 1,
            Some(cur) => {
                if bundle_key.trip_id != cur.trip_id {
                    return Err(CoreError::new(
                        ErrorCode::Stale,
                        format!(
                            "bundle trip_id {} != current {}",
                            bundle_key.trip_id, cur.trip_id
                        ),
                    ));
                }
                let trip_rev_ok = match &source {
                    CommitSource::Issued => bundle_key.trip_revision == cur.trip_revision,
                    CommitSource::PlanCandidate { trip_revision, .. } => {
                        bundle_key.trip_revision == *trip_revision
                            && (bundle_key.trip_revision == cur.trip_revision
                                || cur.trip_revision.0.checked_add(1)
                                    == Some(bundle_key.trip_revision.0))
                    }
                };
                if !trip_rev_ok {
                    return Err(CoreError::new(
                        ErrorCode::Stale,
                        format!(
                            "bundle trip_revision {} not allowed for current {}",
                            bundle_key.trip_revision.0, cur.trip_revision.0
                        ),
                    ));
                }
                if bundle_key.graph_version != cur.graph_version {
                    return Err(CoreError::new(
                        ErrorCode::Stale,
                        format!(
                            "bundle graph_version {} != current {}",
                            bundle_key.graph_version, cur.graph_version
                        ),
                    ));
                }
                cur.route_revision
                    .0
                    .checked_add(1)
                    .ok_or_else(|| CoreError::new(ErrorCode::Stale, "route_revision exhausted"))?
            }
        };
        let mut loaded = LoadedRoute::load(&req.bundle)
            .map_err(|e| CoreError::new(ErrorCode::Schema, format!("route_bundle: {e}")))?;
        loaded.assign_route_key(RouteKey {
            route_revision: contracts::U64Str(revision),
            ..bundle_key.clone()
        });
        let result = RouteCommitResult {
            route_key: loaded.route_key.clone(),
            legs: req.bundle.legs.len() as u32,
            segments: loaded.segs.len() as u32,
            maneuvers: loaded.maneuvers.len() as u32,
        };
        let undo = self.undo_window_for(&req.req_id, &loaded);
        let now = self.last_now_mono_ms.unwrap_or(0);
        let mut fx = Effects::default();
        self.planner
            .on_route_changed(now, &loaded.route_key, &mut fx);
        self.pending_planner.extend(fx.planner_cmds);
        self.pending_log.extend(fx.log);
        self.planner.invalidate_route_eta();
        if matches!(source, CommitSource::PlanCandidate { .. }) {
            self.planner.close_plan_commit_committed();
        }
        self.current_route_key = Some(loaded.route_key.clone());
        self.history.on_route_changed(
            now,
            &loaded.history_pois,
            &mut self.pending_voice,
            &mut self.pending_ui,
            &mut self.pending_log,
        );
        self.route = Some(loaded);
        self.aligner = Aligner::new(self.align_cfg);
        self.finished = false;
        self.frames.reset_route();
        self.last_fix_at = None;
        self.last_fix_lateral_ok = false;
        self.last_fix_acc_and_bearing_ok = false;
        self.reset_avoid();
        // #22: the undo brings the original route back (the same body, by ordinal): its
        // junctions already tried stay tried, so the one just undone is not offered again.
        if let Some(r) = self.undo_return.take() {
            if req.req_id == r.undo_req_id {
                let key = &self.current_route_key;
                self.avoid_tried = r
                    .tried_before
                    .into_iter()
                    .map(|mut id| {
                        if let Some(k) = key {
                            id.route_key = k.clone();
                        }
                        id
                    })
                    .collect();
            }
        }
        self.undo = undo;
        self.issued_commits.clear();
        Ok(result)
    }

    /// The undo state after loading `loaded` (03 §3.1, team decision 2026-11-26): when this
    /// commit loads the consented avoid route (its `req_id` is the consent commit's, so a
    /// reroute committed against the same guard is never taken for it), the divergence
    /// point is computed once, here, from the old route and the new one and kept for
    /// "戻して". Any other commit (including the undo itself) ends the window.
    fn undo_window_for(&self, req_id: &str, loaded: &LoadedRoute) -> UndoState {
        let UndoState::Consented(c) = &self.undo else {
            return UndoState::None;
        };
        let Some(old) = self.route.as_ref() else {
            return UndoState::None;
        };
        if req_id != c.req_id {
            return UndoState::None;
        }
        UndoState::Window(UndoWindow {
            offer_id: c.offer_id.clone(),
            undo_req_id: c.undo_req_id.clone(),
            route_key: loaded.route_key.clone(),
            consent_at_mono_ms: c.consent_at_mono_ms,
            divergence_cm: avoid::divergence_cm(
                &old.edge_spans(),
                &loaded.edge_spans(),
                c.start_progress_cm,
            ),
            tried_before: self.avoid_tried.clone(),
        })
    }

    /// "戻して" (`user_tap{offer_id, undo}` at `at`, or `CXL` spoken at `at`), 03 §3.1.
    /// Ok: `commit{req_id: undo_req_id, route_key: current}` pushed to `cmds` and the held
    /// result dropped. Before the avoid route is loaded (consent `commit` not settled yet):
    /// `cancel_commit{req_id}` instead (docs 836a370) — Rust stops waiting for that commit at
    /// once, so a `dc_route_commit` for it arriving later is `E_STALE`; also a success.
    /// Err: the reject code; the caller speaks the refusal.
    fn try_undo(
        &mut self,
        now: u64,
        at: u64,
        target_id: Option<&str>,
        cmds: &mut Vec<PlannerCmd>,
        log: &mut Vec<LogEvent>,
    ) -> Result<String, RejectCode> {
        let (offer_id, consent_at) = match &self.undo {
            UndoState::None => return Err(RejectCode::NoPendingTarget),
            UndoState::Consented(c) => (&c.offer_id, c.consent_at_mono_ms),
            UndoState::Window(w) => (&w.offer_id, w.consent_at_mono_ms),
        };
        if target_id.is_some_and(|t| t != offer_id) {
            return Err(RejectCode::NoPendingTarget);
        }
        if at.saturating_sub(consent_at) > self.avoid_cfg.undo_window_ms {
            self.undo = UndoState::None;
            return Err(RejectCode::NoPendingTarget);
        }
        // Consent given but the avoid route is not loaded yet: withdraw the commit.
        if let UndoState::Consented(c) = &self.undo {
            let req_id = c.req_id.clone();
            let offer_id = c.offer_id.clone();
            self.issued_commits.retain(|(r, _)| *r != req_id);
            let mut fx = Effects::default();
            self.planner.discard_held(at, "undo", &mut fx);
            log.append(&mut fx.log);
            cmds.push(PlannerCmd::CancelCommit {
                req_id: req_id.clone(),
            });
            log.push(log_req(at, "avoid_commit_cancelled", &req_id, &offer_id));
            self.undo = UndoState::None;
            return Ok(req_id);
        }
        let UndoState::Window(w) = &self.undo else {
            return Err(RejectCode::NoPendingTarget);
        };
        if self.current_route_key.as_ref() != Some(&w.route_key) {
            return Err(RejectCode::NoPendingTarget);
        }
        let Some(divergence) = w.divergence_cm else {
            return Err(RejectCode::NoEffect);
        };
        // uncertain (or off route): whether the divergence was passed is unknown → safe side.
        if self.horizon_status(now) != Some(HorizonStatus::Tracking) {
            return Err(RejectCode::NoEffect);
        }
        match self.aligner.progress_cm() {
            Some(p) if p < divergence => {}
            _ => return Err(RejectCode::NoEffect),
        }
        let req_id = w.undo_req_id.clone();
        let mut fx = Effects::default();
        self.planner.discard_held(at, "undo", &mut fx);
        log.append(&mut fx.log);
        cmds.push(PlannerCmd::Commit {
            req_id: req_id.clone(),
            route_key: w.route_key.clone(),
            undo_req_id: None,
        });
        log.push(log_req(at, "avoid_undo_commit", &req_id, &w.offer_id));
        self.undo_return = Some(UndoReturn {
            undo_req_id: req_id.clone(),
            tried_before: w.tried_before.clone(),
        });
        self.undo = UndoState::None;
        Ok(req_id)
    }

    /// The short fixed phrase for a refused "戻して" (03 §3.1).
    fn undo_refused(&mut self, at: u64, code: RejectCode, log: &mut Vec<LogEvent>) {
        let code_name = match code {
            RejectCode::NoEffect => "NO_EFFECT",
            _ => "NO_PENDING_TARGET",
        };
        log.push(log_at(at, "avoid_undo_refused", Some(code_name.to_owned())));
        // TODO(05 §5): utterance_token encoding is provisional.
        let token = format!("{}-{at}", avoid::UNDO_UNAVAILABLE_TEMPLATE_ID);
        self.history.note_p2_token(&token);
        self.pending_voice.push(VoiceCmd::Speak {
            token,
            priority: VoicePriority::P2,
            template_id: avoid::UNDO_UNAVAILABLE_TEMPLATE_ID.to_owned(),
            params: std::collections::BTreeMap::new(),
        });
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
        let mut tick_voice: Vec<VoiceCmd> = Vec::new();
        let mut extra_log: Vec<LogEvent> = Vec::new();

        // Time never goes backwards inside the core: clamp to the previous value and
        // record clock_regression (02 §2.1 / team decision 2026-11-24).
        let now = self.clamp_now(input.now_mono_ms, &mut extra_log);
        // Status and remaining ETA as of `now`, before this tick's events / fix.
        self.refresh_eta(now);
        let gate = self.commit_gate(now);

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
                    base_facts_ref,
                    ..
                } => {
                    self.planner.on_search_finished(
                        at,
                        &req_id,
                        &route_key,
                        outcome,
                        facts_ref.as_deref(),
                        base_facts_ref.as_deref(),
                        self.current_route_key.as_ref(),
                        gate,
                        &mut fx,
                    );
                    self.on_avoid_finished(now, at, &req_id, &mut fx, &mut extra_log);
                }
                EventBody::UserTap { target_id, action } => {
                    self.on_user_tap(now, at, &target_id, action, &mut fx, &mut extra_log);
                }
                EventBody::AsrResult {
                    text_class: TextClass::Dict,
                    dict_norm: Some(norm),
                    ..
                } if avoid::is_dict_yes(&norm) => {
                    // Consent to the offer on screen only (no offer → nothing; 04 §4.3).
                    if matches!(self.avoid, AvoidState::Offered { .. }) {
                        self.consent(now, at, "dict_yes", &mut fx, &mut extra_log);
                    }
                }
                EventBody::AsrResult {
                    text_class: TextClass::Dict,
                    dict_norm: Some(norm),
                    ..
                } if self.history.is_speaking() && history::is_dict_stop(&norm) => {
                    // 05 §6.2 (94ef3b6 / fffd429): while P3 is Speaking, P3_STOP phrases
                    // ("やめて" / "止めて" / "とめて" / "ストップ") stop P3 only and never
                    // trigger CXL (no undo / no offer dismissal).
                    self.history
                        .on_user_stop(at, &mut tick_voice, &mut extra_log);
                }
                EventBody::AsrResult {
                    text_class: TextClass::Dict,
                    dict_norm: Some(norm),
                    ..
                } if avoid::is_dict_cxl(&norm) => {
                    // 05 §6.2 (94ef3b6 / fffd429): "キャンセル" / "取り消し" / "取り消して" /
                    // "戻して" / "元に戻して" are always CXL (never stop P3); "やめて" also
                    // falls through here when P3 is not Speaking.
                    self.on_dict_cxl(now, at, &mut fx, &mut extra_log);
                }
                EventBody::TtsStarted { token } => {
                    self.history.on_tts_started(
                        at,
                        &token,
                        &mut tick_voice,
                        &mut fx.ui_cmds,
                        &mut extra_log,
                    );
                }
                EventBody::TtsDone { token } => {
                    self.history.on_tts_done(at, &token, &mut extra_log);
                }
                EventBody::TtsStopped { token } => {
                    self.history.on_tts_stopped(at, &token, &mut extra_log);
                }
                EventBody::CallState { active } => {
                    self.history.on_call_state(
                        at,
                        active,
                        &mut tick_voice,
                        &mut fx.ui_cmds,
                        &mut extra_log,
                    );
                }
                EventBody::AudioFocusLost {} => {
                    self.history.on_audio_focus_lost(
                        at,
                        &mut tick_voice,
                        &mut fx.ui_cmds,
                        &mut extra_log,
                    );
                }
                EventBody::AudioFocusGained {} => {
                    self.history.on_audio_focus_gained(at, &mut extra_log);
                }
                EventBody::HistoryConsent { on } => {
                    self.history
                        .on_consent(at, on, &mut tick_voice, &mut extra_log);
                }
                EventBody::EngineReset {} => self.planner.on_engine_reset(at, &mut fx),
                EventBody::Thermal { status } => {
                    let lvl = ThermalLevel::from(status);
                    if lvl != self.thermal {
                        extra_log.push(log_at(at, "thermal_changed", Some(format!("{lvl:?}"))));
                        self.thermal = lvl;
                    }
                }
                // TODO(drive-core): ASR (asr_* other than dict yes/stop, override_ready,
                // user_tap for overrides / confirm screens), inference tier, low memory.
                _ => {}
            }
        }
        if let Some(fix) = input.fix {
            self.on_fix(now, fix, &mut fx, &mut extra_log);
        }
        self.planner.on_now(now, &mut fx);
        self.avoid_budget(now, &mut fx, &mut extra_log);
        // A result held while uncertain is committed once the session may commit again.
        let gate = self.commit_gate(now);
        self.planner
            .release_held(now, gate, self.current_route_key.as_ref(), &mut fx);
        self.refresh_eta(now);
        self.avoid_step(now, &mut fx, &mut extra_log);
        let horizon = self.next_horizon(now);
        self.history_step(now, &mut tick_voice, &mut fx.ui_cmds, &mut extra_log);
        self.finish_with_horizon(now, horizon, fx, tick_voice, extra_log)
    }

    fn history_step(
        &mut self,
        now: u64,
        tick_voice: &mut Vec<VoiceCmd>,
        ui_cmds: &mut Vec<UiCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        let has_p0_or_p1_speak = self
            .pending_voice
            .iter()
            .chain(tick_voice.iter())
            .any(|c| matches!(c.priority(), Some(VoicePriority::P0 | VoicePriority::P1)));
        let has_any_other_speak = self
            .pending_voice
            .iter()
            .chain(tick_voice.iter())
            .any(|c| matches!(c, VoiceCmd::Speak { .. }));
        let status = self.horizon_status(now);
        let last_h = self.frames.last();
        let next_p1_deadline_mono_ms = last_h.and_then(|h| h.next_p1_deadline_monotonic_ms);
        let next_complex_window = last_h.and_then(|h| h.next_complex_window.as_ref());
        let ctx = StepContext {
            now,
            route_key: self.current_route_key.as_ref(),
            status,
            progress_cm: self.aligner.progress_cm(),
            next_p1_deadline_mono_ms,
            next_complex_window,
            lateral_ok: self.last_fix_lateral_ok,
            acc_and_bearing_ok: self.last_fix_acc_and_bearing_ok,
            thermal_ok: self.thermal <= ThermalLevel::Light,
            has_p0_or_p1_speak,
            has_any_other_speak,
        };
        self.history.step(&ctx, tick_voice, ui_cmds, log);
    }

    /// The avoid-chip search `req_id` ended: a usable `ok` with both `facts_ref` and
    /// `base_facts_ref` (only while tracking, 03 §3.1) emits `evaluate_offer` and waits for
    /// `dc_avoid_offer_eval`; a missing ref (no `base_facts` → (b) cannot be checked, 04 §4.3)
    /// or anything else ends the attempt, logged.
    fn on_avoid_finished(
        &mut self,
        now: u64,
        at: u64,
        req_id: &str,
        fx: &mut Effects,
        log: &mut Vec<LogEvent>,
    ) {
        let AvoidState::Searching {
            req_id: r, target, ..
        } = &self.avoid
        else {
            return;
        };
        if r != req_id {
            return;
        }
        let ready = fx.avoid_ready.iter().find(|a| a.req_id == req_id);
        let tracking = self.horizon_status(now) == Some(HorizonStatus::Tracking);
        let refs = ready.and_then(|a| Some((a.facts_ref.clone()?, a.base_facts_ref.clone()?)));
        self.avoid = if let (Some((facts_ref, base_facts_ref)), true) = (refs, tracking) {
            fx.planner_cmds.push(PlannerCmd::EvaluateOffer {
                req_id: r.clone(),
                facts_ref,
                base_facts_ref,
            });
            AvoidState::Ready {
                req_id: r.clone(),
                target: target.clone(),
            }
        } else {
            let why = match ready {
                None => "search_unused",
                Some(_) if !tracking => "not_tracking",
                Some(a) if a.facts_ref.is_none() => "no_facts_ref",
                Some(_) => "no_base_facts_ref",
            };
            log.push(log_req(at, "avoid_attempt_ended", req_id, why));
            AvoidState::Idle
        };
    }

    /// Dictionary `CXL` (`asr_result{text_class: dict}` matching `avoid::DICT_CXL`,
    /// when not intercepted as `P3_STOP` during P3 Speaking): dismisses an active avoid offer
    /// or triggers the 5 s avoid undo (`try_undo`). Never stops P3 history commentary.
    fn on_dict_cxl(&mut self, now: u64, at: u64, fx: &mut Effects, log: &mut Vec<LogEvent>) {
        if let AvoidState::Offered { req_id, offer, .. } = &self.avoid {
            let offer_id = offer.offer_id.clone();
            let req_id = req_id.clone();
            fx.ui_cmds.push(UiCmd::Dismiss {
                target_id: offer_id.clone(),
                reason: DismissReason::Cancelled,
            });
            log.push(log_req(at, "avoid_offer_cancelled", &req_id, &offer_id));
            self.avoid = AvoidState::Idle;
            return;
        }
        if !matches!(self.undo, UndoState::None) {
            if let Err(code) = self.try_undo(now, at, None, &mut fx.planner_cmds, log) {
                self.undo_refused(at, code, log);
            }
            return;
        }
        log.push(log_at(
            at,
            "dict_cxl_no_target",
            Some("NO_PENDING_TARGET".to_owned()),
        ));
    }

    /// Consent to the chip: `user_tap{target_id: offer_id, action: avoid}` before the
    /// deadline, while tracking, on the route the search started on → `commit`.
    fn on_user_tap(
        &mut self,
        now: u64,
        at: u64,
        target_id: &str,
        action: TapAction,
        fx: &mut Effects,
        log: &mut Vec<LogEvent>,
    ) {
        if action == TapAction::Undo {
            if let Err(code) = self.try_undo(now, at, Some(target_id), &mut fx.planner_cmds, log) {
                self.undo_refused(at, code, log);
            }
            return;
        }
        let AvoidState::Offered { offer, .. } = &self.avoid else {
            return;
        };
        if offer.offer_id != target_id {
            return;
        }
        if action != TapAction::Avoid {
            let req_id = self.avoid_req_id().unwrap_or_default();
            log.push(log_req(at, "avoid_consent_refused", &req_id, "action"));
            return;
        }
        self.consent(now, at, "tap", fx, log);
    }

    fn avoid_req_id(&self) -> Option<String> {
        match &self.avoid {
            AvoidState::Searching { req_id, .. }
            | AvoidState::Ready { req_id, .. }
            | AvoidState::Offered { req_id, .. } => Some(req_id.clone()),
            AvoidState::Idle => None,
        }
    }

    /// Consent to the offer on screen at `at` (`user_tap{offer_id, avoid}` or the dictionary
    /// "はい", ADR-0007: the only two confirming inputs): before the deadline, while
    /// tracking, on the route the search started on → `commit{req_id, route_key,
    /// undo_req_id}`. Otherwise refused (logged, nothing changes).
    fn consent(
        &mut self,
        now: u64,
        at: u64,
        source: &str,
        fx: &mut Effects,
        log: &mut Vec<LogEvent>,
    ) {
        let AvoidState::Offered {
            req_id,
            target,
            offer,
        } = &self.avoid
        else {
            return;
        };
        let refused = if at >= target.deadline_mono_ms {
            Some("deadline_passed")
        } else if self.horizon_status(now) != Some(HorizonStatus::Tracking) {
            Some("not_tracking")
        } else if self.current_route_key.as_ref() != Some(&target.route_key) {
            Some("route_key")
        } else {
            None
        };
        if let Some(why) = refused {
            log.push(log_req(at, "avoid_consent_refused", req_id, why));
            return;
        }
        let undo_req_id = avoid::undo_req_id(req_id);
        fx.planner_cmds.push(PlannerCmd::Commit {
            req_id: req_id.clone(),
            route_key: target.route_key.clone(),
            undo_req_id: Some(undo_req_id.clone()),
        });
        log.push(log_req(
            at,
            "avoid_consent_commit",
            req_id,
            &format!("{} source={source}", offer.offer_id),
        ));
        self.undo = UndoState::Consented(Consent {
            req_id: req_id.clone(),
            offer_id: offer.offer_id.clone(),
            undo_req_id,
            from_route_key: target.route_key.clone(),
            start_progress_cm: target.start_progress_cm,
            consent_at_mono_ms: at,
        });
        self.avoid = AvoidState::Idle;
    }

    /// The avoid search outlived its compute budget (1.5 s, 04 §4.3 item 1): `cancel` once.
    /// The slot stays occupied until `search_finished` (03 §3.1), which ends the attempt.
    fn avoid_budget(&mut self, now: u64, fx: &mut Effects, log: &mut Vec<LogEvent>) {
        let AvoidState::Searching {
            req_id,
            budget_deadline_mono_ms: budget,
            ..
        } = &mut self.avoid
        else {
            return;
        };
        if !budget.is_some_and(|at| now >= at) {
            return;
        }
        *budget = None;
        let req_id = req_id.clone();
        if self.planner.cancel_search(now, &req_id, fx) {
            log.push(log_req(now, "avoid_budget_expired", &req_id, "cancel"));
        }
    }

    /// End of a tick: removes a chip past its deadline or off the route, cancels any running
    /// avoid-chip search if the vehicle left the route, then looks for a new avoid target
    /// (see `crate::avoid`).
    fn avoid_step(&mut self, now: u64, fx: &mut Effects, log: &mut Vec<LogEvent>) {
        let status = self.horizon_status(now);
        let off = matches!(
            status,
            Some(HorizonStatus::OffRoute | HorizonStatus::Rerouting | HorizonStatus::Finished)
        );
        if off {
            if let AvoidState::Searching {
                req_id,
                budget_deadline_mono_ms,
                ..
            } = &mut self.avoid
            {
                *budget_deadline_mono_ms = None;
                let req_id = req_id.clone();
                self.planner.cancel_search(now, &req_id, fx);
            }
            self.planner.cancel_avoid_in_slot(now, fx);
        }
        if let Some(t) = self.avoid.target() {
            let expired = now >= t.deadline_mono_ms;
            // A search still running ends through its own search_finished.
            let searching = matches!(self.avoid, AvoidState::Searching { .. });
            if expired || (off && !searching) {
                let reason = if expired {
                    DismissReason::Expired
                } else {
                    DismissReason::RouteChanged
                };
                if let Some((_, offer)) = self.avoid.offer() {
                    fx.ui_cmds.push(UiCmd::Dismiss {
                        target_id: offer.offer_id.clone(),
                        reason,
                    });
                }
                let what = if expired {
                    "deadline_passed"
                } else {
                    "left_route"
                };
                log.push(log_at(now, "avoid_withdrawn", Some(what.to_owned())));
                self.avoid = AvoidState::Idle;
            }
        }
        if !self.avoid_cfg.enabled
            || self.avoid != AvoidState::Idle
            || status != Some(HorizonStatus::Tracking)
            || !self.planner.is_idle()
        {
            return;
        }
        let (Some(route), Some(progress), Some(key)) = (
            self.route.as_ref(),
            self.aligner.progress_cm(),
            self.current_route_key.clone(),
        ) else {
            return;
        };
        let sampled = self.last_fix_at.unwrap_or(now);
        let current = route.time_at(progress);
        let factors = self.frames.eta_factors();
        let first = self.frames.first_unpassed(route);
        let mut found = None;
        for (k, m) in route.maneuvers.iter().enumerate().skip(first) {
            let earliest =
                u64::from(eta_range(route, m.anchor.along_route_cm, current, factors).earliest);
            if k - first >= UPCOMING_MAX && earliest > self.avoid_cfg.trigger_eta_ms {
                break;
            }
            let Some(kind) = avoid::target_kind(m) else {
                continue;
            };
            if self.avoid_tried.contains(&m.id) {
                continue;
            }
            self.avoid_tried.push(m.id.clone());
            let deadline = sampled
                .saturating_add(earliest)
                .saturating_sub(self.avoid_cfg.deadline_before_ms);
            if now.saturating_add(self.avoid_cfg.budget_ms) > deadline {
                let detail = format!("ordinal={} deadline={deadline}", m.id.ordinal);
                log.push(log_at(now, "avoid_too_close", Some(detail)));
                continue;
            }
            found = Some(AvoidTarget {
                maneuver_id: m.id.clone(),
                route_key: key.clone(),
                deadline_mono_ms: deadline,
                base_eta_ms: u64::from(route.remaining_ms(progress)),
                avoided: avoid::avoided(m),
                kind,
                start_progress_cm: progress,
                edge_pair: route.edge_pair_at(m.anchor.along_route_cm),
            });
            break;
        }
        let Some(target) = found else { return };
        let before = fx.planner_cmds.len();
        self.planner
            .request(now, SearchPriority::AvoidChip, key, None, fx);
        let started = fx.planner_cmds[before..].iter().find_map(|c| match c {
            PlannerCmd::Start {
                req_id,
                priority: SearchPriority::AvoidChip,
                ..
            } => Some(req_id.clone()),
            _ => None,
        });
        if let Some(req_id) = started {
            self.avoid = AvoidState::Searching {
                req_id,
                target,
                budget_deadline_mono_ms: Some(now.saturating_add(self.avoid_cfg.budget_ms)),
            };
        }
    }

    /// `dc_avoid_offer_eval(s, {req_id, now_mono_ms, alt_facts, base_facts})` (02 §2.1 #11):
    /// for the `req_id` of `evaluate_offer`, turns the waiting
    /// avoid-chip result into an `AvoidOffer` on its `HorizonEntry` when it is still
    /// before the deadline, the session is tracking and the alternative is within the
    /// detour cap (`DetourPolicy`, base_eta = remaining ETA when the search started).
    /// Any other outcome drops the result. The frame itself comes with the next `dc_tick`.
    pub fn avoid_offer_eval(&mut self, req: &AvoidOfferEvalRequest) -> AvoidOfferEvalResult {
        let now = self
            .last_now_mono_ms
            .map_or(req.now_mono_ms, |p| p.max(req.now_mono_ms));
        let declined = |reason| AvoidOfferEvalResult::Declined { reason };
        // A search still running, an offer already shown, or another search's req_id is
        // left as it is.
        if !matches!(&self.avoid, AvoidState::Ready { req_id, .. } if *req_id == req.req_id) {
            return declined(AvoidDeclineReason::NoPendingResult);
        }
        let AvoidState::Ready { req_id, target } =
            std::mem::replace(&mut self.avoid, AvoidState::Idle)
        else {
            unreachable!("checked above");
        };
        if self.horizon_status(now) != Some(HorizonStatus::Tracking) {
            return declined(AvoidDeclineReason::NotTracking);
        }
        if now >= target.deadline_mono_ms {
            return declined(AvoidDeclineReason::DeadlinePassed);
        }
        match avoid::build_offer(
            &req_id,
            &target,
            &target.route_key.graph_version,
            &req.alt_facts,
            &req.base_facts,
            &self.detour,
        ) {
            Ok(offer) => {
                self.avoid = AvoidState::Offered {
                    req_id,
                    target,
                    offer: offer.clone(),
                };
                AvoidOfferEvalResult::Offered { avoid_offer: offer }
            }
            Err(reason) => declined(reason),
        }
    }

    /// Soft-mode entry / drive-start tracking (02 §2.1 #9, 03 §3.1, 07 §1):
    /// `speed_cm_s >= drive_start_speed_cm_s` (default 222 cm/s = 8 km/h) continuously for
    /// `drive_start_hold_ms` (default 3000 ms) closes the current plan's commit window
    /// (`E_FORBIDDEN_WHILE_DRIVING`). A fix without speed, below threshold, or with
    /// `acc_cm > max_acc_cm` resets the continuous run; an out-of-order fix is ignored.
    fn observe_drive_start(&mut self, fix: &Fix) {
        let at = fix.at_mono_ms;
        if self.drive_last_fix_at_mono_ms.is_some_and(|prev| at < prev) {
            return;
        }
        self.drive_last_fix_at_mono_ms = Some(at);
        let fast = fix.acc_cm <= self.align_cfg.max_acc_cm
            && fix
                .speed_cm_s
                .is_some_and(|s| s >= self.drive_start_speed_cm_s);
        if !fast {
            self.drive_fast_since_mono_ms = None;
            return;
        }
        let since = *self.drive_fast_since_mono_ms.get_or_insert(at);
        if at.saturating_sub(since) >= self.drive_start_hold_ms {
            self.planner.close_plan_commit_driving();
        }
    }

    /// On-route alignment of one fix (04 §3.1); confirms off-route and requests the reroute
    /// with this fix as the new origin.
    fn on_fix(&mut self, now: u64, fix: Fix, fx: &mut Effects, log: &mut Vec<LogEvent>) {
        // Tracked even before the first route is committed.
        self.observe_drive_start(&fix);
        let Some(route) = self.route.as_ref() else {
            return;
        };
        if self.finished {
            return;
        }
        let was_tracking = self.aligner.tracking(now);
        let at = fix.at_mono_ms;
        let r = self.aligner.on_fix(route, &fix);
        if matches!(r, FixResult::OnRoute { .. }) {
            self.last_fix_at = Some(at);
        }
        if r != FixResult::OutOfOrder && (!was_tracking || !matches!(r, FixResult::OnRoute { .. }))
        {
            self.frames.observe_sample(route, at, None);
        }
        match r {
            FixResult::OnRoute {
                along_cm,
                lateral_cm,
                mode,
            } => {
                self.last_fix_lateral_ok = lateral_cm <= u64::from(self.align_cfg.lateral_tol_cm);
                self.last_fix_acc_and_bearing_ok = fix.acc_cm <= self.align_cfg.max_acc_cm
                    && fix.bearing_cdeg.is_some()
                    && fix
                        .speed_cm_s
                        .is_some_and(|v| v >= self.align_cfg.heading_min_speed_cm_s);
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
                    .observe_sample(route, at, Some(along_cm))
                    .is_some_and(|p| arrived(route, p))
                {
                    self.finished = true;
                    log.push(log_at(at, "route_finished", None));
                }
            }
            FixResult::Ambiguous { parts } => {
                self.last_fix_lateral_ok = false;
                self.last_fix_acc_and_bearing_ok = false;
                if was_tracking || parts > 1 {
                    log.push(log_at(
                        at,
                        "align_ambiguous",
                        Some(format!("parts={parts}")),
                    ));
                }
            }
            FixResult::Off { run, confirmed_now } => {
                self.last_fix_lateral_ok = false;
                self.last_fix_acc_and_bearing_ok = false;
                if confirmed_now {
                    log.push(log_at(
                        at,
                        "off_route_confirmed",
                        Some(format!("run={run}")),
                    ));
                }
            }
            FixResult::LowAccuracy { run } => {
                self.last_fix_lateral_ok = false;
                self.last_fix_acc_and_bearing_ok = false;
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

    fn finish(&mut self, now: u64, fx: Effects, extra_log: Vec<LogEvent>) -> TickOutput {
        let horizon = self.next_horizon(now);
        self.finish_with_horizon(now, horizon, fx, Vec::new(), extra_log)
    }

    fn finish_with_horizon(
        &mut self,
        now: u64,
        horizon: Option<ManeuverHorizon>,
        fx: Effects,
        tick_voice: Vec<VoiceCmd>,
        mut extra_log: Vec<LogEvent>,
    ) -> TickOutput {
        let mut log = std::mem::take(&mut self.pending_log);
        log.extend(fx.log);
        log.append(&mut extra_log);
        log.sort_by_key(|l| l.at_mono_ms); // stable: deterministic order
                                           // The freshness limit can turn tracking into uncertain without new input.
        let stale_at = self.route.as_ref().and_then(|_| self.aligner.stale_at(now));
        let mut ui_cmds = std::mem::take(&mut self.pending_ui);
        ui_cmds.extend(fx.ui_cmds);
        let mut voice_cmds = std::mem::take(&mut self.pending_voice);
        voice_cmds.extend(tick_voice);
        let mut planner_cmds = std::mem::take(&mut self.pending_planner);
        planner_cmds.extend(fx.planner_cmds);
        // In order: a consent and its cancel in the same output leave nothing waiting.
        for c in &planner_cmds {
            match c {
                PlannerCmd::Commit {
                    req_id, route_key, ..
                } => self
                    .issued_commits
                    .push((req_id.clone(), route_key.clone())),
                PlannerCmd::CancelCommit { req_id } => {
                    self.issued_commits.retain(|(r, _)| r != req_id)
                }
                _ => {}
            }
        }
        let next_wake_mono_ms = [
            self.planner.next_wake(),
            self.avoid.target().map(|t| t.deadline_mono_ms),
            match &self.avoid {
                AvoidState::Searching {
                    budget_deadline_mono_ms,
                    ..
                } => *budget_deadline_mono_ms,
                _ => None,
            },
            stale_at,
            self.frames.last_ref_expires_at(),
            self.frames.window_expires_at(),
            self.history.next_wake(now),
        ]
        .into_iter()
        .flatten()
        .min();
        TickOutput {
            horizon,
            voice_cmds,
            ui_cmds,
            planner_cmds,
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
            now,
            self.avoid.offer(),
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
            // CXL without a pending offer: "戻して" of an avoid switch confirmed within 5 s
            // (03 §3.1). The commit / refusal phrase goes out with the next dc_tick.
            Command::Cxl if req.pending_offer_id.is_none() => {
                let at = req.utterance_mono_ms;
                let now = self.last_now_mono_ms.map_or(at, |p| p.max(at));
                let mut cmds = Vec::new();
                let mut log = Vec::new();
                let r = self.try_undo(now, at, None, &mut cmds, &mut log);
                if let Err(code) = r {
                    self.undo_refused(at, code, &mut log);
                }
                self.pending_planner.extend(cmds);
                self.pending_log.extend(log);
                match r {
                    Ok(req_id) => Ok(ValidateOverrideResult::Undone { req_id }),
                    Err(code) => rejected(code),
                }
            }
            // L3 BMK ("あとで詳しく", 03 §4.1, 05 §6.2): target is the currently Speaking POI,
            // otherwise the most recent Completed / Interrupted POI within p3_bmk_window_ms
            // (default 60 s). Never guesses another POI -> NO_PENDING_TARGET.
            Command::Bmk => {
                let at = req.utterance_mono_ms;
                match self.history.resolve_bmk_target(at) {
                    Some(poi_id) => {
                        self.pending_ui.push(UiCmd::SaveForLater {
                            poi_id: poi_id.clone(),
                            reason: SaveForLaterReason::Bmk,
                        });
                        self.pending_log.push(log_at(
                            at,
                            "p3_bmk_saved",
                            Some(format!("poi_id={poi_id}")),
                        ));
                        Ok(ValidateOverrideResult::Bookmarked { poi_id })
                    }
                    None => {
                        self.pending_log.push(log_at(
                            at,
                            "p3_bmk_rejected",
                            Some("NO_PENDING_TARGET".to_owned()),
                        ));
                        rejected(RejectCode::NoPendingTarget)
                    }
                }
            }
            // NO_PENDING_TARGET = YES / NO without a target (team decision #3).
            // TODO(drive-core): pending offers should come from session state, not only the
            // request field.
            Command::Yes | Command::No if req.pending_offer_id.is_none() => {
                rejected(RejectCode::NoPendingTarget)
            }
            // A missing last junction skips only the profile proposal (03 §4.1).
            Command::Ovr(a) if a.jp == Some(JpRef::Next) => {
                if let Some(reference) = self.frames.refs().and_then(|refs| refs.next.as_ref()) {
                    if self.frames.frame_seq() != req.ref_frame_seq {
                        return rejected(RejectCode::StaleRevision);
                    }
                    if req.utterance_mono_ms >= reference.deadline_mono_ms {
                        return rejected(RejectCode::Expired);
                    }
                    return Err(CoreError::new(
                        ErrorCode::Unimplemented,
                        "override safety gate pending",
                    ));
                }
                rejected(RejectCode::UnknownRef)
            }
            _ => Err(CoreError::new(
                ErrorCode::Unimplemented,
                "parsed; contract / Safety Gate / revision checks not implemented",
            )),
        }
    }
}

fn log_req(at: u64, what: &str, req_id: &str, detail: &str) -> LogEvent {
    LogEvent {
        at_mono_ms: at,
        what: what.to_owned(),
        req_id: Some(req_id.to_owned()),
        detail: Some(detail.to_owned()),
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
