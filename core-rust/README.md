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
| 3 | `dc_session_new(config, **out)` | Creates the session state (search slot, deadlines, etc.) from config JSON. On failure sets `*out=NULL` and returns an error envelope | Implemented (config holds only `search_deadline_ms` = watchdog limit, default 10 s; short-distance 5 s TODO) |
| 4 | `dc_session_free(s)` | Frees the session when the actor explicitly closes it. NULL is safe; double free is undefined | Implemented |
| 5 | `dc_plan_candidates(snapshot)` | Builds the candidate search requests `CandidateRequest[]` from a `TripPlanSnapshot` | `E_UNIMPLEMENTED` |
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
