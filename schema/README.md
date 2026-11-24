# schema/ — JSON Schema (single source of truth, v1 skeleton)

The contract shared by Web (Zod) and Rust (serde) (ADR-0002 §4). All files use draft 2020-12.

| File | Role | Used by |
|---|---|---|
| `common.schema.json` | `U64Str` (u64 as a decimal string, Web-reachable IDs / revisions only) / `MonoMs` (JSON integer) / `UnixS` / `RouteKey` / `Evidence` / `ErrorCode` / `DcEnvelope` | Referenced by every schema |
| `trip-plan-snapshot.schema.json` | `TripPlanSnapshot` | Input to `dc_plan_candidates` |
| `candidate-facts.schema.json` | `CandidateFacts` (scoring JSON generated once by C++) | Input to `dc_rank` / `dc_avoid_offer_eval` |
| `maneuver-horizon.schema.json` | `ManeuverHorizon` v1 (including `refs{next?,last?}` and `AvoidOffer.offer_id`) | `TickOutput.horizon` |
| `personal-routing-override.schema.json` | Expanded form of the 1-line `OVR` code | `Applied.override` from `dc_validate_override` |
| `slm/override.gbnf` | Canonical GBNF grammar for the driving-time 1-line L3 command code (03 §4.1) | `:inference` (`llama.cpp`), `drive-core` parser conformance, Gate 4 harness |
| `slm/expected_codes.json` | Canonical specification of 1-line command codes, deterministic phrase dictionary, OVR canonical order, and Gate 4 scoring policy (03 §4.1 / 07 §2.3) | Gate 4 harness (`pipeline/harness/gate4/`), Rust dict prefilter |

- `fixtures/valid/<schema>.*.json` must pass the schema; `fixtures/invalid/<schema>.*.json` must fail it.
  This is checked by `core-rust/crates/contracts/tests/schema_fixtures.rs`.
- The sidecar binary spec is `pipeline/packs/sidecar-format.md` (sol6.2さん).
- The `dc_tick` `TickInput`/`TickOutput` types are canonical in ADR-0008. This skeleton has no tick schema yet; only the Rust types exist (`contracts::tick`) (TODO).

JSON conventions (docs v1.2 ADR-0008 decision 2 / 04 §2): `MonoMs` and other numbers are JSON integers; `U64Str`
is used only for Web-reachable u64 IDs / revisions; enum values are snake_case.

Sources: docs v1.2 (ADR-0008, 02 §2.1, 03 §3.1 / §4, 04 §2) and the devteam schema draft (#13).
Undefined points are marked `TODO(schema-v1)` in each file's `description`.
