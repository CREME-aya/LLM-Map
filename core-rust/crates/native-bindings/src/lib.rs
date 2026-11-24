//! Thin boundary over `drive-core`: C ABI v1 (`include/dc_core.h`) and the JNI
//! ByteArray pass-through. No logic lives here; panics never cross the boundary.

pub mod abi;
pub mod jni;
pub mod ops;

/// Upper 16 bit = ABI version, lower 16 bit = schema version.
pub const ABI_VERSION: u16 = 1;
pub const SCHEMA_VERSION: u16 = 1;

pub const fn abi_version_word() -> u32 {
    ((ABI_VERSION as u32) << 16) | SCHEMA_VERSION as u32
}
