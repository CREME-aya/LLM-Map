//! Boundary types shared by `drive-core` and `native-bindings`.
//!
//! Sources:
//! - docs v1.2: ADR-0008 (dc_tick v2, canonical), 02 §2.1 (C ABI v1), 03 §3.1 / §4, 04 §2
//! - `schema/*.schema.json` (this repo)
//!
//! JSON conventions (ADR-0008 decision 2): `MonoMs` and other numbers are JSON numbers;
//! only Web-reachable u64 IDs / revisions are `U64Str` decimal strings; enum values are
//! snake_case (the 7 reject codes and `E_*` error codes are fixed uppercase strings).

pub mod common;
pub mod envelope;
pub mod facts;
pub mod horizon;
pub mod override_types;
pub mod plan;
pub mod route;
pub mod tick;

pub use common::{MonoMs, RouteKey, U64Str};
pub use envelope::{envelope_err, envelope_ok, ErrorCode};
