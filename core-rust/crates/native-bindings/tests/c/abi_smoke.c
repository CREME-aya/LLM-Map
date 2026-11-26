/* Compiles against include/dc_core.h and links libdc_core: checks that the header
 * declarations match the exported Rust symbols. Run via core-rust/scripts/host-smoke.sh. */
#include "dc_core.h"
#include <stdio.h>
#include <string.h>

static dc_in in_str(const char* s) { dc_in i = { (const uint8_t*)s, strlen(s) }; return i; }

static int expect(dc_out o, const char* needle, const char* what) {
    int ok = o.ptr && memmem(o.ptr, o.len, needle, strlen(needle)) != NULL;
    printf("%s %s: %.*s\n", ok ? "PASS" : "FAIL", what, (int)o.len, (const char*)o.ptr);
    dc_buf_free(o);
    return ok ? 0 : 1;
}

int main(void) {
    int fails = 0;
    uint32_t v = dc_abi_version();
    printf("%s abi_version: abi=%u schema=%u\n", v == 0x00010001u ? "PASS" : "FAIL", v >> 16, v & 0xffffu);
    fails += v != 0x00010001u;

    dc_session* s = NULL;
    fails += expect(dc_session_new(in_str("{}"), &s), "\"ok\":true", "session_new");
    if (!s) { puts("FAIL session is NULL"); return 1; }
    fails += expect(dc_tick(s, in_str("{\"now_mono_ms\":10}")), "\"planner_cmds\":[]", "tick");
    fails += expect(dc_validate_override(s, in_str(
        "{\"code_line\":\"YES\",\"contract\":{},\"utterance_id\":\"u1\",\"utterance_mono_ms\":1,\"ref_frame_seq\":1}")),
        "NO_PENDING_TARGET", "validate_override");
    /* no current route: the commit's route_key is stale */
    fails += expect(dc_route_commit(s, in_str("{\"req_id\":\"r1\",\"expected_route_key\":{\"trip_id\":\"t\",\"trip_revision\":\"1\",\"route_revision\":\"1\",\"graph_version\":\"g\"}}")), "E_STALE", "route_commit");
    /* req_id is required (omitted or null -> E_SCHEMA) */
    fails += expect(dc_route_commit(s, in_str("{\"expected_route_key\":null,\"bundle\":{}}")), "E_SCHEMA", "route_commit(no req_id)");
    fails += expect(dc_route_commit(s, in_str("{\"req_id\":null,\"expected_route_key\":null,\"bundle\":{}}")), "E_SCHEMA", "route_commit(null req_id)");
    fails += expect(dc_avoid_offer_eval(s, in_str("{}")), "E_SCHEMA", "avoid_offer_eval");
    fails += expect(dc_avoid_offer_eval(s, in_str("{\"req_id\":\"r1\",\"now_mono_ms\":30,\"alt_facts\":{\"candidate_id\":\"c1\",\"label\":\"through_anchor\",\"plan_request_id\":\"p\",\"graph_version\":\"g\",\"fingerprint\":\"f\",\"legal\":true,\"complete\":true,\"eta_ms\":1,\"length_m\":1,\"counts\":{\"right_unprotected\":0,\"right_signal_unknown_protection\":0,\"right_signal_unknown\":0,\"right_protected\":0,\"right_entry\":0,\"merges\":0},\"narrow_m\":{\"confirmed\":0,\"inferred\":0},\"unknown_width_residential_m\":0,\"unknown_length_m\":0,\"junctions\":[],\"through_anchors\":[]},\"base_facts\":{\"candidate_id\":\"c1\",\"label\":\"through_anchor\",\"plan_request_id\":\"p\",\"graph_version\":\"g\",\"fingerprint\":\"f\",\"legal\":true,\"complete\":true,\"eta_ms\":1,\"length_m\":1,\"counts\":{\"right_unprotected\":0,\"right_signal_unknown_protection\":0,\"right_signal_unknown\":0,\"right_protected\":0,\"right_entry\":0,\"merges\":0},\"narrow_m\":{\"confirmed\":0,\"inferred\":0},\"unknown_width_residential_m\":0,\"unknown_length_m\":0,\"junctions\":[],\"through_anchors\":[]}}")),
        "\"reason\":\"no_pending_result\"", "avoid_offer_eval(no pending)");
    fails += expect(dc_plan_candidates(s, in_str("{\"now_mono_ms\":20,\"eta_estimate_s\":1799,\"snapshot\":{\"schema_version\":1,\"trip_id\":\"t1\",\"trip_revision\":\"3\",\"plan_request_id\":\"p1\",\"graph_version\":\"g\",\"policy_version\":\"pol-1\",\"origin\":{\"lat\":35.681,\"lon\":139.767},\"destination\":{\"display_point\":{\"lat\":35.69,\"lon\":139.70}},\"vias\":[],\"vehicle\":{\"profile\":\"standard_car\",\"width_mm\":1695,\"height_mm\":1550},\"avoid\":{\"toll\":false,\"ferry\":true},\"depart_at_unix_s\":1795000000,\"budget\":{\"max_requests\":3,\"deadline_ms\":3000}}}")),
        "\"distance_class\":\"short\"", "plan_candidates");
    fails += expect(dc_plan_candidates(NULL, in_str("{}")), "E_INVALID_HANDLE", "plan_candidates(NULL)");
    fails += expect(dc_rank(in_str("{}")), "E_SCHEMA", "rank(empty)");
    fails += expect(dc_rank(in_str("{\"facts\":[{\"candidate_id\":\"c1\",\"label\":\"baseline\",\"plan_request_id\":\"p1\",\"graph_version\":\"g\",\"fingerprint\":\"fp1\",\"legal\":true,\"complete\":true,\"eta_ms\":600000,\"length_m\":5000,\"counts\":{\"right_unprotected\":0,\"right_signal_unknown_protection\":0,\"right_signal_unknown\":0,\"right_protected\":0,\"right_entry\":1,\"merges\":0},\"narrow_m\":{\"confirmed\":0,\"inferred\":0},\"unknown_width_residential_m\":0,\"unknown_length_m\":0,\"junctions\":[],\"through_anchors\":[]}]}")),
        "\"score_ms\":690000", "rank");
    fails += expect(dc_trip_score(in_str("{}")), "E_UNIMPLEMENTED", "trip_score");
    fails += expect(dc_exchange_validate(in_str("{}")), "E_UNIMPLEMENTED", "exchange_validate");
    fails += expect(dc_tick(NULL, in_str("{}")), "E_INVALID_HANDLE", "tick(NULL)");
    dc_session_free(s);
    dc_session_free(NULL);
    printf("%s\n", fails ? "C ABI smoke: FAILED" : "C ABI smoke: OK");
    return fails ? 1 : 0;
}
