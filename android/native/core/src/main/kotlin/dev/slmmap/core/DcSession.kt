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
// - planner_cmds commit{req_id, route_key}: call routeCommit() with
//   {expected_route_key, bundle}: that route_key (the route's key when the search started) and the
//   route bundle of search req_id (schema/route-bundle.schema.json, built by C++). The first
//   route of a session goes in with expected_route_key = null. E_STALE means the route was replaced
//   meanwhile: drop the result (nothing was replaced).
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
