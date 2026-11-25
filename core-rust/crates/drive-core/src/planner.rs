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
//!   `start`).
//! - Distance class outside a plan (reroute / avoid chip; team decision 2026-11-25): fixed
//!   when the search *starts* from the valid remaining ETA on the current route
//!   (`set_route_eta`): `< 1800 s` → short; `>= 1800 s`, unknown or invalidated (e.g. by
//!   off-route, `invalidate_route_eta`) → long. The latest plan's class is never inherited.
//!   Only the watchdog limit (5 s / 10 s) applies to these searches; the comparison budget
//!   (3 s / 8 s) is for plans only. The avoid chip's own compute budget (1.5 s) and the
//!   20 s-before-branch deadline are separate and not handled here.
//! - Plan queue (team decision 2026-11-25): `begin_plan_with` queues the plan's candidate
//!   searches (baseline `explicit_plan`, the rest `comparison`); they share the single slot
//!   with ad-hoc requests and start one at a time. The plan ends with exactly one
//!   `rank{plan_request_id, facts_refs, reason}` (Kotlin then calls `dc_rank`):
//!   `all_done` once every queued search returned, or `budget_expired` when the comparison
//!   budget passes first. On expiry the queued searches are dropped (never started) and a
//!   running one of the plan gets `cancel`; it keeps the slot until its `search_finished`
//!   (an `ok` then is `discarded_late`). No `start` is emitted for an ended plan.
//! - Commit (team decision 2026-11-25): a `reroute` / ad-hoc `explicit_plan` search that
//!   returns `ok` while still valid (not cancelled / stuck, `route_key` equal to both its
//!   start key and the current route) yields `commit{req_id, route_key}` (the key at the
//!   search start); Kotlin then calls `dc_route_commit` with that search's route body. An
//!   avoid-chip `ok` is never committed here (it goes through `dc_avoid_offer_eval` and
//!   consent). "経路を確認中" is shown through `horizon.status`, not a UiCmd per result.
//!   The commit additionally needs the session to be following the vehicle with
//!   confidence (`CommitGate`: `uncertain` holds the result, `finished` drops it).
//! - Hold while `uncertain` (04 §3): a committable `reroute` / ad-hoc `explicit_plan` ok
//!   that returns while the session is `uncertain` (`CommitGate::Hold`) is not
//!   discarded but held (one at most; a newer committable ok replaces it,
//!   `discarded_hold_replaced`). Once the session may commit again (status `tracking` /
//!   `off_route` / `rerouting`), `release_held` issues its `commit` if its start-time
//!   `route_key` is still the current route (`discarded_stale_route_key` otherwise). After
//!   `commit_hold_ms` (10 s, provisional) it is dropped as `discarded_hold_expired`.
//!   TODO(04 §3 / Gate 7): the hold limit and whether an expired hold restarts the reroute
//!   are undecided; nothing is restarted here (a still-confirmed off-route requests a
//!   new reroute on the next fix, as before).
//! - `engine_stuck` emits no UiCmd (docs 03 §3.1, `show_checking_route` is abolished): a
//!   stuck reroute keeps `horizon.status = rerouting` (the slot stays occupied); a stuck
//!   avoid-chip search leaves the status unchanged and is only logged, and no new avoid
//!   chip can start until the slot is freed.

use contracts::tick::{
    DistanceClass, Fix, LogEvent, PlannerCmd, RankReason, SearchOutcome, SearchPriority, UiCmd,
};
use contracts::RouteKey;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReq {
    pub req_id: String,
    pub priority: SearchPriority,
    pub route_key: RouteKey,
    pub distance_class: DistanceClass,
    pub started_at_mono_ms: u64,
    /// Set for a candidate search of a plan.
    pub plan_request_id: Option<String>,
    pub candidate_id: Option<String>,
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
    /// How long a result returned while `uncertain` is held for a later commit
    /// (04 §3; 10 s, provisional — not in 07 §1).
    pub commit_hold_ms: u64,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            watchdog_short_ms: 5_000,
            watchdog_long_ms: 10_000,
            comparison_budget_short_ms: 3_000,
            comparison_budget_long_ms: 8_000,
            commit_hold_ms: 10_000,
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
    /// Key every candidate search of the plan is started with.
    pub route_key: Option<RouteKey>,
    /// Candidate searches not started yet, in request order.
    pub queue: VecDeque<PlanSearch>,
    /// `facts_ref` of each candidate search that returned `ok`, in request order.
    pub facts_refs: Vec<String>,
    /// Set once `rank` was emitted (or the plan was superseded): nothing more starts.
    pub ended: bool,
}

/// A queued candidate search of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSearch {
    pub candidate_id: String,
    pub priority: SearchPriority,
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
    /// Current position for a reroute (`start.fix`).
    fix: Option<Fix>,
}

/// Whether a committable `ok` may be committed now (decided by the session from
/// `horizon.status`, 04 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitGate {
    /// `tracking` / `off_route` / `rerouting` (or no geometry yet): commit.
    Allowed,
    /// `uncertain`: hold until the status allows a commit again.
    Hold,
    /// `finished`: discard (`discarded_not_tracking`).
    Denied,
}

/// A committable result returned while the session could not commit (04 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldResult {
    pub req_id: String,
    pub priority: SearchPriority,
    /// The current route's key when the search started (the `commit` key).
    pub route_key: RouteKey,
    pub held_at_mono_ms: u64,
}

/// Remaining ETA on a route, as last reported. Valid only for that `route_key` and until
/// invalidated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteEta {
    route_key: RouteKey,
    remaining_eta_s: u64,
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
    route_eta: Option<RouteEta>,
    held: Option<HeldResult>,
}

impl SearchPlanner {
    pub fn new(limits: SearchLimits) -> Self {
        Self {
            slot: Slot::Idle,
            pending: None,
            next_req_seq: 1,
            limits,
            plan: None,
            route_eta: None,
            held: None,
        }
    }

    pub fn held(&self) -> Option<&HeldResult> {
        self.held.as_ref()
    }

    /// Last instant (inclusive) the held result may still be committed.
    fn hold_deadline(&self) -> Option<u64> {
        self.held
            .as_ref()
            .map(|h| h.held_at_mono_ms.saturating_add(self.limits.commit_hold_ms))
    }

    fn replace_held(&mut self, at: u64, new: Option<HeldResult>, fx: &mut Effects) {
        if let Some(old) = self.held.take() {
            log(fx, at, "discarded_hold_replaced", Some(&old.req_id), None);
        }
        self.held = new;
    }

    /// Called at the end of every tick: drops an expired hold, or issues the held
    /// `commit` once the session may commit again and the route has not changed.
    pub fn release_held(
        &mut self,
        now: u64,
        gate: CommitGate,
        current_route_key: Option<&RouteKey>,
        fx: &mut Effects,
    ) {
        let Some(deadline) = self.hold_deadline() else {
            return;
        };
        if now > deadline {
            let h = self.held.take().expect("deadline implies hold");
            log(fx, now, "discarded_hold_expired", Some(&h.req_id), None);
            return;
        }
        match gate {
            CommitGate::Hold => return,
            CommitGate::Denied => {
                let h = self.held.take().expect("deadline implies hold");
                log(
                    fx,
                    now,
                    "discarded_not_tracking",
                    Some(&h.req_id),
                    Some("from_hold".into()),
                );
                return;
            }
            CommitGate::Allowed => {}
        }
        let h = self.held.take().expect("deadline implies hold");
        if Some(&h.route_key) == current_route_key {
            fx.planner_cmds.push(PlannerCmd::Commit {
                req_id: h.req_id.clone(),
                route_key: h.route_key,
            });
            log(
                fx,
                now,
                "commit_issued",
                Some(&h.req_id),
                Some("from_hold".into()),
            );
        } else {
            log(
                fx,
                now,
                "discarded_stale_route_key",
                Some(&h.req_id),
                Some("from_hold".into()),
            );
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
    /// Queues no candidate search (used by tests and by callers that request searches
    /// themselves); see `begin_plan_with`.
    pub fn begin_plan(
        &mut self,
        now: u64,
        plan_request_id: &str,
        eta_estimate_s: Option<u64>,
        fx: &mut Effects,
    ) -> DistanceClass {
        self.begin_plan_with(now, plan_request_id, eta_estimate_s, None, Vec::new(), fx)
    }

    /// `begin_plan` plus the plan's candidate searches, queued in order and started one at
    /// a time with `route_key`. Same id as the current plan → nothing changes (no re-queue).
    /// A different id supersedes the current plan: its queue is dropped and its running
    /// search is cancelled (no `rank` for a superseded plan).
    pub fn begin_plan_with(
        &mut self,
        now: u64,
        plan_request_id: &str,
        eta_estimate_s: Option<u64>,
        route_key: Option<RouteKey>,
        searches: Vec<PlanSearch>,
        fx: &mut Effects,
    ) -> DistanceClass {
        if let Some(p) = &self.plan {
            if p.plan_request_id == plan_request_id {
                return p.distance_class;
            }
            if !p.ended {
                let detail = format!("plan_request_id={}", p.plan_request_id);
                log(fx, now, "plan_superseded", None, Some(detail));
                self.end_plan(now, fx);
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
        let head = searches.first().map(|s| s.priority);
        self.plan = Some(Plan {
            plan_request_id: plan_request_id.to_owned(),
            distance_class,
            started_at_mono_ms: now,
            route_key,
            queue: searches.into(),
            facts_refs: Vec::new(),
            ended: false,
        });
        if let Some(priority) = head {
            self.preempt_or_start(now, priority, fx);
        }
        distance_class
    }

    /// Ends the current plan: drops its queued searches and cancels its running one.
    fn end_plan(&mut self, now: u64, fx: &mut Effects) {
        let Some(plan) = self.plan.as_mut() else {
            return;
        };
        plan.ended = true;
        for s in plan.queue.drain(..) {
            log(
                fx,
                now,
                "plan_search_dropped",
                None,
                Some(format!("candidate_id={}", s.candidate_id)),
            );
        }
        let id = plan.plan_request_id.clone();
        let of_plan = |r: &SearchReq| r.plan_request_id.as_deref() == Some(id.as_str());
        match &self.slot {
            Slot::Running { req } if of_plan(req) => {
                let req = req.clone();
                Self::send_cancel(now, &req, fx);
                self.slot = Slot::CancelRequested { req };
            }
            Slot::EngineStuck {
                req,
                cancel_sent: false,
            } if of_plan(req) => {
                let req = req.clone();
                Self::send_cancel(now, &req, fx);
                self.slot = Slot::EngineStuck {
                    req,
                    cancel_sent: true,
                };
            }
            _ => {}
        }
    }

    /// Emits the plan's single `rank` and ends it.
    fn rank(&mut self, now: u64, reason: RankReason, fx: &mut Effects) {
        let Some(plan) = self.plan.as_ref().filter(|p| !p.ended) else {
            return;
        };
        fx.planner_cmds.push(PlannerCmd::Rank {
            plan_request_id: plan.plan_request_id.clone(),
            facts_refs: plan.facts_refs.clone(),
            reason,
        });
        let detail = format!(
            "plan_request_id={} reason={reason:?} facts={}",
            plan.plan_request_id,
            plan.facts_refs.len()
        );
        log(fx, now, "plan_ranked", None, Some(detail));
        self.end_plan(now, fx);
    }

    fn active_plan(&self, plan_request_id: &str) -> bool {
        self.plan
            .as_ref()
            .is_some_and(|p| !p.ended && p.plan_request_id == plan_request_id)
    }

    /// Comparison budget deadline of the current plan (None once expired / no plan).
    pub fn comparison_deadline(&self) -> Option<u64> {
        self.plan.as_ref().filter(|p| !p.ended).map(|p| {
            p.started_at_mono_ms
                .saturating_add(self.limits.comparison_budget_ms(p.distance_class))
        })
    }

    /// Records the remaining ETA on `route_key` (the current route). Used for the distance
    /// class of searches outside a plan when they start.
    pub fn set_route_eta(&mut self, route_key: RouteKey, remaining_eta_s: u64) {
        self.route_eta = Some(RouteEta {
            route_key,
            remaining_eta_s,
        });
    }

    /// The remaining ETA is no longer valid (off-route etc.): searches outside a plan
    /// started from now on are `long` until a new ETA is set.
    pub fn invalidate_route_eta(&mut self) {
        self.route_eta = None;
    }

    /// Distance class of a search outside a plan on `route_key`, decided at its start:
    /// the valid remaining ETA on that route `< 1800 s` → short, otherwise long.
    pub fn adhoc_distance_class(&self, route_key: &RouteKey) -> DistanceClass {
        let eta = self
            .route_eta
            .as_ref()
            .filter(|e| e.route_key == *route_key)
            .map(|e| e.remaining_eta_s);
        DistanceClass::from_eta_estimate_s(eta)
    }

    /// Enqueue a search request into the single priority queue. `fix` is the current
    /// position handed to a reroute (`start.fix`).
    pub fn request(
        &mut self,
        now: u64,
        priority: SearchPriority,
        route_key: RouteKey,
        fix: Option<Fix>,
        fx: &mut Effects,
    ) {
        self.keep_higher(Pending {
            priority,
            route_key,
            fix,
        });
        self.preempt_or_start(now, priority, fx);
    }

    /// A search of `priority` is waiting: start it if the slot is free, or cancel a
    /// lower-priority running search (it keeps the slot until its `search_finished`).
    fn preempt_or_start(&mut self, now: u64, priority: SearchPriority, fx: &mut Effects) {
        match &self.slot {
            Slot::Idle => self.start_next(now, fx),
            Slot::Running { req } if req.priority < priority => {
                let req = req.clone();
                Self::send_cancel(now, &req, fx);
                self.slot = Slot::CancelRequested { req };
            }
            Slot::EngineStuck {
                req,
                cancel_sent: false,
            } if req.priority < priority => {
                let req = req.clone();
                Self::send_cancel(now, &req, fx);
                self.slot = Slot::EngineStuck {
                    req,
                    cancel_sent: true,
                };
            }
            _ => {}
        }
    }

    fn send_cancel(now: u64, req: &SearchReq, fx: &mut Effects) {
        fx.planner_cmds.push(PlannerCmd::Cancel {
            req_id: req.req_id.clone(),
        });
        log(fx, now, "search_cancel_requested", Some(&req.req_id), None);
    }

    /// A reroute is waiting or occupies the slot (no second one is requested).
    pub fn has_reroute(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| p.priority == SearchPriority::Reroute)
            || self.reroute_in_slot()
            || self
                .held
                .as_ref()
                .is_some_and(|h| h.priority == SearchPriority::Reroute)
    }

    /// The slot holds a reroute (running, being cancelled or stuck): `horizon.status`
    /// is `rerouting`.
    pub fn reroute_in_slot(&self) -> bool {
        match &self.slot {
            Slot::Idle => false,
            Slot::Running { req }
            | Slot::CancelRequested { req }
            | Slot::EngineStuck { req, .. } => req.priority == SearchPriority::Reroute,
        }
    }

    /// A new route was committed: a waiting request made for another route is dropped.
    pub fn on_route_changed(&mut self, now: u64, route_key: &RouteKey, fx: &mut Effects) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.route_key != *route_key)
        {
            let p = self.pending.take().expect("checked");
            let detail = format!("priority={}", priority_name(p.priority));
            log(fx, now, "pending_dropped_route_changed", None, Some(detail));
        }
        if self
            .held
            .as_ref()
            .is_some_and(|h| h.route_key != *route_key)
        {
            let h = self.held.take().expect("checked");
            let detail = "from_hold".to_owned();
            log(
                fx,
                now,
                "discarded_stale_route_key",
                Some(&h.req_id),
                Some(detail),
            );
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

    /// Starts the highest waiting search (ad-hoc pending or the plan's next candidate; on a
    /// tie the ad-hoc one, which can only be reroute / avoid chip / explicit request).
    fn start_next(&mut self, now: u64, fx: &mut Effects) {
        if self.slot != Slot::Idle {
            return;
        }
        let plan_head = self
            .plan
            .as_ref()
            .filter(|p| !p.ended && p.route_key.is_some())
            .and_then(|p| p.queue.front())
            .map(|s| s.priority);
        let take_plan = match (&self.pending, plan_head) {
            (Some(p), Some(h)) => h > p.priority,
            (None, Some(_)) => true,
            (_, None) => false,
        };
        let (priority, route_key, distance_class, plan_request_id, candidate_id, fix) = if take_plan
        {
            let plan = self.plan.as_mut().expect("plan head implies plan");
            let s = plan.queue.pop_front().expect("plan head");
            (
                s.priority,
                plan.route_key.clone().expect("checked above"),
                plan.distance_class,
                Some(plan.plan_request_id.clone()),
                Some(s.candidate_id),
                None,
            )
        } else {
            let Some(p) = self.pending.take() else { return };
            // Fixed here, at the start, for the whole native call.
            let class = self.adhoc_distance_class(&p.route_key);
            (p.priority, p.route_key, class, None, None, p.fix)
        };
        let req_id = format!("r{}", self.next_req_seq); // TODO(abi-v1): req_id format provisional
        self.next_req_seq += 1;
        fx.planner_cmds.push(PlannerCmd::Start {
            req_id: req_id.clone(),
            priority,
            distance_class,
            graph_version: route_key.graph_version.clone(),
            route_key: route_key.clone(),
            candidate_id: candidate_id.clone(),
            fix,
        });
        let mut detail = format!("distance_class={}", class_name(distance_class));
        if let Some(c) = &candidate_id {
            detail.push_str(&format!(" candidate_id={c}"));
        }
        log(fx, now, "search_started", Some(&req_id), Some(detail));
        self.slot = Slot::Running {
            req: SearchReq {
                req_id,
                priority,
                route_key,
                distance_class,
                started_at_mono_ms: now,
                plan_request_id,
                candidate_id,
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
        facts_ref: Option<&str>,
        current_route_key: Option<&RouteKey>,
        gate: CommitGate,
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
        let plan_id = req.plan_request_id.as_deref();
        let in_active_plan = plan_id.is_some_and(|id| self.active_plan(id));
        match outcome {
            // engine_stuck wins over an earlier cancel (03 §3.1 state machine).
            SearchOutcome::Ok if stuck => log(fx, at, "discarded_after_stuck", Some(req_id), None),
            SearchOutcome::Ok if cancelled => log(fx, at, "discarded_late", Some(req_id), None),
            // A search of a plan that already ranked (or was superseded) only frees the slot.
            SearchOutcome::Ok if plan_id.is_some() && !in_active_plan => {
                log(fx, at, "discarded_late", Some(req_id), None)
            }
            SearchOutcome::Ok if *route_key != req.route_key => {
                log(fx, at, "discarded_stale_route_key", Some(req_id), None)
            }
            // Candidate search: collected for `rank`, never displayed on its own.
            SearchOutcome::Ok if in_active_plan => {
                let plan = self.plan.as_mut().expect("active plan");
                match facts_ref {
                    Some(f) => {
                        plan.facts_refs.push(f.to_owned());
                        log(fx, at, "candidate_collected", Some(req_id), None);
                    }
                    // TODO(abi-v1): an ok without facts_ref is not specified; not ranked.
                    None => log(fx, at, "candidate_without_facts", Some(req_id), None),
                }
            }
            SearchOutcome::Ok if Some(route_key) != current_route_key => {
                log(fx, at, "discarded_stale_route_key", Some(req_id), None)
            }
            // 04 §3 / 03 §3.1: commit only while tracking / off_route / rerouting (the state
            // a reroute was started for). `uncertain` holds the result, `finished` drops it.
            // Decided by the session (`gate`).
            SearchOutcome::Ok
                if gate == CommitGate::Denied
                    && matches!(
                        req.priority,
                        SearchPriority::Reroute | SearchPriority::ExplicitPlan
                    ) =>
            {
                log(fx, at, "discarded_not_tracking", Some(req_id), None)
            }
            SearchOutcome::Ok
                if gate == CommitGate::Hold
                    && matches!(
                        req.priority,
                        SearchPriority::Reroute | SearchPriority::ExplicitPlan
                    ) =>
            {
                let held = HeldResult {
                    req_id: req_id.to_owned(),
                    priority: req.priority,
                    route_key: req.route_key.clone(),
                    held_at_mono_ms: at,
                };
                self.replace_held(at, Some(held), fx);
                log(fx, at, "commit_held_uncertain", Some(req_id), None)
            }
            SearchOutcome::Ok
                if matches!(
                    req.priority,
                    SearchPriority::Reroute | SearchPriority::ExplicitPlan
                ) =>
            {
                self.replace_held(at, None, fx);
                fx.planner_cmds.push(PlannerCmd::Commit {
                    req_id: req_id.to_owned(),
                    route_key: req.route_key.clone(),
                });
                log(fx, at, "commit_issued", Some(req_id), None);
            }
            // TODO(drive-core): avoid chip → dc_avoid_offer_eval / AvoidOffer (not wired).
            SearchOutcome::Ok if req.priority == SearchPriority::AvoidChip => {
                log(fx, at, "avoid_result_ready", Some(req_id), None)
            }
            SearchOutcome::Ok => log(fx, at, "search_result_unused", Some(req_id), None),
            other => log(
                fx,
                at,
                "search_ended",
                Some(req_id),
                Some(format!("{other:?}").to_lowercase()),
            ),
        }
        self.slot = Slot::Idle;
        if in_active_plan && self.plan.as_ref().is_some_and(|p| p.queue.is_empty()) {
            self.rank(at, RankReason::AllDone, fx);
        }
        self.start_next(at, fx);
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
                let plan = self.plan.as_ref().expect("deadline implies plan");
                let detail = format!("plan_request_id={}", plan.plan_request_id);
                log(fx, now, "comparison_budget_expired", None, Some(detail));
                // Return what is complete so far; queued searches are dropped and a running
                // one is cancelled (it keeps the slot until its search_finished).
                self.rank(now, RankReason::BudgetExpired, fx);
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
        // No UiCmd: a stuck reroute stays `rerouting` (the slot is still occupied); a stuck
        // avoid chip / other search leaves horizon.status unchanged, log only.
        let detail = format!("priority={}", priority_name(req.priority));
        log(fx, now, "engine_stuck", Some(&req.req_id), Some(detail));
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
    /// Also the first instant a held result counts as expired.
    pub fn next_wake(&self) -> Option<u64> {
        [
            self.watchdog_deadline(),
            self.comparison_deadline(),
            self.hold_deadline().map(|d| d.saturating_add(1)),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

fn priority_name(p: SearchPriority) -> &'static str {
    match p {
        SearchPriority::Reroute => "reroute",
        SearchPriority::ExplicitPlan => "explicit_plan",
        SearchPriority::AvoidChip => "avoid_chip",
        SearchPriority::Comparison => "comparison",
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
