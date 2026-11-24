//! `dc_session` state. Single-threaded: called only from the NavigationSession actor.

use crate::command_line::{self, Command, JpRef};
use crate::planner::{Effects, SearchPlanner};
use contracts::override_types::{RejectCode, ValidateOverrideRequest, ValidateOverrideResult};
use contracts::tick::{EventBody, LogEvent, SearchPriority, ThermalStatus, TickInput, TickOutput};
use contracts::{ErrorCode, RouteKey, U64Str};
use serde::Deserialize;

/// `dc_session_new` config. TODO(abi-v1): config contents are not specified anywhere;
/// only the provisional search deadline is read. Unknown fields are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// Long-distance search budget 8 s + 2 s margin (#41, provisional initial value).
    #[serde(default = "default_deadline")]
    pub search_deadline_ms: u64,
}

fn default_deadline() -> u64 {
    10_000
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            search_deadline_ms: default_deadline(),
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
            planner: SearchPlanner::new(config.search_deadline_ms),
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
        let raw_now = input.now_mono_ms.0;
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

        let mut prev_at: Option<u64> = None;
        for ev in input.events {
            // Events must be in at_mono_ms ascending order. Not re-sorted: an out-of-order
            // event is clamped to the previous value and clock_regression is recorded.
            let raw_at = ev.at_mono_ms.0;
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
                    comparison_ref,
                    ..
                } => {
                    self.planner.on_search_finished(
                        at,
                        &req_id,
                        &route_key,
                        outcome,
                        comparison_ref.as_deref(),
                        self.current_route_key.as_ref(),
                        &mut fx,
                    );
                }
                EventBody::EngineReset => self.planner.on_engine_reset(at, &mut fx),
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

    fn finish(&self, fx: Effects, mut extra_log: Vec<LogEvent>) -> TickOutput {
        let mut log = fx.log;
        log.append(&mut extra_log);
        log.sort_by_key(|l| l.at_mono_ms); // stable: deterministic order
        TickOutput {
            horizon: None, // TODO(drive-core): ManeuverHorizon normalization
            voice_cmds: Vec::new(),
            ui_cmds: fx.ui_cmds,
            planner_cmds: fx.planner_cmds,
            next_wake_mono_ms: self.planner.next_wake().map(U64Str),
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
        at_mono_ms: U64Str(at),
        what: what.to_owned(),
        req_id: None,
        detail,
    }
}
