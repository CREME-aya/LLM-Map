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
    fails += expect(dc_tick(s, in_str("{\"now_mono_ms\":\"10\"}")), "\"planner_cmds\":[]", "tick");
    fails += expect(dc_validate_override(s, in_str(
        "{\"code_line\":\"YES\",\"contract\":{},\"utterance_mono_ms\":\"1\",\"ref_frame_seq\":\"1\"}")),
        "NO_PENDING_TARGET", "validate_override");
    fails += expect(dc_route_commit(s, in_str("{}")), "E_UNIMPLEMENTED", "route_commit");
    fails += expect(dc_avoid_offer_eval(s, in_str("{}")), "E_UNIMPLEMENTED", "avoid_offer_eval");
    fails += expect(dc_plan_candidates(in_str("{}")), "E_UNIMPLEMENTED", "plan_candidates");
    fails += expect(dc_rank(in_str("{}")), "E_UNIMPLEMENTED", "rank");
    fails += expect(dc_trip_score(in_str("{}")), "E_UNIMPLEMENTED", "trip_score");
    fails += expect(dc_exchange_validate(in_str("{}")), "E_UNIMPLEMENTED", "exchange_validate");
    fails += expect(dc_tick(NULL, in_str("{}")), "E_INVALID_HANDLE", "tick(NULL)");
    dc_session_free(s);
    dc_session_free(NULL);
    printf("%s\n", fails ? "C ABI smoke: FAILED" : "C ABI smoke: OK");
    return fails ? 1 : 0;
}
