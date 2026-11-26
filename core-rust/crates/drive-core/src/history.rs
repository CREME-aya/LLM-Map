//! P3 history commentary state machine (docs 05 §6.2, ADR-0008). Integer-only.
//!
//! State lifecycle per POI on the current route:
//! ```text
//! NotReached ──▶ Eligible ──▶ Pending ──▶ Speaking ──▶ Completed
//!                   │            │           └───────▶ Interrupted (Stop + save_for_later)
//!                   └────────────┴───────────────────▶ Expired
//! ```
//!
//! Session-scoped state that persists across route changes (`dc_route_commit`):
//! - `consent`: defaults to OFF (`false`), updated by `EventBody::HistoryConsent`.
//! - `spoken_pois`: each `poi_id` may be started at most once per trip.
//! - `last_started_at_mono_ms`: minimum 5-minute interval between P3 starts.
//! - `last_bmk_target`: last `Completed` or `Interrupted` POI (for L3 `BMK` within 60 s).
//! - Platform/audio flags (`call_active`, `audio_focus_lost`, active non-P3 TTS tokens).
//!
//! All numeric thresholds are provisional and configured via `SessionConfig` (05 §6.2).
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use contracts::horizon::{ComplexWindow, HorizonStatus};
use contracts::route::{HistoryPoi, HistoryVersion, HistoryVersionKind};
use contracts::tick::{LogEvent, SaveForLaterReason, UiCmd, VoiceCmd};
use contracts::RouteKey;
use std::collections::BTreeSet;

/// Provisional dictionary words for stopping P3 history commentary (05 §3.2 / §6.2).
pub const DICT_STOP: &[&str] = &["やめて", "止めて", "とめて", "キャンセル", "ストップ"];

/// True when `dict_norm` is a stop phrase for P3 history commentary.
pub fn is_dict_stop(norm: &str) -> bool {
    DICT_STOP.contains(&norm.trim())
}

/// Provisional P3 history commentary thresholds (05 §6.2; passed via `SessionConfig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryConfig {
    /// Far boundary of the Eligible corridor ahead of the POI (cm; 2 km = 200_000).
    pub corridor_far_cm: u32,
    /// Near boundary of the Eligible corridor ahead of the POI (cm; 0.5 km = 50_000).
    pub corridor_near_cm: u32,
    /// Maximum duration a POI may wait in `Pending` before expiring (ms; 30 s).
    pub pending_timeout_ms: u64,
    /// Minimum interval between P3 utterance starts (ms; 5 min = 300_000).
    pub min_interval_ms: u64,
    /// Estimated TTS preparation time (ms; 800 ms, provisional until Gate 4).
    pub tts_prep_ms: u64,
    /// Safety margin before next P1 deadline / complex window (ms; 5 s = 5_000).
    pub safety_margin_ms: u64,
    /// Window after `Completed` / `Interrupted` during which `BMK` targets the POI (ms; 60 s).
    pub bmk_window_ms: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            corridor_far_cm: 200_000,
            corridor_near_cm: 50_000,
            pending_timeout_ms: 30_000,
            min_interval_ms: 300_000,
            tts_prep_ms: 800,
            safety_margin_ms: 5_000,
            bmk_window_ms: 60_000,
        }
    }
}

/// Runtime state of a single POI on the current route (05 §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoiState {
    NotReached,
    Eligible,
    Pending {
        since_mono_ms: u64,
    },
    Speaking {
        token: String,
        started_at_mono_ms: u64,
    },
    Completed,
    Interrupted,
    Expired,
}

#[derive(Debug, Clone)]
struct PoiEntry {
    poi: HistoryPoi,
    state: PoiState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BmkTarget {
    poi_id: String,
    ended_at_mono_ms: u64,
}

/// Snapshot of the current tick's horizon and vehicle/device state needed for P3 decisions.
pub(crate) struct StepContext<'a> {
    pub now: u64,
    pub route_key: Option<&'a RouteKey>,
    pub status: Option<HorizonStatus>,
    pub progress_cm: Option<u32>,
    pub next_p1_deadline_mono_ms: Option<u64>,
    pub next_complex_window: Option<&'a ComplexWindow>,
    pub lateral_ok: bool,
    pub acc_and_bearing_ok: bool,
    pub thermal_ok: bool,
    pub has_p0_or_p1_speak: bool,
    pub has_any_other_speak: bool,
}

/// P3 history commentary scheduler held by `Session`.
#[derive(Debug, Clone)]
pub struct HistoryScheduler {
    cfg: HistoryConfig,
    /// Consent persists across route changes (default OFF, 05 §6.2).
    consent: bool,
    /// POIs already started in this trip (once per trip, persists across route changes).
    spoken_pois: BTreeSet<String>,
    /// Start timestamp of the last P3 utterance (5-min interval, persists across route changes).
    last_started_at_mono_ms: Option<u64>,
    /// Last `Completed` or `Interrupted` POI for the 60 s `BMK` window.
    last_bmk_target: Option<BmkTarget>,
    /// Monotonic utterance counter so P3 tokens never collide across routes.
    token_seq: u64,
    /// Tokens issued by this P3 scheduler (so late `tts_*` callbacks are recognized as P3).
    p3_tokens: BTreeSet<String>,
    /// Non-P3 tokens emitted by Rust at P2 (so their `tts_started` does not preempt P3).
    p2_tokens: BTreeSet<String>,
    /// Active non-P3 TTS tokens (`tts_started` received, not yet `tts_done` / `tts_stopped`).
    other_tts_tokens: BTreeSet<String>,
    /// Phone call active (`call_state{active}`).
    call_active: bool,
    /// Audio focus lost (`audio_focus_lost`).
    audio_focus_lost: bool,
    /// POIs on the current route, ordered by `along_route_cm`.
    entries: Vec<PoiEntry>,
    /// Wake hint for a complex window that is currently blocking a `Pending` POI.
    blocked_window_wake_ms: Option<u64>,
    /// Wake hint for the next P1 deadline while a POI is `Speaking`.
    speaking_p1_wake_ms: Option<u64>,
}

impl HistoryScheduler {
    pub fn new(cfg: HistoryConfig) -> Self {
        Self {
            cfg,
            consent: false,
            spoken_pois: BTreeSet::new(),
            last_started_at_mono_ms: None,
            last_bmk_target: None,
            token_seq: 0,
            p3_tokens: BTreeSet::new(),
            p2_tokens: BTreeSet::new(),
            other_tts_tokens: BTreeSet::new(),
            call_active: false,
            audio_focus_lost: false,
            entries: Vec::new(),
            blocked_window_wake_ms: None,
            speaking_p1_wake_ms: None,
        }
    }

    pub fn config(&self) -> HistoryConfig {
        self.cfg
    }

    pub fn consent(&self) -> bool {
        self.consent
    }

    pub fn poi_state(&self, poi_id: &str) -> Option<&PoiState> {
        self.entries
            .iter()
            .find(|e| e.poi.poi_id == poi_id)
            .map(|e| &e.state)
    }

    pub fn has_spoken(&self, poi_id: &str) -> bool {
        self.spoken_pois.contains(poi_id)
    }

    pub fn spoken_count(&self) -> usize {
        self.spoken_pois.len()
    }

    /// Record a Rust-emitted P2 token so its `tts_started` is tracked as busy audio
    /// without treating it as a P0/P1 interruption.
    pub fn note_p2_token(&mut self, token: &str) {
        self.p2_tokens.insert(token.to_owned());
    }

    /// Route changed (`dc_route_commit` succeeded): replace the POI list, preserving consent,
    /// once-per-trip spoken history, 5-minute interval, and BMK target (05 §6.2).
    pub fn on_route_changed(
        &mut self,
        now: u64,
        pois: &[HistoryPoi],
        voice_cmds: &mut Vec<VoiceCmd>,
        ui_cmds: &mut Vec<UiCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        self.interrupt_speaking(now, "route_changed", voice_cmds, ui_cmds, log);
        self.blocked_window_wake_ms = None;
        self.speaking_p1_wake_ms = None;
        self.entries = pois
            .iter()
            .cloned()
            .map(|poi| PoiEntry {
                poi,
                state: PoiState::NotReached,
            })
            .collect();
    }

    /// `EventBody::HistoryConsent { on }`.
    /// When OFF: expire all Eligible / Pending POIs, and stop any Speaking POI WITHOUT
    /// emitting `save_for_later` (05 §6.2).
    pub fn on_consent(
        &mut self,
        at: u64,
        on: bool,
        voice_cmds: &mut Vec<VoiceCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        if self.consent != on {
            log.push(log_at(
                at,
                "history_consent_changed",
                Some(format!("on={on}")),
            ));
            self.consent = on;
        }
        if !on {
            for e in &mut self.entries {
                match &e.state {
                    PoiState::Eligible | PoiState::Pending { .. } => {
                        e.state = PoiState::Expired;
                        log.push(log_at(
                            at,
                            "p3_expired",
                            Some(format!("poi_id={} reason=consent_off", e.poi.poi_id)),
                        ));
                    }
                    PoiState::Speaking { token, .. } => {
                        let tok = token.clone();
                        e.state = PoiState::Expired;
                        voice_cmds.push(VoiceCmd::Stop { token: tok });
                        log.push(log_at(
                            at,
                            "p3_expired",
                            Some(format!("poi_id={} reason=consent_off", e.poi.poi_id)),
                        ));
                    }
                    _ => {}
                }
            }
            self.blocked_window_wake_ms = None;
            self.speaking_p1_wake_ms = None;
        }
    }

    /// `EventBody::TtsStarted { token }`.
    pub fn on_tts_started(
        &mut self,
        at: u64,
        token: &str,
        voice_cmds: &mut Vec<VoiceCmd>,
        ui_cmds: &mut Vec<UiCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        if self.p3_tokens.contains(token) {
            return;
        }
        self.other_tts_tokens.insert(token.to_owned());
        if !self.p2_tokens.contains(token) {
            self.interrupt_speaking(at, "external_tts_started", voice_cmds, ui_cmds, log);
        }
    }

    /// `EventBody::TtsDone { token }`.
    pub fn on_tts_done(&mut self, at: u64, token: &str, log: &mut Vec<LogEvent>) {
        self.other_tts_tokens.remove(token);
        for e in &mut self.entries {
            if matches!(&e.state, PoiState::Speaking { token: t, .. } if t == token) {
                e.state = PoiState::Completed;
                self.last_bmk_target = Some(BmkTarget {
                    poi_id: e.poi.poi_id.clone(),
                    ended_at_mono_ms: at,
                });
                self.speaking_p1_wake_ms = None;
                log.push(log_at(
                    at,
                    "p3_completed",
                    Some(format!("poi_id={}", e.poi.poi_id)),
                ));
                break;
            }
        }
    }

    /// `EventBody::TtsStopped { token }`.
    pub fn on_tts_stopped(&mut self, at: u64, token: &str, log: &mut Vec<LogEvent>) {
        self.other_tts_tokens.remove(token);
        for e in &mut self.entries {
            if matches!(&e.state, PoiState::Speaking { token: t, .. } if t == token) {
                e.state = PoiState::Expired;
                self.speaking_p1_wake_ms = None;
                log.push(log_at(
                    at,
                    "p3_expired",
                    Some(format!("poi_id={} reason=tts_stopped", e.poi.poi_id)),
                ));
                break;
            }
        }
    }

    /// `EventBody::CallState { active }`.
    pub fn on_call_state(
        &mut self,
        at: u64,
        active: bool,
        voice_cmds: &mut Vec<VoiceCmd>,
        ui_cmds: &mut Vec<UiCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        self.call_active = active;
        if active {
            self.interrupt_speaking(at, "call_active", voice_cmds, ui_cmds, log);
        }
    }

    /// `EventBody::AudioFocusLost {}`.
    pub fn on_audio_focus_lost(
        &mut self,
        at: u64,
        voice_cmds: &mut Vec<VoiceCmd>,
        ui_cmds: &mut Vec<UiCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        self.audio_focus_lost = true;
        self.interrupt_speaking(at, "audio_focus_lost", voice_cmds, ui_cmds, log);
    }

    /// User explicitly requested stop (e.g. dict `"やめて"` / `"キャンセル"`): expires
    /// Speaking / Pending without `save_for_later` (05 §6.2).
    pub fn on_user_stop(
        &mut self,
        at: u64,
        voice_cmds: &mut Vec<VoiceCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        for e in &mut self.entries {
            match &e.state {
                PoiState::Speaking { token, .. } => {
                    let tok = token.clone();
                    e.state = PoiState::Expired;
                    voice_cmds.push(VoiceCmd::Stop { token: tok });
                    self.speaking_p1_wake_ms = None;
                    log.push(log_at(
                        at,
                        "p3_expired",
                        Some(format!("poi_id={} reason=user_stop", e.poi.poi_id)),
                    ));
                }
                PoiState::Pending { .. } => {
                    e.state = PoiState::Expired;
                    self.blocked_window_wake_ms = None;
                    log.push(log_at(
                        at,
                        "p3_expired",
                        Some(format!("poi_id={} reason=user_stop", e.poi.poi_id)),
                    ));
                }
                _ => {}
            }
        }
    }

    /// Resolve L3 `BMK` target at `at` (05 §6.2, `f9d629a`):
    /// 1. Currently `Speaking` POI.
    /// 2. Otherwise the most recent `Completed` / `Interrupted` POI within `bmk_window_ms`
    ///    (default 60 s).
    /// 3. Otherwise `None` (caller returns `NO_PENDING_TARGET`; never guesses another POI).
    pub fn resolve_bmk_target(&self, at: u64) -> Option<String> {
        if let Some(e) = self
            .entries
            .iter()
            .find(|e| matches!(e.state, PoiState::Speaking { .. }))
        {
            return Some(e.poi.poi_id.clone());
        }
        let target = self.last_bmk_target.as_ref()?;
        if at.saturating_sub(target.ended_at_mono_ms) <= self.cfg.bmk_window_ms {
            Some(target.poi_id.clone())
        } else {
            None
        }
    }

    /// Evaluate corridor entry/expiry, Speaking preemption, Pending timeout, and
    /// `Pending → Speaking` version selection at the end of a `dc_tick`.
    pub(crate) fn step(
        &mut self,
        ctx: &StepContext<'_>,
        voice_cmds: &mut Vec<VoiceCmd>,
        ui_cmds: &mut Vec<UiCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        self.blocked_window_wake_ms = None;
        self.speaking_p1_wake_ms = None;

        // 1. Check Speaking interruptions (P0/P1 speak, P1 deadline reached, off-route /
        //    rerouting, thermal >= moderate).
        let speaking_preempt_reason = if ctx.has_p0_or_p1_speak {
            Some("p0_p1_speak")
        } else if matches!(
            ctx.status,
            Some(HorizonStatus::OffRoute | HorizonStatus::Rerouting)
        ) {
            Some("off_route")
        } else if ctx.status == Some(HorizonStatus::Tracking)
            && ctx.next_p1_deadline_mono_ms.is_some_and(|dl| ctx.now >= dl)
        {
            Some("p1_deadline")
        } else if !ctx.thermal_ok {
            Some("thermal")
        } else {
            None
        };
        if let Some(reason) = speaking_preempt_reason {
            self.interrupt_speaking(ctx.now, reason, voice_cmds, ui_cmds, log);
        } else if ctx.status == Some(HorizonStatus::Finished) {
            // Route finished: silence any remaining P3 narration without saving.
            for e in &mut self.entries {
                if let PoiState::Speaking { token, .. } = &e.state {
                    let tok = token.clone();
                    e.state = PoiState::Expired;
                    voice_cmds.push(VoiceCmd::Stop { token: tok });
                    log.push(log_at(
                        ctx.now,
                        "p3_expired",
                        Some(format!("poi_id={} reason=route_finished", e.poi.poi_id)),
                    ));
                }
            }
        }

        // 2. Update Eligible / Pending / Expired states from route status & corridor position.
        let left_route = matches!(
            ctx.status,
            Some(HorizonStatus::OffRoute | HorizonStatus::Rerouting | HorizonStatus::Finished)
        );
        if left_route {
            let reason = if ctx.status == Some(HorizonStatus::Finished) {
                "route_finished"
            } else {
                "off_route"
            };
            for e in &mut self.entries {
                if matches!(e.state, PoiState::Eligible | PoiState::Pending { .. }) {
                    e.state = PoiState::Expired;
                    log.push(log_at(
                        ctx.now,
                        "p3_expired",
                        Some(format!("poi_id={} reason={reason}", e.poi.poi_id)),
                    ));
                }
            }
        } else if ctx.status == Some(HorizonStatus::Tracking) {
            if let Some(progress) = ctx.progress_cm {
                let far = u64::from(self.cfg.corridor_far_cm);
                let near = u64::from(self.cfg.corridor_near_cm);
                let prog = u64::from(progress);
                for e in &mut self.entries {
                    let along = u64::from(e.poi.along_route_cm);
                    let passed_near = prog.saturating_add(near) > along;
                    let inside_corridor = !passed_near && prog.saturating_add(far) >= along;
                    match e.state {
                        PoiState::NotReached => {
                            if passed_near {
                                e.state = PoiState::Expired;
                                log.push(log_at(
                                    ctx.now,
                                    "p3_expired",
                                    Some(format!("poi_id={} reason=passed_corridor", e.poi.poi_id)),
                                ));
                            } else if inside_corridor
                                && self.consent
                                && !self.spoken_pois.contains(&e.poi.poi_id)
                            {
                                e.state = PoiState::Eligible;
                                log.push(log_at(
                                    ctx.now,
                                    "p3_eligible",
                                    Some(format!("poi_id={}", e.poi.poi_id)),
                                ));
                            }
                        }
                        PoiState::Eligible | PoiState::Pending { .. } if !inside_corridor => {
                            let reason = if passed_near {
                                "passed_corridor"
                            } else {
                                "outside_corridor"
                            };
                            e.state = PoiState::Expired;
                            log.push(log_at(
                                ctx.now,
                                "p3_expired",
                                Some(format!("poi_id={} reason={reason}", e.poi.poi_id)),
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }

        // 3. Expire any Pending POI that waited > pending_timeout_ms (30 s).
        for e in &mut self.entries {
            if let PoiState::Pending { since_mono_ms } = e.state {
                if ctx.now.saturating_sub(since_mono_ms) > self.cfg.pending_timeout_ms {
                    e.state = PoiState::Expired;
                    log.push(log_at(
                        ctx.now,
                        "p3_expired",
                        Some(format!("poi_id={} reason=pending_timeout", e.poi.poi_id)),
                    ));
                }
            }
        }

        // 4. If already Speaking, record next P1 deadline wake and stop here.
        if self
            .entries
            .iter()
            .any(|e| matches!(e.state, PoiState::Speaking { .. }))
        {
            if let Some(dl) = ctx.next_p1_deadline_mono_ms {
                if dl > ctx.now {
                    self.speaking_p1_wake_ms = Some(dl);
                }
            }
            return;
        }

        // 5. Promote the earliest Eligible POI to Pending if none is currently Pending.
        let has_pending = self
            .entries
            .iter()
            .any(|e| matches!(e.state, PoiState::Pending { .. }));
        if !has_pending && self.consent {
            if let Some(e) = self.entries.iter_mut().find(|e| {
                e.state == PoiState::Eligible && !self.spoken_pois.contains(&e.poi.poi_id)
            }) {
                e.state = PoiState::Pending {
                    since_mono_ms: ctx.now,
                };
                log.push(log_at(
                    ctx.now,
                    "p3_pending",
                    Some(format!("poi_id={}", e.poi.poi_id)),
                ));
            }
        }

        // 6. Attempt Pending -> Speaking transition.
        let Some(pending_idx) = self
            .entries
            .iter()
            .position(|e| matches!(e.state, PoiState::Pending { .. }))
        else {
            return;
        };

        let in_complex_window = ctx.next_complex_window.is_some_and(|w| {
            ctx.now >= w.start_monotonic_ms && w.end_monotonic_ms.is_none_or(|end| ctx.now <= end)
        });
        if in_complex_window {
            if let Some(end) = ctx.next_complex_window.and_then(|w| w.end_monotonic_ms) {
                if end >= ctx.now {
                    self.blocked_window_wake_ms = end.checked_add(1);
                }
            }
            return;
        }

        let interval_ok = self
            .last_started_at_mono_ms
            .is_none_or(|t| ctx.now.saturating_sub(t) >= self.cfg.min_interval_ms);
        let ready = self.consent
            && ctx.status == Some(HorizonStatus::Tracking)
            && ctx.lateral_ok
            && ctx.acc_and_bearing_ok
            && ctx.thermal_ok
            && !self.call_active
            && !self.audio_focus_lost
            && self.other_tts_tokens.is_empty()
            && !ctx.has_any_other_speak
            && interval_ok;
        if !ready {
            return;
        }

        let avail = available_ms(
            ctx.now,
            ctx.next_p1_deadline_mono_ms,
            ctx.next_complex_window,
            self.cfg.tts_prep_ms,
            self.cfg.safety_margin_ms,
        );
        let Some(ver) = pick_version(&self.entries[pending_idx].poi, avail) else {
            return;
        };
        let content_ref = ver.content_ref.clone();
        let ver_kind = match ver.kind {
            HistoryVersionKind::Short => "short",
            HistoryVersionKind::Summary => "summary",
        };
        let poi_id = self.entries[pending_idx].poi.poi_id.clone();

        self.token_seq += 1;
        // TODO(05 §5): utterance_token encoding is provisional.
        let token = match ctx.route_key {
            Some(rk) => format!(
                "p3:{}:{}:{}:{}",
                rk.trip_id, rk.route_revision.0, poi_id, self.token_seq
            ),
            None => format!("p3:0:{}:{}", poi_id, self.token_seq),
        };
        self.p3_tokens.insert(token.clone());
        self.spoken_pois.insert(poi_id.clone());
        self.last_started_at_mono_ms = Some(ctx.now);
        self.entries[pending_idx].state = PoiState::Speaking {
            token: token.clone(),
            started_at_mono_ms: ctx.now,
        };
        voice_cmds.push(VoiceCmd::SpeakContent { token, content_ref });
        log.push(log_at(
            ctx.now,
            "p3_speaking",
            Some(format!("poi_id={poi_id} version={ver_kind}")),
        ));
        if let Some(dl) = ctx.next_p1_deadline_mono_ms {
            if dl > ctx.now {
                self.speaking_p1_wake_ms = Some(dl);
            }
        }
    }

    /// Earliest future monotonic time when P3 state may change without new input.
    pub fn next_wake(&self, now: u64) -> Option<u64> {
        let mut wake: Option<u64> = self.speaking_p1_wake_ms.filter(|&t| t > now);
        let mut push = |cand: Option<u64>| {
            if let Some(t) = cand.filter(|&t| t > now) {
                wake = Some(wake.map_or(t, |cur| cur.min(t)));
            }
        };
        for e in &self.entries {
            if let PoiState::Pending { since_mono_ms } = e.state {
                push(
                    since_mono_ms
                        .checked_add(self.cfg.pending_timeout_ms)
                        .and_then(|t| t.checked_add(1)),
                );
                if let Some(last) = self.last_started_at_mono_ms {
                    push(last.checked_add(self.cfg.min_interval_ms));
                }
                push(self.blocked_window_wake_ms);
            }
        }
        wake
    }

    fn interrupt_speaking(
        &mut self,
        at: u64,
        reason: &str,
        voice_cmds: &mut Vec<VoiceCmd>,
        ui_cmds: &mut Vec<UiCmd>,
        log: &mut Vec<LogEvent>,
    ) {
        for e in &mut self.entries {
            if let PoiState::Speaking { token, .. } = &e.state {
                let tok = token.clone();
                let poi_id = e.poi.poi_id.clone();
                e.state = PoiState::Interrupted;
                self.last_bmk_target = Some(BmkTarget {
                    poi_id: poi_id.clone(),
                    ended_at_mono_ms: at,
                });
                self.speaking_p1_wake_ms = None;
                voice_cmds.push(VoiceCmd::Stop { token: tok });
                ui_cmds.push(UiCmd::SaveForLater {
                    poi_id: poi_id.clone(),
                    reason: SaveForLaterReason::Interrupted,
                });
                log.push(log_at(
                    at,
                    "p3_interrupted",
                    Some(format!("poi_id={poi_id} reason={reason}")),
                ));
            }
        }
    }
}

/// Available playback window at `now` (ms, 05 §6.2):
/// `min(next P1 deadline, next complex window start) - tts_prep_ms - safety_margin_ms`.
/// `None` when neither horizon bound exists (unbounded).
pub fn available_ms(
    now: u64,
    next_p1_deadline_mono_ms: Option<u64>,
    window: Option<&ComplexWindow>,
    tts_prep_ms: u64,
    safety_margin_ms: u64,
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
            .saturating_sub(safety_margin_ms),
    )
}

fn pick_version(poi: &HistoryPoi, available: Option<u64>) -> Option<&HistoryVersion> {
    let fits = |v: &HistoryVersion| available.is_none_or(|a| u64::from(v.duration_ms) <= a);
    poi.versions
        .iter()
        .find(|v| v.kind == HistoryVersionKind::Short && fits(v))
        .or_else(|| {
            poi.versions
                .iter()
                .find(|v| v.kind == HistoryVersionKind::Summary && fits(v))
        })
}

fn log_at(at: u64, what: &str, detail: Option<String>) -> LogEvent {
    LogEvent {
        at_mono_ms: at,
        what: what.to_owned(),
        req_id: None,
        detail,
    }
}
