package dev.slmmap.core

import java.io.Closeable

// Owner of one dc_session handle. Must be confined to the NavigationSession actor
// (single thread / single coroutine); this class does no locking on purpose.
//
// Time / scheduling contract (docs v1.2 ADR-0008):
// - Pass SystemClock.elapsedRealtimeNanos()/1e6 as now_mono_ms (a JSON number) on every tick.
// - Call tick() again no later than TickOutput.next_wake_mono_ms (it includes the search
//   watchdog deadline); null means "tick on each Fix only". Never report a search timeout:
//   Rust detects it from now_mono_ms and enters engine_stuck. search_finished.outcome is
//   ok | cancelled | error, sent exactly once when the native call really returns.
// - TickInput.fix is integers only (docs 04 §3.1, ADR-0008). Round the Location doubles
//   exactly once, here, and nowhere else (determinism starts at the integers):
//     lat_e7       = Math.round(location.latitude  * 1e7)        (Int)
//     lon_e7       = Math.round(location.longitude * 1e7)        (Int)
//     acc_cm       = Math.round(location.accuracy * 100.0)       (meters -> cm)
//     bearing_cdeg = Math.round(location.bearing * 100.0) % 36000   (only if hasBearing();
//                    359.996° rounds to 36000 and wraps to 0; Rust rejects >= 36000 with E_SCHEMA)
//     speed_cm_s   = Math.round(location.speed * 100.0)          (only if hasSpeed(); m/s -> cm/s)
// - routeCommit({req_id, expected_route_key, bundle}) (02 §2.1 #9): req_id is mandatory
//   (null or omitted is E_SCHEMA; the "null for the first route" exception is abolished).
//   Rust accepts req_id only when it names either:
//   (1) an unsettled planner_cmds commit{req_id, route_key} Rust emitted (reroute, consented
//       avoid_chip, or undo-<req_id>), with expected_route_key = that commit's route_key; or
//   (2) an ok candidate search of the current plan (from planCandidates / dc_tick) chosen by
//       the user from the comparison table (first route with expected_route_key = null, or
//       mid-trip replan with expected_route_key = current route's key). A plan's commit window
//       closes when the next plan starts, when one candidate is committed (E_STALE), or once
//       the vehicle starts driving (>= drive_start_speed_cm_s [222 cm/s = 8 km/h] continuously
//       for drive_start_hold_ms [3000 ms]; slow / missing-speed / acc_cm > max_acc_cm fixes
//       reset the run -> E_FORBIDDEN_WHILE_DRIVING).
//   The bundle always carries route_revision "0" (non-"0" is E_SCHEMA): Rust assigns
//   current + 1 (first route: 1; never reset when trip_revision advances) and returns the new
//   key in the result's route_key (use that key from then on). E_STALE means the route was
//   replaced meanwhile, req_id is unknown/stale, or the bundle's trip_id / graph_version
//   differ from the current route, or trip_revision is neither current nor (for a plan
//   candidate matching the plan's trip_revision) current + 1: drop the result (nothing was
//   replaced).
// - planner_cmds commit{..., undo_req_id} (only the avoid-chip consent commit): when you commit
//   that avoid route, keep the route body that was current until then for 5 s under
//   undo_req_id (provisional "undo-<req_id>"). It still carries the route_revision it was
//   loaded with. "戻して" (user_tap{target_id: offer_id, action: undo}, or CXL through
//   validateOverride -> {result: "undone", req_id}, the commit then comes with the next tick)
//   yields commit{req_id: undo_req_id, route_key: the current avoid route's key}: pass
//   req_id = undo_req_id and set the held body's route_revision back to "0" before routeCommit (Rust assigns current + 1; passing the
//   old revision is E_SCHEMA). Drop the held body after 5 s.
// - planner_cmds cancel_commit{req_id}: "戻して" reached Rust before you called routeCommit for
//   the consent commit req_id (docs 836a370). Drop the avoid route body held for req_id and do
//   not commit it (Rust answers E_STALE if you do). Not a refusal: no phrase is spoken; CXL
//   returns {result: "undone", req_id}. The actor is serial, so the order decides. A refused undo comes back as one
//   voice_cmds speak{template_id: "undo_unavailable"} (template provisional).
// - planner_cmds start{priority: reroute, fix}: Rust decided the vehicle left the route
//   (on-route alignment is in Rust, docs 04 §3.1); search from that fix.
// - "経路を確認中" is horizon.status off_route / rerouting; there is no show_checking_route UiCmd.
// - P3 history commentary (docs 05 §6.2, ADR-0008):
//   * Pass corridor POIs in route_bundle.history_pois (content_ref + duration_ms only; no text).
//     Also pass empty history_pois when consent is OFF (defense in depth).
//   * Send history_consent{on: Boolean} via dc_tick (default OFF; persists across routeCommit).
//     Consent OFF expires Eligible/Pending POIs and emits voice_cmds stop{token} if speaking
//     (without save_for_later).
//   * voice_cmds speak carries either (template_id + params) OR content_ref (priority: p3 only).
//     Echo tts_started / tts_done / tts_stopped with the same token; on stop{token} do NOT wait
//     for onStop before starting P1.
//   * Send audio_focus_lost / audio_focus_gained via dc_tick: audio_focus_lost interrupts
//     Speaking P3 (save_for_later{interrupted}) and suppresses new P3; audio_focus_gained
//     restores eligibility for future / pending POIs without resuming an interrupted POI.
//   * Dict dispatch (05 §6.2, schema/slm/expected_codes.json): while P3 is Speaking,
//     "やめて" / "止めて" / "とめて" / "ストップ" stop P3 only (Expired, no save_for_later,
//     no CXL). Outside P3 Speaking, "やめて" resolves to CXL. "キャンセル" / "取り消し" /
//     "取り消して" / "戻して" / "元に戻して" always resolve to CXL and never stop P3.
//   * ui_cmds save_for_later{poi_id, reason: interrupted | bmk}: persist to the "save for later"
//     list. L3 BMK via validateOverride targets the Speaking POI or the most recent Completed /
//     Interrupted POI within 60 s (provisional), returning {result: "bookmarked", poi_id} and
//     emitting save_for_later{poi_id, bmk} on the next tick (else NO_PENDING_TARGET).
//
// TODO(android): NOT COMPILED (no Gradle/Android SDK yet).
// TODO(android): JSON parsing of the envelope (kotlinx.serialization?) is not decided;
//                methods return the raw envelope bytes for now.
class DcSession private constructor(private var handle: Long) : Closeable {

    fun tick(tickJson: ByteArray): ByteArray = call { DcNative.tick(it, tickJson) }

    fun validateOverride(reqJson: ByteArray): ByteArray = call { DcNative.validateOverride(it, reqJson) }

    fun routeCommit(bundleJson: ByteArray): ByteArray = call { DcNative.routeCommit(it, bundleJson) }

    /**
     * dc_avoid_offer_eval: {req_id, now_mono_ms, alt_facts: CandidateFacts,
     * base_facts: CandidateFacts} -> {result: "offered", avoid_offer} | {result: "declined",
     * reason} (shape provisional). Call it for planner_cmds evaluate_offer{req_id, facts_ref,
     * base_facts_ref}, with the CandidateFacts behind both refs. For an avoid_chip search, build
     * base_facts with the alternative (the current route from the search-start position to the
     * destination; no new search) and pass base_facts_ref in its search_finished: without it
     * Rust makes no offer. The chip is shown only from the next tick's horizon;
     * consent is user_tap{target_id: offer_id, action: avoid} or the dictionary "はい"
     * (asr_result{text_class: dict, dict_norm: "はい"}), then Rust emits commit.
     */
    fun avoidOfferEval(reqJson: ByteArray): ByteArray = call { DcNative.avoidOfferEval(it, reqJson) }

    /**
     * dc_plan_candidates: {now_mono_ms, eta_estimate_s?, snapshot: TripPlanSnapshot} ->
     * {plan_request_id, distance_class, candidates, planner_cmds, next_wake_mono_ms, log}.
     * Execute planner_cmds and honour next_wake_mono_ms exactly as for tick(); the remaining
     * candidate searches and the final rank come from later tick() outputs.
     */
    fun planCandidates(reqJson: ByteArray): ByteArray = call { DcNative.planCandidates(it, reqJson) }

    /** Explicit close from the actor. Idempotent on the Kotlin side. */
    override fun close() {
        val h = handle
        handle = 0
        if (h != 0L) DcNative.sessionFree(h)
    }

    private inline fun call(f: (Long) -> ByteArray?): ByteArray {
        check(handle != 0L) { "DcSession is closed" }
        // null only when the JVM failed to allocate the result array (exception pending).
        return f(handle) ?: throw OutOfMemoryError("dc_core: result ByteArray allocation failed")
    }

    companion object {
        /**
         * Stateless dc_rank: {facts: CandidateFacts[], policy?, profile?} -> RouteComparison
         * (schema/route-comparison.schema.json; 02 §2.1 #6, 03 §3.2). Integer-only 3-axis scoring
         * with shared DetourPolicy (baseline always kept) and raw unweighted 3-axis display values.
         */
        fun rank(reqJson: ByteArray): ByteArray =
            DcNative.rank(reqJson) ?: throw OutOfMemoryError("dc_core: rank result ByteArray allocation failed")

        /**
         * Stateless dc_trip_score: 05 §6.1 trip-mode card (C01) trigger scoring
         * (schema/trip-score.schema.json; 02 §2.1 #8). Integer-only; threshold default 3
         * (provisional); suppressed (show_card=false) on driving reroutes.
         */
        fun tripScore(reqJson: ByteArray): ByteArray =
            DcNative.tripScore(reqJson) ?: throw OutOfMemoryError("dc_core: tripScore result ByteArray allocation failed")

        /**
         * Stateless dc_exchange_validate: validates exchange-v1 JSON (IntentPreset / TripPlan /
         * DriveDiary; schema/exchange.schema.json; 02 §2.1 #12, 06 §4.2). Enforce the 16 MB
         * expanded-size limit when unzipping .drivediary before passing the extracted JSON here.
         */
        fun exchangeValidate(reqJson: ByteArray): ByteArray =
            DcNative.exchangeValidate(reqJson) ?: throw OutOfMemoryError("dc_core: exchangeValidate result ByteArray allocation failed")

        /** Returns the session, or the error envelope bytes from dc_session_new. */
        fun open(configJson: ByteArray): Result<DcSession> {
            check(DcNative.abiVersion() == DcNative.EXPECTED_ABI_VERSION) { "dc_core ABI mismatch" }
            val out = LongArray(1)
            val env = DcNative.sessionNew(configJson, out)
                ?: return Result.failure(OutOfMemoryError("dc_core: sessionNew result allocation failed"))
            return if (out[0] != 0L) Result.success(DcSession(out[0]))
            else Result.failure(IllegalArgumentException(String(env, Charsets.UTF_8)))
        }
    }
}
