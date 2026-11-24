package dev.slmmap.core

// JNI ByteArray pass-through to libdc_core.so (Rust, core-rust/crates/native-bindings/src/jni.rs).
//
// - Inputs are the UTF-8 JSON bytes as produced upstream (e.g. the C++ scoring JSON);
//   Kotlin does NOT re-serialize them. Outputs are the {ok, data | error{code,msg}} envelope.
// - Rust copies the output into a new ByteArray and frees its own buffer; dc_buf_free is
//   not used on this path.
// - Not thread-safe: call only from the single NavigationSession actor (see DcSession).
//
// TODO(android): package name `dev.slmmap.core` is provisional; renaming it requires renaming
//                the Java_dev_slmmap_core_DcNative_* symbols in jni.rs.
// TODO(android): NOT COMPILED — no Gradle / Android SDK / NDK on the dev machine yet.
//                Host check of the same JNI signatures: core-rust/scripts/host-smoke.sh
//                (Java stand-in tests/jni/dev/slmmap/core/DcNative.java).
// TODO(gate1): put the JNI transfer timing (300 ms investigation threshold) around these calls.
object DcNative {
    init {
        System.loadLibrary("dc_core")
    }

    /** Upper 16 bit = ABI version, lower 16 bit = schema version (expected 0x0001_0001). */
    @JvmStatic external fun abiVersion(): Int

    /** dc_session_new. outHandle[0] receives the handle (0 on failure). */
    @JvmStatic external fun sessionNew(config: ByteArray, outHandle: LongArray): ByteArray?

    /** dc_session_free. Explicit close only (never from a finalizer). 0 is safe. */
    @JvmStatic external fun sessionFree(handle: Long)

    @JvmStatic external fun tick(handle: Long, input: ByteArray): ByteArray?
    @JvmStatic external fun validateOverride(handle: Long, input: ByteArray): ByteArray?
    @JvmStatic external fun routeCommit(handle: Long, input: ByteArray): ByteArray?
    @JvmStatic external fun avoidOfferEval(handle: Long, input: ByteArray): ByteArray?

    @JvmStatic external fun planCandidates(input: ByteArray): ByteArray?
    @JvmStatic external fun rank(input: ByteArray): ByteArray?
    @JvmStatic external fun tripScore(input: ByteArray): ByteArray?
    @JvmStatic external fun exchangeValidate(input: ByteArray): ByteArray?

    const val EXPECTED_ABI_VERSION: Int = 0x0001_0001
}
