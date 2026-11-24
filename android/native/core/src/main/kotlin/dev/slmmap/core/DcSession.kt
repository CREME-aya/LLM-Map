package dev.slmmap.core

import java.io.Closeable

// Owner of one dc_session handle. Must be confined to the NavigationSession actor
// (single thread / single coroutine); this class does no locking on purpose.
//
// TODO(android): NOT COMPILED (no Gradle/Android SDK yet).
// TODO(android): JSON parsing of the envelope (kotlinx.serialization?) is not decided;
//                methods return the raw envelope bytes for now.
class DcSession private constructor(private var handle: Long) : Closeable {

    fun tick(tickJson: ByteArray): ByteArray = call { DcNative.tick(it, tickJson) }

    fun validateOverride(reqJson: ByteArray): ByteArray = call { DcNative.validateOverride(it, reqJson) }

    fun routeCommit(bundleJson: ByteArray): ByteArray = call { DcNative.routeCommit(it, bundleJson) }

    fun avoidOfferEval(reqJson: ByteArray): ByteArray = call { DcNative.avoidOfferEval(it, reqJson) }

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
