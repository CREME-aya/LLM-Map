import dev.slmmap.core.DcNative;
import java.nio.charset.StandardCharsets;

public final class JniSmoke {
    static int fails = 0;

    static void check(String what, byte[] out, String needle) {
        String s = out == null ? "null" : new String(out, StandardCharsets.UTF_8);
        boolean ok = s.contains(needle);
        if (!ok) fails++;
        System.out.println((ok ? "PASS " : "FAIL ") + what + ": " + s);
    }

    static byte[] b(String s) { return s.getBytes(StandardCharsets.UTF_8); }

    public static void main(String[] args) {
        System.load(args[0]);
        int v = DcNative.abiVersion();
        System.out.println((v == 0x00010001 ? "PASS" : "FAIL") + " abiVersion: " + Integer.toHexString(v));
        if (v != 0x00010001) fails++;

        long[] h = new long[1];
        check("sessionNew", DcNative.sessionNew(b("{}"), h), "\"ok\":true");
        if (h[0] == 0) { System.out.println("FAIL handle == 0"); System.exit(1); }
        check("tick", DcNative.tick(h[0], b("{\"now_mono_ms\":10,\"events\":[{\"at_mono_ms\":9,\"kind\":\"thermal\",\"status\":\"shutdown\"}]}")), "thermal_changed");
        check("tick bad", DcNative.tick(h[0], b("{")), "E_SCHEMA");
        check("tick null array", DcNative.tick(h[0], null), "E_SCHEMA");
        check("tick handle 0", DcNative.tick(0L, b("{\"now_mono_ms\":1}")), "E_INVALID_HANDLE");
        check("validateOverride", DcNative.validateOverride(h[0], b("{\"code_line\":\"OVR dt=R\",\"contract\":{},\"utterance_id\":\"u1\",\"utterance_mono_ms\":1,\"ref_frame_seq\":1}")), "SYNTAX");
        check("routeCommit", DcNative.routeCommit(h[0], b("{}")), "E_UNIMPLEMENTED");
        check("avoidOfferEval", DcNative.avoidOfferEval(h[0], b("{}")), "E_UNIMPLEMENTED");
        check("planCandidates", DcNative.planCandidates(b("{}")), "E_UNIMPLEMENTED");
        check("rank", DcNative.rank(b("{}")), "E_UNIMPLEMENTED");
        check("tripScore", DcNative.tripScore(b("{}")), "E_UNIMPLEMENTED");
        check("exchangeValidate", DcNative.exchangeValidate(b("{}")), "E_UNIMPLEMENTED");
        // large payload pass-through (≈1 MB) still round-trips as a ByteArray
        StringBuilder sb = new StringBuilder("{\"now_mono_ms\":20,\"events\":[");
        for (int i = 0; i < 20000; i++) { if (i > 0) sb.append(','); sb.append("{\"at_mono_ms\":20,\"kind\":\"low_memory\"}"); }
        sb.append("]}");
        check("tick 1MB", DcNative.tick(h[0], b(sb.toString())), "\"ok\":true");
        DcNative.sessionFree(h[0]);
        DcNative.sessionFree(0L);
        System.out.println(fails == 0 ? "JNI smoke: OK" : "JNI smoke: FAILED (" + fails + ")");
        System.exit(fails == 0 ? 0 : 1);
    }
}
