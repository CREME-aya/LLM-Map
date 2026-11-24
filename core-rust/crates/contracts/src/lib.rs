//! Boundary types shared by `drive-core` and `native-bindings`.
//!
//! Sources (no v1.2 spec document exists yet; see README):
//! - `schema/*.schema.json` (this repo)
//! - devteam conversation: `dc_core.h` ABI v1 (#40), `dc_tick` v2 (#44),
//!   `search_finished` single terminal event (#41, #54, #56).

pub mod common;
pub mod envelope;
pub mod override_types;
pub mod tick;

pub use common::{MonoMs, RouteKey, U64Str};
pub use envelope::{envelope_err, envelope_ok, ErrorCode};
