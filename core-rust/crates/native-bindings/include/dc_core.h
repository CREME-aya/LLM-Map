/* dc_core.h — narrow pure Rust core, C ABI v1 (12 functions).
 *
 * Source: docs v1.2 — 02 §2.1 (C ABI v1), ADR-0008 (dc_tick v2, canonical type definitions),
 *         03 §3.1 (search slot / search_finished), 03 §4.2 (dc_validate_override), 04 §2.
 * Keep this header and core-rust/README.md in sync.
 *
 * Ownership: dc_in is owned by the caller and valid until the function returns
 *            (UTF-8 JSON, NOT NUL-terminated).
 *            dc_out is allocated by Rust and MUST be released with dc_buf_free (exactly once).
 *            Every dc_out is {"ok":true,"data":...} | {"ok":false,"error":{"code":..,"msg":..}}
 *            code: E_SCHEMA | E_VERSION | E_STALE | E_REJECTED | E_PANIC | E_INVALID_HANDLE
 *                  (+ E_UNIMPLEMENTED: skeleton-only placeholder, TODO remove before ABI freeze)
 * Threads:   dc_session is NOT thread-safe. Call only from the single NavigationSession actor.
 * Time:      *_mono_ms are SystemClock.elapsedRealtime based. Rust never reads a clock.
 * JSON:      *_mono_ms (MonoMs) and other numbers are JSON numbers. Only Web-reachable u64
 *            ids / revisions (trip_revision, route_revision, OSM/Valhalla edge ids) are decimal
 *            strings. Enum values are snake_case.
 * Panics:    caught at the boundary with catch_unwind and returned as E_PANIC
 *            (requires panic = "unwind"; see core-rust/Cargo.toml).
 */
#pragma once
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct { const uint8_t* ptr; size_t len; } dc_in;
typedef struct { uint8_t* ptr; size_t len; } dc_out;
typedef struct dc_session dc_session;

/* 1  */ uint32_t dc_abi_version(void);                 /* upper 16 bit = ABI, lower 16 bit = schema */
/* 2  */ void     dc_buf_free(dc_out buf);              /* {NULL,0} is a no-op */
/* 3  */ dc_out   dc_session_new(dc_in config_json, dc_session** out_session); /* on failure *out = NULL */
/* 4  */ void     dc_session_free(dc_session* s);       /* NULL is safe. Double free is undefined.
                                                         * Called on explicit actor close, never from a GC finalizer. */

/* 5  */ dc_out   dc_plan_candidates(dc_in trip_plan_snapshot_json);           /* -> CandidateRequest[]   (TODO: E_UNIMPLEMENTED) */
/* 6  */ dc_out   dc_rank(dc_in rank_req_json);         /* {facts[], policy, profile} -> RouteComparison (TODO: E_UNIMPLEMENTED) */
/* 7  */ dc_out   dc_validate_override(dc_session* s, dc_in req_json);
         /* req: {code_line, contract, utterance_id (required), utterance_mono_ms, ref_frame_seq, pending_offer_id?}
          * -> {"result":"applied", override_id, override, review_proposal?, route_key, expires_at_mono_ms}
          *  | {"result":"rejected", code: SYNTAX|UNKNOWN_REF|STALE_REVISION|EXPIRED
          *                               |FORBIDDEN_WHILE_DRIVING|NO_EFFECT|NO_PENDING_TARGET}
          * (skeleton: SYNTAX / NO_PENDING_TARGET / UNKNOWN_REF only, else E_UNIMPLEMENTED) */
/* 8  */ dc_out   dc_trip_score(dc_in trip_summary_json);                      /* TODO: E_UNIMPLEMENTED */
/* 9  */ dc_out   dc_route_commit(dc_session* s, dc_in route_bundle_json);     /* route body only here (TODO: E_UNIMPLEMENTED) */
/* 10 */ dc_out   dc_tick(dc_session* s, dc_in tick_json);
         /* dc_tick v2:
          * in : {now_mono_ms, fix?{at_mono_ms, lat, lon, acc_m, bearing?, speed?},
          *       events:[{at_mono_ms, kind, ...}]   // ascending at_mono_ms
          *   kind: tts_started|tts_done|tts_stopped{token}
          *       | asr_started{utterance_id} | asr_result{utterance_id, text_class: dict|natural, dict_norm?}
          *       | asr_failed{utterance_id}
          *       | override_ready{utterance_id, override_id?, status: ok|timeout|error, confirm_screen_id}
          *       | user_tap{target_id, action: confirm|avoid|undo}
          *             // target_id = AvoidOffer.offer_id | override_id | confirm_screen_id
          *       | thermal{status: none|light|moderate|severe|critical|emergency|shutdown}
          *       | low_memory | inference_down{reason} | inference_up{tier}
          *       | audio_focus_lost | call_state{active}
          *       | search_finished{req_id, route_key, graph_version, outcome: ok|cancelled|error,
          *                         comparison_ref?, engine_ms}     // exactly one per search
          *             // no timeout outcome: Rust detects the watchdog deadline from now_mono_ms
          *             // (-> engine_stuck); an ok returned after cancel is discarded_late,
          *             // after engine_stuck discarded_after_stuck
          *       | engine_reset }
          * out: {horizon? (only when frame_seq changed), voice_cmds[], ui_cmds[],
          *       planner_cmds[start{req_id, priority, route_key, graph_version} | cancel{req_id}],
          *       next_wake_mono_ms: u64|null, log[]}
          *   next_wake_mono_ms: earliest time the output may change without new input (includes the
          *   search watchdog deadline). Kotlin calls dc_tick by then; null = tick on each Fix only. */
/* 11 */ dc_out   dc_avoid_offer_eval(dc_session* s, dc_in req_json);         /* {now_mono_ms, alt_facts} (TODO: E_UNIMPLEMENTED) */
/* 12 */ dc_out   dc_exchange_validate(dc_in req_json);                        /* {kind, payload} (TODO: E_UNIMPLEMENTED) */

#ifdef __cplusplus
}
#endif
