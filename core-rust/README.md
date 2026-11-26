# core-rust/ — narrow pure Rust core + C ABI v1 + JNI ByteArray pass-through

```
core-rust/
├── Cargo.toml                 # workspace. panic = "unwind" (required for catch_unwind)
├── .cargo/config.toml         # aarch64-linux-android: -z max-page-size=16384 (unverified)
├── crates/contracts/          # serde types mirroring schema/ and dc_tick v2 (no IO)
├── crates/drive-core/         # pure state machines: search planner, 1-line code parser, Session
├── crates/native-bindings/    # lib name = dc_core. include/dc_core.h, src/abi.rs (C ABI), src/jni.rs (JNI)
├── tests/fixtures/tick/       # Gate 7 common test data (step sequences for dc_tick)
└── scripts/host-smoke.sh      # C header ⇔ symbol check + JNI host check (paths with spaces quoted)
```

## The 12 functions of C ABI v1 (the authoritative signatures are in `crates/native-bindings/include/dc_core.h`)

| # | Function | Description | Skeleton status |
|---:|---|---|---|
| 1 | `dc_abi_version()` | Returns a u32 with the ABI version in the upper 16 bits and the schema version in the lower 16 (currently `0x0001_0001`) | Implemented |
| 2 | `dc_buf_free(dc_out)` | Frees a `dc_out` allocated by Rust, exactly once. `{NULL,0}` is a no-op | Implemented |
| 3 | `dc_session_new(config, **out)` | Creates the session state (search slot, deadlines, etc.) from config JSON. On failure sets `*out=NULL` and returns an error envelope | Implemented (config: optional `watchdog_{short,long}_ms` 5 s / 10 s and `comparison_budget_{short,long}_ms` 3 s / 8 s; distance class `short|long` fixed per plan from the pre-search ETA, or per search outside a plan from the remaining ETA, < 1800 s → short) |
| 4 | `dc_session_free(s)` | Frees the session when the actor explicitly closes it. NULL is safe; double free is undefined | Implemented |
| 5 | `dc_plan_candidates(s, req)` | `{now_mono_ms, eta_estimate_s?, snapshot}` → `{plan_request_id, distance_class, candidates, planner_cmds, next_wake_mono_ms, log}`: fixes the plan's distance class, builds `CandidateRequest[]` from the `TripPlanSnapshot`, queues their searches and returns the first `start` | Implemented (`Session::plan_candidates`; candidate triggers / `through_anchor` TODO) |
| 6 | `dc_rank(req)` | Applies L1/L2 3-axis scoring and the detour cap (shared with #11) to `{facts[], policy, profile}` and returns a `RouteComparison` | `E_UNIMPLEMENTED` |
| 7 | `dc_validate_override(s, req)` | Parses and validates the SLM's 1-line command code in Rust and returns `Applied` or `Rejected{code}` | SYNTAX / NO_PENDING_TARGET / UNKNOWN_REF only; everything else `E_UNIMPLEMENTED` |
| 8 | `dc_trip_score(summary)` | Computes the trip score (long-distance/trip detection) from a trip summary | `E_UNIMPLEMENTED` |
| 9 | `dc_route_commit(s, req)` | Loads the confirmed route body (the only place it is passed) | Implemented: `{req_id: string\|null, expected_route_key: RouteKey\|null, bundle: RouteBundle}` — the guard is re-checked first (`E_STALE`, nothing replaced): `expected_route_key` (from `commit`, `null` for the first route) and `req_id` (provisional: while a route is current it must name a `commit` Rust issued with that `route_key` and not settled yet; any loaded route settles them all; `null` allowed for the first route), then the bundle must carry `route_revision` "0" (`E_SCHEMA`; Rust assigns current + 1, the first route 1, rewrites every `route_key` in the bundle and returns it), and while a route is current its `trip_id` / `trip_revision` / `graph_version` must match (`E_STALE`), then the bundle is validated (`E_SCHEMA`) and indexed |
| 10 | `dc_tick(s, tick)` | The only event entry point. Takes `TickInput` and returns `horizon?/voice_cmds/ui_cmds/planner_cmds/next_wake_mono_ms/log` | Search planner (incl. watchdog → `engine_stuck` detected from `now_mono_ms`), on-route alignment of `fix` (off-route → `reroute` start with the fix), remaining ETA, `ManeuverHorizon` (3 ahead, `frame_seq` only on change, `next_complex_window` from the junction classes of `drive_core::junction` / groups while tracking; P3 available time `horizon::p3_available_ms`, 05 §6.2), time clamping and thermal collapse |
| 11 | `dc_avoid_offer_eval(s, req)` | Decides whether to show an `AvoidOffer` from `{req_id, now_mono_ms, alt_facts, base_facts}` (called for `planner_cmds: evaluate_offer{req_id, facts_ref, base_facts_ref}`; `base_facts_ref` comes optionally with the avoid search's `search_finished`, no offer without it; 04 §4.3 item 2: (a) the target's `(edge_in, edge_out)` not in `alt_facts.junctions`, (b) against `base_facts` fewer avoid targets and `narrow_m` not longer, else declined), using the same detour-cap function as #6 (`drive_core::avoid::DetourPolicy`) → `{result: offered, avoid_offer} \| {result: declined, reason}` (shape provisional) | implemented |
| 12 | `dc_exchange_validate(req)` | Validates `{kind, payload}` received via `exchange-v1` | `E_UNIMPLEMENTED` |

JSON conventions (docs v1.2 ADR-0008): `*_mono_ms` are JSON numbers; only Web-reachable u64 IDs / revisions
(`trip_revision`, `route_revision`, edge ids) are decimal strings; enum values are snake_case;
`dc_validate_override` returns `{"result":"applied"|"rejected", ...}`. `search_finished.outcome` is `ok|cancelled|error`.
`planner_cmds.start` carries the required `distance_class: short|long` fixed by the Planner,
and `candidate_id` for a plan's candidate search. A plan's class comes from the pre-search ETA
estimate and is fixed for the plan; a search outside a plan (reroute / avoid chip) fixes its
class at its start from the valid remaining ETA on the current route (< 1800 s → short;
≥ 1800 s, unknown or invalidated by off-route → long; the plan's class is not inherited) and
uses it only for the watchdog limit (5 s / 10 s); the comparison budget (3 s / 8 s) is for plans. A plan ends with exactly one
`rank{plan_request_id, facts_refs, reason: all_done|budget_expired}`: Kotlin then calls
`dc_rank` with the CandidateFacts behind `facts_refs` (`search_finished.facts_ref`,
formerly `comparison_ref`). On budget expiry queued candidates are dropped and a running one
is cancelled; it keeps the slot until its `search_finished`.
`commit{req_id, route_key}` is emitted only when a `reroute` / ad-hoc `explicit_plan` search
returns `ok` with a valid state (not cancelled / stuck, `route_key` equal to the current
route): Kotlin calls `dc_route_commit` with that search's route body. `route_key` is the current
route's key when the search started (not the new route's); `dc_route_commit` re-checks it and
returns `E_STALE` if the route was replaced meanwhile. An avoid-chip `ok` is not committed when
it returns: it waits for `dc_avoid_offer_eval`, the `AvoidOffer` rides on its `HorizonEntry`,
and only `user_tap{target_id: offer_id, action: avoid}` before the deadline (while tracking,
same route_key) issues `commit` (`drive_core::avoid`; look-ahead off by default,
`avoid_chip_lookahead`, 04 §4.3 item 6). The deadline removes it with `dismiss{expired}`. "経路を確認中" for a search in
progress is `horizon.status` (`off_route` / `rerouting`), not a per-result UiCmd.
"戻して" (03 §3.1; team decision 2026-11-26): the consent `commit` carries `undo_req_id`
(`undo-<req_id>`, provisional) and Kotlin keeps the previous route body under it for 5 s. The
divergence point is computed once, in the `dc_route_commit` that loads the avoid route (docs
say at `AvoidOffer` creation; same result since an undo needs that commit first), from the
position when the avoid search started (`avoid::divergence_cm`; not found / ambiguous →
unknown). `user_tap{offer_id, undo}` or `CXL` (`dc_validate_override` → `{result: undone,
req_id}`, commit with the next tick): > 5 s after consent → `NO_PENDING_TARGET`; avoid
route not loaded yet → `cancel_commit{req_id}` (docs 836a370: the consent commit is withdrawn
and its late `dc_route_commit` is `E_STALE`; a success, no phrase; CXL → `undone{req_id}`); unknown
divergence, not `tracking` (`uncertain`), or at / past the
divergence point → `NO_EFFECT`; else `commit{req_id: undo_req_id, route_key: current}` and the
held result is dropped. Kotlin passes the held body with `route_revision` "0" (Rust: current
+ 1; the revision it carried is `E_SCHEMA`). A refusal speaks `undo_unavailable` (P2, no params;
template id provisional) once.
A committable `ok` that returns while `uncertain` is held (one at most; a newer one replaces it,
`discarded_hold_replaced`) and its `commit` is issued when the status is back to `tracking` /
`off_route` / `rerouting` and the route_key still matches; after `commit_hold_ms` (config,
10 s provisional) it is `discarded_hold_expired`. While `finished` it is `discarded_not_tracking`.
## Route bundle and on-route alignment (docs 04 §3.1)

- `schema/route-bundle.schema.json` (mirror: `contracts::route`): `legs[{leg_index,
  along_route_start_cm, length_cm, chunks[{first_shape_index, origin_e7{lat,lon}, k_lat_q16,
  k_lon_q16, points_cm[[dx,dy]] (i32), seg_along_cm[u32], bearing_cdeg[u16]}], edges[{edge_id,
  from_pt, to_pt, length_cm, cum_time_ms, flags}], maneuvers[Maneuver ($ref)]}]`, `route_key`,
  `eta_ms`, `length_cm`. Distances per leg (`seg_along_cm`) and route-wide (`along_route_cm` =
  `along_route_start_cm` + leg value). Checks JSON Schema cannot express (continuity, edge
  coverage, ordering, totals) are done on load (`drive_core::route`).
- Fixed point (team proposal, provisional): `k_*_q16` = cm per 1e-7 deg in Q16, computed once by
  C++ (cos); `dx = round_q16((lon_e7 - origin.lon) as i64 * k_lon_q16)`, `round_q16(v) = (v +
  2^15) >> 16`; i64 squares / dot / cross, integer sqrt (floor). Segment bearings come from C++
  (`bearing_cdeg`), compared as `min(d, 36000 - d)` with `d = |a - b| mod 36000`. A chunk is valid
  within its points' bounding box + 1 km; near a boundary both chunks are tried, the smaller
  lateral distance wins (tie: smaller along). No f32 / f64 in `geo` / `route` / `align` /
  `horizon` / `session` / `planner` (`tests/no_float.rs` + `#![deny(clippy::float_arithmetic)]`).
- Alignment (`drive_core::align`): local window around the previous progress; grid re-acquisition
  (500 m cells per chunk) when lost; candidates on distinct route parts → `uncertain`. Thresholds
  in `dc_session_new` config (07 §1, provisional): `lateral_tol_cm` 3000, `heading_tol_deg` 60
  (`heading_min_speed_cm_s` 140), `confirm_fixes` 3, `max_acc_cm` (default = `lateral_tol_cm`;
  such a fix resets the off-route run, `confirm_fixes` of them → `uncertain`), `fix_stale_ms` 2500.
- Remaining ETA = `eta_ms` - time at the matched position (`edges.cum_time_ms`), only while
  `tracking`; it decides the distance class of searches outside a plan.
- Gate 7 alignment cases: `tests/fixtures/route/*.json` (generated by `gen_route_fixtures.py`,
  which plays C++ and Kotlin's rounding), replayed by `crates/drive-core/tests/gate7_alignment.rs`
  (`cargo test -p drive-core --test gate7_alignment -- --nocapture` prints the recorded values).

`TickInput.fix` is integers only (docs 04 §3.1, ADR-0008): `{at_mono_ms, lat_e7, lon_e7, acc_cm,
bearing_cdeg?, speed_cm_s?}`. Kotlin rounds once (`Math.round(deg * 1e7)`,
`Math.round(deg * 100) % 36000`, cm, cm/s); out-of-range values (e.g. `bearing_cdeg` 36000) → `E_SCHEMA`.
Unknown fields are rejected with `E_SCHEMA` wherever the schema says `additionalProperties: false`
(TickInput, Fix, every Event kind, PlannerCmd, VoiceCmd, UiCmd, TickOutput, LogEvent) and in the schema-less
`dc_validate_override` request and `dc_session_new` config.

`voice_cmds` / `ui_cmds` (types only; nothing emits voice / confirm yet, `dismiss` only for the
avoid chip) hold only
what docs v1.2 names:
- `voice_cmds`: `speak{token, priority: p0|p1|p2|p3, template_id, params{name: integer}}` |
  `stop{token}` (05 §5; no text crosses the boundary, Kotlin renders the template).
- `ui_cmds`: `show_confirm{confirm_screen_id, override_id, expires_at_mono_ms}` (05 §3.2, ADR-0007) |
  `dismiss{target_id, reason: expired|cancelled|route_changed|superseded}` (04 §4.3, 05 §3.2, ADR-0007).

Not defined because docs do not (TODO, needs a docs decision first):
- `show_avoid_offer` / `horizon_update`: the chip (`HorizonEntry.avoid_offer`) and the 3-ahead
  cards come from `TickOutput.horizon`, the single source (04 §1); a separate UiCmd would duplicate it.
- The crossfade switch of an avoid chip (04 §4.3 item 3).
- "あとで見る" save of an interrupted P3 commentary (05 §6, ADR-0006).
- Thermal / low-memory degradation notices (05 §3.1), Tier 0 fallback chips after an SLM timeout (05 §3.2).
- The `utterance_token {route_key, event_id, generation}` encoding (string today, same as `tts_*{token}`),
  the template id catalogue and the param names.

Rules: `dc_in` is owned by the caller and valid until the function returns (not NUL-terminated). `dc_out` is allocated by Rust and released with `dc_buf_free`.
Every return value is `{ok, data | error{code,msg}}`. Panics become `E_PANIC`. Calls come only from the NavigationSession actor.
On the JNI path, Rust copies the result into a `ByteArray` and frees its own buffer (Kotlin never calls `dc_buf_free`).

## Build and test

```sh
cd "<repo>/core-rust"
cargo test                 # schema conformance, planner replay, parser, C ABI
cargo clippy --all-targets
./scripts/host-smoke.sh    # C header smoke + JNI (javac/java) smoke
```

Android (`cargo ndk -t arm64-v8a build --release`, `readelf -l` for 16 KB alignment) has not been run because the NDK and cargo-ndk are not installed.
