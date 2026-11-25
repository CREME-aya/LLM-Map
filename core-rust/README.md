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
| 5 | `dc_plan_candidates(snapshot)` | Builds the candidate search requests `CandidateRequest[]` from a `TripPlanSnapshot` | `E_UNIMPLEMENTED` (not wired: the body `Session::plan_candidates(now, eta?, snapshot)` exists and needs the session; the signature change comes first) |
| 6 | `dc_rank(req)` | Applies L1/L2 3-axis scoring and the detour cap (shared with #11) to `{facts[], policy, profile}` and returns a `RouteComparison` | `E_UNIMPLEMENTED` |
| 7 | `dc_validate_override(s, req)` | Parses and validates the SLM's 1-line command code in Rust and returns `Applied` or `Rejected{code}` | SYNTAX / NO_PENDING_TARGET / UNKNOWN_REF only; everything else `E_UNIMPLEMENTED` |
| 8 | `dc_trip_score(summary)` | Computes the trip score (long-distance/trip detection) from a trip summary | `E_UNIMPLEMENTED` |
| 9 | `dc_route_commit(s, bundle)` | Normalizes and loads the confirmed route's maneuver list (the only place the route body is passed) | `E_UNIMPLEMENTED` |
| 10 | `dc_tick(s, tick)` | The only event entry point. Takes `TickInput` and returns `horizon?/voice_cmds/ui_cmds/planner_cmds/next_wake_mono_ms/log` | Search planner (incl. watchdog → `engine_stuck` detected from `now_mono_ms`), time clamping and thermal collapse only |
| 11 | `dc_avoid_offer_eval(s, req)` | Decides whether to show an `AvoidOffer` from `{now_mono_ms, alt_facts}`, using the same detour-cap function as #6 | `E_UNIMPLEMENTED` |
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
Unknown fields are rejected with `E_SCHEMA` wherever the schema says `additionalProperties: false`
(TickInput, Fix, every Event kind, PlannerCmd, VoiceCmd, UiCmd, TickOutput, LogEvent) and in the schema-less
`dc_validate_override` request and `dc_session_new` config.

`voice_cmds` / `ui_cmds` (types only; nothing emits voice / confirm / dismiss yet) hold only
what docs v1.2 names:
- `voice_cmds`: `speak{token, priority: p0|p1|p2|p3, template_id, params{name: integer}}` |
  `stop{token}` (05 §5; no text crosses the boundary, Kotlin renders the template).
- `ui_cmds`: `show_checking_route` | `search_result_ready{req_id, facts_ref}` |
  `show_confirm{confirm_screen_id, override_id, expires_at_mono_ms}` (05 §3.2, ADR-0007) |
  `dismiss{target_id, reason: expired|cancelled|route_changed|superseded}` (04 §4.3, 05 §3.2, ADR-0007).

Not defined because docs do not (TODO, needs a docs decision first):
- `show_avoid_offer` / `horizon_update`: the chip (`HorizonEntry.avoid_offer`) and the 3-ahead
  cards come from `TickOutput.horizon`, the single source (04 §1); a separate UiCmd would duplicate it.
- The 5 s undo window after an avoid switch ("戻して", 04 §4.3 item 3) and the crossfade switch itself.
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
