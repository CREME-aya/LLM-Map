//! Search slot state machine (docs v1.2 03 §3.1, ADR-0008 decision 6).
//!
//! ```text
//! Idle ──start──▶ Running ──cancel──▶ CancelRequested ──search_finished──▶ Idle
//! Running / CancelRequested ──watchdog deadline (now_mono_ms)──▶ EngineStuck
//! EngineStuck ──search_finished──▶ Idle
//! ```
//! - Exactly one terminal `search_finished` per search. The next `start` is never emitted
//!   before it arrives, so at most one search runs at a time *by construction*.
//! - Lower-priority searches are stopped with `cancel` (Kotlin sets the atomic cancel flag);
//!   re-search starts when the native call has returned (= `search_finished`).
//! - `ok` after `cancel` is discarded (`discarded_late`), it only frees the slot.
//! - The watchdog deadline is detected here from `now_mono_ms` and the `start` time; Kotlin
//!   never reports a timeout (there is no `timeout` outcome). On expiry → `engine_stuck`:
//!   the slot stays occupied until the call physically returns; an `ok` returned then is
//!   discarded (`discarded_after_stuck`). `engine_reset` alone never resumes searching.

use contracts::tick::{LogEvent, PlannerCmd, SearchOutcome, SearchPriority, UiCmd};
use contracts::RouteKey;

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
    Running {
        req: SearchReq,
    },
    CancelRequested {
        req: SearchReq,
    },
    /// Watchdog deadline passed while the native call had not returned. The slot stays
    /// occupied until the real `search_finished`. `cancel_sent` avoids duplicate cancels.
    EngineStuck {
        req: SearchReq,
        cancel_sent: bool,
    },
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
            Slot::Running { req } if req.priority < priority => {
                let req = req.clone();
                Self::send_cancel(now, &req, priority, fx);
                self.slot = Slot::CancelRequested { req };
                self.keep_higher(new);
            }
            Slot::EngineStuck {
                req,
                cancel_sent: false,
            } if req.priority < priority => {
                let req = req.clone();
                Self::send_cancel(now, &req, priority, fx);
                self.slot = Slot::EngineStuck {
                    req,
                    cancel_sent: true,
                };
                self.keep_higher(new);
            }
            _ => self.keep_higher(new),
        }
    }

    fn send_cancel(now: u64, req: &SearchReq, by: SearchPriority, fx: &mut Effects) {
        fx.planner_cmds.push(PlannerCmd::Cancel {
            req_id: req.req_id.clone(),
        });
        if by == SearchPriority::Reroute {
            fx.ui_cmds.push(UiCmd::ShowCheckingRoute);
        }
        log(fx, now, "search_cancel_requested", Some(&req.req_id), None);
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
            Slot::Running { req } if req.req_id == req_id => (req.clone(), false, false),
            Slot::CancelRequested { req } if req.req_id == req_id => (req.clone(), true, false),
            Slot::EngineStuck { req, .. } if req.req_id == req_id => (req.clone(), false, true),
            _ => {
                log(fx, at, "search_finished_unknown_req", Some(req_id), None);
                return;
            }
        };
        match outcome {
            // engine_stuck wins over an earlier cancel (03 §3.1 state machine).
            SearchOutcome::Ok if stuck => log(fx, at, "discarded_after_stuck", Some(req_id), None),
            SearchOutcome::Ok if cancelled => log(fx, at, "discarded_late", Some(req_id), None),
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
        // Never resumes on its own (03 §3.1): the slot is freed only by search_finished.
        log(fx, at, "engine_reset", None, None);
    }

    /// Watchdog check driven by `now_mono_ms` (Rust has no timer; Kotlin never reports
    /// a timeout). Deadline = `start` time + watchdog limit.
    pub fn on_now(&mut self, now: u64, fx: &mut Effects) {
        let Some(deadline) = self.next_wake() else {
            return;
        };
        if now < deadline {
            return;
        }
        let (req, cancel_sent) = match &self.slot {
            Slot::Running { req } => (req.clone(), false),
            Slot::CancelRequested { req } => (req.clone(), true),
            _ => return,
        };
        fx.ui_cmds.push(UiCmd::ShowCheckingRoute);
        log(fx, now, "engine_stuck", Some(&req.req_id), None);
        self.slot = Slot::EngineStuck { req, cancel_sent };
    }

    /// Watchdog deadline Kotlin must honour via `next_wake_mono_ms`, if any. None once
    /// stuck: only the real `search_finished` can change the slot then.
    pub fn next_wake(&self) -> Option<u64> {
        match &self.slot {
            Slot::Running { req } | Slot::CancelRequested { req } => {
                Some(req.started_at_mono_ms.saturating_add(self.deadline_ms))
            }
            _ => None,
        }
    }
}

fn log(fx: &mut Effects, at: u64, what: &str, req_id: Option<&str>, detail: Option<String>) {
    fx.log.push(LogEvent {
        at_mono_ms: at,
        what: what.to_owned(),
        req_id: req_id.map(str::to_owned),
        detail,
    });
}
