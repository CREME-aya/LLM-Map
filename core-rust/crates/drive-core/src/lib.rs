//! Narrow pure Rust core (ADR-0002). Everything is a function of its inputs:
//! time only arrives as `now_mono_ms` / `at_mono_ms`; no IO, DB, HTTP, threads or clocks.

pub mod align;
pub mod candidates;
pub mod command_line;
pub mod geo;
pub mod horizon;
pub mod junction;
pub mod planner;
pub mod route;
pub mod session;

pub use session::{CoreError, Session, SessionConfig};
