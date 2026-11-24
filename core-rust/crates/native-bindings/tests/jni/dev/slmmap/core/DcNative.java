// Host-only stand-in for the Kotlin `object DcNative` (same JNI class name and signatures),
// used to exercise the Rust JNI shim with a desktop JVM. Not shipped.
package dev.slmmap.core;

public final class DcNative {
    public static native int abiVersion();
    public static native byte[] sessionNew(byte[] config, long[] outHandle);
    public static native void sessionFree(long handle);
    public static native byte[] tick(long handle, byte[] input);
    public static native byte[] validateOverride(long handle, byte[] input);
    public static native byte[] routeCommit(long handle, byte[] input);
    public static native byte[] avoidOfferEval(long handle, byte[] input);
    public static native byte[] planCandidates(byte[] input);
    public static native byte[] rank(byte[] input);
    public static native byte[] tripScore(byte[] input);
    public static native byte[] exchangeValidate(byte[] input);
}
