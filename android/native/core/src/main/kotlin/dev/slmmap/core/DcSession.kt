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
