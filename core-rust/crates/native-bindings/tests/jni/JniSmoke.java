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
        check("routeCommit", DcNative.routeCommit(h[0], b("{\"req_id\":\"r1\",\"expected_route_key\":{\"trip_id\":\"t\",\"trip_revision\":\"1\",\"route_revision\":\"1\",\"graph_version\":\"g\"}}")), "E_STALE");
        check("routeCommit null req_id", DcNative.routeCommit(h[0], b("{\"req_id\":null,\"expected_route_key\":null,\"bundle\":{}}")), "E_SCHEMA");
        check("avoidOfferEval", DcNative.avoidOfferEval(h[0], b("{}")), "E_SCHEMA");
        check("planCandidates", DcNative.planCandidates(h[0], b("{\"now_mono_ms\":20,\"eta_estimate_s\":1799,\"snapshot\":{\"schema_version\":1,\"trip_id\":\"t1\",\"trip_revision\":\"3\",\"plan_request_id\":\"p1\",\"graph_version\":\"g\",\"policy_version\":\"pol-1\",\"origin\":{\"lat\":35.681,\"lon\":139.767},\"destination\":{\"display_point\":{\"lat\":35.69,\"lon\":139.70}},\"vias\":[],\"vehicle\":{\"profile\":\"standard_car\",\"width_mm\":1695,\"height_mm\":1550},\"avoid\":{\"toll\":false,\"ferry\":true},\"depart_at_unix_s\":1795000000,\"budget\":{\"max_requests\":3,\"deadline_ms\":3000}}}")), "\"candidate_id\":\"c1\"");
        check("planCandidates bad", DcNative.planCandidates(h[0], b("{\"now_mono_ms\":1}")), "E_SCHEMA");
        check("planCandidates handle 0", DcNative.planCandidates(0L, b("{}")), "E_INVALID_HANDLE");
        check("rank bad", DcNative.rank(b("{}")), "E_SCHEMA");
        check("rank", DcNative.rank(b("{\"facts\":[{\"candidate_id\":\"c1\",\"label\":\"baseline\",\"plan_request_id\":\"p1\",\"graph_version\":\"g\",\"fingerprint\":\"fp1\",\"legal\":true,\"complete\":true,\"eta_ms\":600000,\"length_m\":5000,\"counts\":{\"right_unprotected\":0,\"right_signal_unknown_protection\":0,\"right_signal_unknown\":0,\"right_protected\":0,\"right_entry\":1,\"merges\":0},\"narrow_m\":{\"confirmed\":0,\"inferred\":0},\"unknown_width_residential_m\":0,\"unknown_length_m\":0,\"junctions\":[],\"through_anchors\":[]}]}")), "\"score_ms\":690000");
        check("tripScore bad", DcNative.tripScore(b("{}")), "E_SCHEMA");
        check("tripScore", DcNative.tripScore(b("{\"distance_m\":150000,\"eta_s\":3600,\"motorway_ratio_permille\":500,\"destination_kind\":\"castle\",\"outside_daily_area\":true,\"corridor_history_density\":\"high\",\"recently_declined_similar\":false,\"is_driving_reroute\":false}")), "\"show_card\":true");
        check("exchangeValidate bad", DcNative.exchangeValidate(b("{}")), "E_SCHEMA");
        check("exchangeValidate", DcNative.exchangeValidate(b("{\"kind\":\"intent_preset\",\"schema_version\":1,\"preset_id\":\"p1\",\"name\":\"night\",\"chips\":[\"c1\"],\"hard_soft\":{\"c1\":\"hard\"},\"detour\":\"tight\",\"weights_permille\":{\"right_turn\":2000,\"narrow_road\":1000,\"merge\":1000}}")), "\"kind\":\"intent_preset\"");
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
