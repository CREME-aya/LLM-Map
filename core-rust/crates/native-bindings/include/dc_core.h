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

/* 5  */ dc_out   dc_plan_candidates(dc_session* s, dc_in req_json);
         /* in : {now_mono_ms, eta_estimate_s?, snapshot: TripPlanSnapshot}
          *      eta_estimate_s = pre-search ETA estimate (< 1800 -> short; >= 1800 or absent -> long)
          * out: {plan_request_id, distance_class: short|long,
          *       candidates[{candidate_id, label, priority, engine_alternates, costing_variant?}],
          *       planner_cmds[] (the first start{..., candidate_id}; the rest come from dc_tick),
          *       next_wake_mono_ms: u64|null, log[]}
          * Same plan_request_id as the current plan -> class kept, nothing re-queued; a new one
          * supersedes the current plan (its running search gets cancel). Unknown fields -> E_SCHEMA. */
/* 6  */ dc_out   dc_rank(dc_in rank_req_json);         /* {facts[], policy, profile} -> RouteComparison (TODO: E_UNIMPLEMENTED) */
/* 7  */ dc_out   dc_validate_override(dc_session* s, dc_in req_json);
         /* req: {code_line, contract, utterance_id (required), utterance_mono_ms, ref_frame_seq, pending_offer_id?}
          * -> {"result":"applied", override_id, override, review_proposal?, route_key, expires_at_mono_ms}
          *  | {"result":"rejected", code: SYNTAX|UNKNOWN_REF|STALE_REVISION|EXPIRED
          *                               |FORBIDDEN_WHILE_DRIVING|NO_EFFECT|NO_PENDING_TARGET}
          * (skeleton: SYNTAX / NO_PENDING_TARGET / UNKNOWN_REF only, else E_UNIMPLEMENTED) */
/* 8  */ dc_out   dc_trip_score(dc_in trip_summary_json);                      /* TODO: E_UNIMPLEMENTED */
/* 9  */ dc_out   dc_route_commit(dc_session* s, dc_in route_bundle_json);     /* route body only here */
         /* in : {req_id: string|null, expected_route_key: RouteKey|null, bundle: RouteBundle}
          *      (schema/route-bundle.schema.json)
          *      req_id (provisional, proposed to the team) = the req_id of the planner_cmds
          *      commit this route answers (undo-<req_id> for an undo). While a route is current
          *      it must name a commit Rust issued with route_key = expected_route_key and not
          *      settled yet (loading any route settles them all; cancel_commit withdraws one)
          *      -> otherwise E_STALE, nothing replaced. null / any value for the first route.
          *      expected_route_key = guard: from planner_cmds commit (the current route's key when the
          *      search started), or null for the first route. Must equal the current route
          *      (null = none) -> otherwise E_STALE, nothing replaced (checked first).
          *      bundle.route_key.route_revision (and every ManeuverId.route_key) must be "0"
          *      -> otherwise E_SCHEMA. Rust assigns current + 1 (the first route: 1) and
          *      rewrites the bundle's route_keys to it. While a route is current, the bundle's
          *      trip_id / trip_revision / graph_version must equal it -> otherwise E_STALE.
          *      Nothing is replaced on any error.
          *      bundle = {route_key (new, route_revision "0"), eta_ms, length_cm, legs[{leg_index, along_route_start_cm,
          *      length_cm, chunks[{first_shape_index, origin_e7{lat,lon}, k_lat_q16, k_lon_q16,
          *      points_cm[[dx,dy]], seg_along_cm[], bearing_cdeg[]}], edges[{edge_id, from_pt, to_pt,
          *      length_cm, cum_time_ms, flags}], maneuvers[Maneuver]}]}; invalid -> E_SCHEMA.
          * out: {route_key (with the assigned route_revision), legs, segments, maneuvers}. The next dc_tick returns the new horizon
          *      (uncertain until a fix is aligned on the new route). */
/* 10 */ dc_out   dc_tick(dc_session* s, dc_in tick_json);
         /* dc_tick v2:
          * in : {now_mono_ms, fix?{at_mono_ms, lat_e7, lon_e7, acc_cm, bearing_cdeg?, speed_cm_s?},
          *       // integers only (04 §3.1): lat_e7/lon_e7 = Math.round(deg*1e7), acc_cm in cm,
          *       // bearing_cdeg = Math.round(deg*100) % 36000 (0..35999; 36000 -> E_SCHEMA), speed_cm_s
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
          *                         facts_ref?, engine_ms}     // exactly one per search
          *             // no timeout outcome: Rust detects the watchdog deadline from now_mono_ms
          *             // (-> engine_stuck); an ok returned after cancel is discarded_late,
          *             // after engine_stuck discarded_after_stuck
          *       | engine_reset }
          * out: {horizon? (only when frame_seq changed),
          *       voice_cmds[speak{token, priority: p0|p1|p2|p3, template_id, params{name: int}}
          *                 | stop{token}],
          *       ui_cmds[show_confirm{confirm_screen_id, override_id, expires_at_mono_ms}
          *              | dismiss{target_id, reason: expired|cancelled|route_changed|superseded}],
          *       planner_cmds[start{req_id, priority, distance_class: short|long, route_key,
          *                    graph_version, candidate_id?, fix?} | cancel{req_id}
          *                  | rank{plan_request_id, facts_refs[], reason: all_done|budget_expired}
          *                  | commit{req_id, route_key, undo_req_id?}],
          *   "経路を確認中" is horizon.status off_route / rerouting (show_checking_route is abolished).
          *   start.fix: for a reroute, the fix that confirmed off-route (Rust aligns the vehicle on
          *   the committed route only; Valhalla searches the road network from that position).
          *   commit: only for a reroute / ad-hoc explicit_plan ok with a valid state (not
          *   cancelled / stuck, route_key current, horizon.status not uncertain / finished); Kotlin calls
          *   dc_route_commit with that search's route body (never for plan candidates). For an
          *   avoid_chip search only after consent: user_tap{target_id: offer_id, action: avoid}
          *   before the offer's deadline, while tracking, route_key unchanged (04 §4.3, 03 §3.1).
          *   That consent commit carries undo_req_id ("undo-<req_id>", provisional): keep the
          *   route body current until then for 5 s under it. "戻して" (user_tap{offer_id, undo}
          *   or CXL via dc_validate_override → {result: undone, req_id}) within 5 s and before
          *   the divergence point → commit{req_id: undo_req_id, route_key: current}: pass the
          *   held body with route_revision set back to "0". Refused (NO_EFFECT /
          *   NO_PENDING_TARGET) → one speak{template_id: undo_unavailable}.
          *   rank: once per plan; Kotlin calls dc_rank with the CandidateFacts behind facts_refs
          *   (may be empty). The plan is over: no more start for it, a late search_finished
          *   only frees the slot (discarded_late).
          *       next_wake_mono_ms: u64|null, log[]}
          *   next_wake_mono_ms: earliest time the output may change without new input (includes the
          *   search watchdog deadline). Kotlin calls dc_tick by then; null = tick on each Fix only. */
/* 11 */ dc_out   dc_avoid_offer_eval(dc_session* s, dc_in req_json);         /* {now_mono_ms, alt_facts: CandidateFacts}
          *   Call after the dc_tick that carried search_finished{ok} of an avoid_chip search.
          * → {result: "offered", avoid_offer: AvoidOffer} (shown on its HorizonEntry from the
          *   next dc_tick; dismiss{expired} at the deadline) | {result: "declined", reason:
          *   no_pending_result|not_tracking|deadline_passed|graph_mismatch|not_legal_or_complete
          *   |over_detour_cap}. Result shape provisional (not in docs). */
/* 12 */ dc_out   dc_exchange_validate(dc_in req_json);                        /* {kind, payload} (TODO: E_UNIMPLEMENTED) */

#ifdef __cplusplus
}
#endif
