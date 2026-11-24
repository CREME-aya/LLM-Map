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
//! - Distance class (team decision 2026-11-24): `begin_plan` fixes `short | long` from the
//!   pre-search ETA estimate (`< 1800 s` → short; `>= 1800 s` or none → long) for every
//!   candidate search of that plan_request. It selects the comparison budget (short 3 s /
//!   long 8 s from the plan start) and the watchdog limit (short 5 s / long 10 s from each
//!   `start`). Searches requested outside any plan are `long`.

use contracts::tick::{DistanceClass, LogEvent, PlannerCmd, SearchOutcome, SearchPriority, UiCmd};
use contracts::RouteKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReq {
    pub req_id: String,
    pub priority: SearchPriority,
    pub route_key: RouteKey,
    pub distance_class: DistanceClass,
    pub started_at_mono_ms: u64,
}

/// Time limits per distance class (03 §3.1, 07 §1; provisional initial values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchLimits {
    /// Native watchdog limit, counted from each `start` (→ `engine_stuck`).
    pub watchdog_short_ms: u64,
    pub watchdog_long_ms: u64,
    /// Comparison budget, counted from the plan start.
    pub comparison_budget_short_ms: u64,
    pub comparison_budget_long_ms: u64,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            watchdog_short_ms: 5_000,
            watchdog_long_ms: 10_000,
            comparison_budget_short_ms: 3_000,
            comparison_budget_long_ms: 8_000,
        }
    }
}

impl SearchLimits {
    pub fn watchdog_ms(&self, c: DistanceClass) -> u64 {
        match c {
            DistanceClass::Short => self.watchdog_short_ms,
            DistanceClass::Long => self.watchdog_long_ms,
        }
    }

    pub fn comparison_budget_ms(&self, c: DistanceClass) -> u64 {
        match c {
            DistanceClass::Short => self.comparison_budget_short_ms,
            DistanceClass::Long => self.comparison_budget_long_ms,
        }
    }
}

/// The current plan_request. Its distance class is fixed at `begin_plan` and never
/// re-evaluated for the candidate searches of the same plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub plan_request_id: String,
    pub distance_class: DistanceClass,
    pub started_at_mono_ms: u64,
    /// Set once the comparison budget has passed (logged once).
    pub budget_expired: bool,
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
    distance_class: DistanceClass,
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
    limits: SearchLimits,
    plan: Option<Plan>,
}

impl SearchPlanner {
    pub fn new(limits: SearchLimits) -> Self {
        Self {
            slot: Slot::Idle,
            pending: None,
            next_req_seq: 1,
            limits,
            plan: None,
        }
    }

    pub fn slot(&self) -> &Slot {
        &self.slot
    }

    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// Start (or continue) a plan_request and fix its distance class before searching.
    /// Calling again with the same `plan_request_id` keeps the class and start time that
    /// were fixed first (a later ETA estimate never reclassifies the same plan).
    pub fn begin_plan(
        &mut self,
        now: u64,
        plan_request_id: &str,
        eta_estimate_s: Option<u64>,
        fx: &mut Effects,
    ) -> DistanceClass {
        if let Some(p) = &self.plan {
            if p.plan_request_id == plan_request_id {
                return p.distance_class;
            }
        }
        let distance_class = DistanceClass::from_eta_estimate_s(eta_estimate_s);
        log(
            fx,
            now,
            "plan_started",
            None,
            Some(format!(
                "plan_request_id={plan_request_id} distance_class={}",
                class_name(distance_class)
            )),
        );
        self.plan = Some(Plan {
            plan_request_id: plan_request_id.to_owned(),
            distance_class,
            started_at_mono_ms: now,
            budget_expired: false,
        });
        distance_class
    }

    /// Comparison budget deadline of the current plan (None once expired / no plan).
    pub fn comparison_deadline(&self) -> Option<u64> {
        self.plan.as_ref().filter(|p| !p.budget_expired).map(|p| {
            p.started_at_mono_ms
                .saturating_add(self.limits.comparison_budget_ms(p.distance_class))
        })
    }

    /// Enqueue a search request into the single priority queue.
    pub fn request(
        &mut self,
        now: u64,
        priority: SearchPriority,
        route_key: RouteKey,
        fx: &mut Effects,
    ) {
        // Class is captured at request time: a later plan never reclassifies it.
        let distance_class = self
            .plan
            .as_ref()
            .map_or(DistanceClass::Long, |p| p.distance_class);
        let new = Pending {
            priority,
            route_key,
            distance_class,
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
            distance_class: p.distance_class,
            graph_version: p.route_key.graph_version.clone(),
            route_key: p.route_key.clone(),
        });
        log(
            fx,
            now,
            "search_started",
            Some(&req_id),
            Some(format!("distance_class={}", class_name(p.distance_class))),
        );
        self.slot = Slot::Running {
            req: SearchReq {
                req_id,
                priority: p.priority,
                route_key: p.route_key,
                distance_class: p.distance_class,
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
    /// a timeout). Deadline = `start` time + watchdog limit of the request's distance class.
    /// Also marks the comparison budget of the current plan as expired.
    pub fn on_now(&mut self, now: u64, fx: &mut Effects) {
        if let Some(budget) = self.comparison_deadline() {
            if now >= budget {
                let plan = self.plan.as_mut().expect("deadline implies plan");
                plan.budget_expired = true;
                // TODO(drive-core): return the comparison from the candidates completed so
                // far (dc_rank) and drop this plan's queued comparison searches.
                let detail = format!("plan_request_id={}", plan.plan_request_id);
                log(fx, now, "comparison_budget_expired", None, Some(detail));
            }
        }
        let Some(deadline) = self.watchdog_deadline() else {
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

    /// Watchdog deadline of the running search, if any. None once stuck: only the real
    /// `search_finished` can change the slot then.
    pub fn watchdog_deadline(&self) -> Option<u64> {
        match &self.slot {
            Slot::Running { req } | Slot::CancelRequested { req } => Some(
                req.started_at_mono_ms
                    .saturating_add(self.limits.watchdog_ms(req.distance_class)),
            ),
            _ => None,
        }
    }

    /// Earliest deadline Kotlin must honour via `next_wake_mono_ms`: the watchdog deadline
    /// or the comparison budget of the current plan, whichever comes first.
    pub fn next_wake(&self) -> Option<u64> {
        match (self.watchdog_deadline(), self.comparison_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

fn class_name(c: DistanceClass) -> &'static str {
    match c {
        DistanceClass::Short => "short",
        DistanceClass::Long => "long",
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
