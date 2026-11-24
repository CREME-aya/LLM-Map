//! Search slot state machine (devteam #41, finalized in #54/#56).
//!
//! ```text
//! Idle → Running{req} → CancelRequested{req} → Idle
//! ```
//! - Exactly one terminal `search_finished` per search. The next `start` is never emitted
//!   before it arrives, so at most one search runs at a time *by construction*.
//! - Lower-priority searches are stopped with `cancel` (Kotlin sets the atomic cancel flag);
//!   re-search starts when the native call has returned (= `search_finished`).
//! - `ok` after `cancel` is discarded (`discarded_late`), it only frees the slot.
//! - Deadline exceeded while the native call is still running → `engine_stuck`:
//!   the slot stays occupied until the call physically returns. `engine_reset` alone
//!   never resumes searching.

use contracts::tick::{LogEvent, PlannerCmd, SearchOutcome, SearchPriority, UiCmd};
use contracts::{RouteKey, U64Str};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReq {
    pub req_id: String,
    pub priority: SearchPriority,
    pub route_key: RouteKey,
    pub started_at_mono_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    Idle,
    Running { req: SearchReq, stuck: bool },
    CancelRequested { req: SearchReq, stuck: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    priority: SearchPriority,
    route_key: RouteKey,
}

#[derive(Debug, Default)]
pub struct Effects {
    pub planner_cmds: Vec<PlannerCmd>,
    pub ui_cmds: Vec<UiCmd>,
    pub log: Vec<LogEvent>,
}

#[derive(Debug)]
pub struct SearchPlanner {
    slot: Slot,
    pending: Option<Pending>,
    next_req_seq: u64,
    deadline_ms: u64,
}

impl SearchPlanner {
    pub fn new(deadline_ms: u64) -> Self {
        Self {
            slot: Slot::Idle,
            pending: None,
            next_req_seq: 1,
            deadline_ms,
        }
    }

    pub fn slot(&self) -> &Slot {
        &self.slot
    }

    /// Enqueue a search request into the single priority queue.
    pub fn request(
        &mut self,
        now: u64,
        priority: SearchPriority,
        route_key: RouteKey,
        fx: &mut Effects,
    ) {
        let new = Pending {
            priority,
            route_key,
        };
        match &self.slot {
            Slot::Idle => {
                self.pending = Some(new);
                self.start_pending(now, fx);
            }
            Slot::Running { req, stuck } if req.priority < priority => {
                let (req, stuck) = (req.clone(), *stuck);
                fx.planner_cmds.push(PlannerCmd::Cancel {
                    req_id: req.req_id.clone(),
                });
                if priority == SearchPriority::Reroute {
                    fx.ui_cmds.push(UiCmd::ShowCheckingRoute);
                }
                log(fx, now, "search_cancel_requested", Some(&req.req_id), None);
                self.slot = Slot::CancelRequested { req, stuck };
                self.keep_higher(new);
            }
            _ => self.keep_higher(new),
        }
    }

    fn keep_higher(&mut self, new: Pending) {
        // Same or higher priority replaces (newest wins); lower is dropped.
        // TODO(abi-v1): confirm drop vs. queue for lower-priority requests.
        match &self.pending {
            Some(p) if p.priority > new.priority => {}
            _ => self.pending = Some(new),
        }
    }

    fn start_pending(&mut self, now: u64, fx: &mut Effects) {
        let Some(p) = self.pending.take() else { return };
        let req_id = format!("r{}", self.next_req_seq); // TODO(abi-v1): req_id format provisional
        self.next_req_seq += 1;
        fx.planner_cmds.push(PlannerCmd::Start {
            req_id: req_id.clone(),
            priority: p.priority,
            graph_version: p.route_key.graph_version.clone(),
            route_key: p.route_key.clone(),
        });
        log(fx, now, "search_started", Some(&req_id), None);
        self.slot = Slot::Running {
            req: SearchReq {
                req_id,
                priority: p.priority,
                route_key: p.route_key,
                started_at_mono_ms: now,
            },
            stuck: false,
        };
    }

    /// Handle the single terminal event.
    #[allow(clippy::too_many_arguments)]
    pub fn on_search_finished(
        &mut self,
        at: u64,
        req_id: &str,
        route_key: &RouteKey,
        outcome: SearchOutcome,
        comparison_ref: Option<&str>,
        current_route_key: Option<&RouteKey>,
        fx: &mut Effects,
    ) {
        let (req, cancelled, stuck) = match &self.slot {
            Slot::Running { req, stuck } if req.req_id == req_id => (req.clone(), false, *stuck),
            Slot::CancelRequested { req, stuck } if req.req_id == req_id => {
                (req.clone(), true, *stuck)
            }
            _ => {
                log(fx, at, "search_finished_unknown_req", Some(req_id), None);
                return;
            }
        };
        match outcome {
            SearchOutcome::Ok if cancelled => log(fx, at, "discarded_late", Some(req_id), None),
            // TODO(abi-v1): a result that returns after engine_stuck is discarded (not specified).
            SearchOutcome::Ok if stuck => log(fx, at, "discarded_after_stuck", Some(req_id), None),
            SearchOutcome::Ok
                if Some(route_key) != current_route_key || *route_key != req.route_key =>
            {
                log(fx, at, "discarded_stale_route_key", Some(req_id), None)
            }
            SearchOutcome::Ok => {
                fx.ui_cmds.push(UiCmd::SearchResultReady {
                    req_id: req_id.to_owned(),
                    comparison_ref: comparison_ref.map(str::to_owned),
                });
                log(fx, at, "search_adopted", Some(req_id), None);
            }
            other => log(
                fx,
                at,
                "search_ended",
                Some(req_id),
                Some(format!("{other:?}").to_lowercase()),
            ),
        }
        self.slot = Slot::Idle;
        self.start_pending(at, fx);
    }

    pub fn on_engine_reset(&mut self, at: u64, fx: &mut Effects) {
        // Never resumes on its own (#54): the slot is freed only by search_finished.
        log(fx, at, "engine_reset", None, None);
    }

    /// Deadline check driven by `now_mono_ms` (Rust has no timer).
    pub fn on_now(&mut self, now: u64, fx: &mut Effects) {
        let deadline = self.deadline_ms;
        let (Slot::Running { req, stuck } | Slot::CancelRequested { req, stuck }) = &mut self.slot
        else {
            return;
        };
        if !*stuck && now >= req.started_at_mono_ms.saturating_add(deadline) {
            *stuck = true;
            let id = req.req_id.clone();
            fx.ui_cmds.push(UiCmd::ShowCheckingRoute);
            log(fx, now, "engine_stuck", Some(&id), None);
        }
    }

    /// Next time Kotlin must call dc_tick for the planner (deadline), if any.
    pub fn next_wake(&self) -> Option<u64> {
        match &self.slot {
            Slot::Running { req, stuck: false } | Slot::CancelRequested { req, stuck: false } => {
                Some(req.started_at_mono_ms.saturating_add(self.deadline_ms))
            }
            _ => None,
        }
    }
}

fn log(fx: &mut Effects, at: u64, what: &str, req_id: Option<&str>, detail: Option<String>) {
    fx.log.push(LogEvent {
        at_mono_ms: U64Str(at),
        what: what.to_owned(),
        req_id: req_id.map(str::to_owned),
        detail,
    });
}
