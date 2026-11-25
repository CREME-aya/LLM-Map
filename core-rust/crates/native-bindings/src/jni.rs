//! JNI ByteArray pass-through (ABI v1).
//!
//! Kotlin passes the UTF-8 JSON `ByteArray` as-is (no re-serialization), Rust returns a
//! `ByteArray` envelope. One JNI copy in (GetByteArrayRegion) and one out
//! (SetByteArrayRegion); `dc_buf_free` is not needed on this path.
//!
//! Kotlin side: `android/native/core/src/main/kotlin/dev/slmmap/core/DcNative.kt`.
//! TODO(android): the package `dev.slmmap.core` is provisional; the symbol names below
//! must change together with it.
//! TODO(gate1): measurement points for the 300 ms JNI investigation threshold belong here.
//! TODO(gate1): if copies dominate, compare a call-scoped DirectByteBuffer variant.
#![allow(non_snake_case, clippy::missing_safety_doc)]

use crate::ops::{self, guard, DcSession};
use jni_sys::{jbyte, jbyteArray, jclass, jint, jlong, jlongArray, jsize, JNIEnv};
use std::ptr;

unsafe fn read_bytes(env: *mut JNIEnv, arr: jbyteArray) -> Option<Vec<u8>> {
    if arr.is_null() {
        return None;
    }
    let f = &**env;
    let len = (f.GetArrayLength?)(env, arr);
    let mut buf = vec![0u8; len.max(0) as usize];
    if len > 0 {
        (f.GetByteArrayRegion?)(env, arr, 0, len, buf.as_mut_ptr() as *mut jbyte);
        if (f.ExceptionCheck?)(env) != 0 {
            return None;
        }
    }
    Some(buf)
}

/// Returns NULL only if the JVM could not allocate (an exception is then pending).
unsafe fn to_jbytes(env: *mut JNIEnv, v: &[u8]) -> jbyteArray {
    let f = &**env;
    let (Some(new), Some(set)) = (f.NewByteArray, f.SetByteArrayRegion) else {
        return ptr::null_mut();
    };
    let Ok(len) = jsize::try_from(v.len()) else {
        return ptr::null_mut();
    };
    let arr = new(env, len);
    if arr.is_null() {
        return arr;
    }
    set(env, arr, 0, len, v.as_ptr() as *const jbyte);
    arr
}

unsafe fn handle<'a>(h: jlong) -> Option<&'a mut DcSession> {
    (h as usize as *mut DcSession).as_mut()
}

fn null_array() -> Vec<u8> {
    contracts::envelope_err(contracts::ErrorCode::Schema, "input ByteArray is null")
}

unsafe fn stateless(
    env: *mut JNIEnv,
    input: jbyteArray,
    f: impl FnOnce(&[u8]) -> Vec<u8>,
) -> jbyteArray {
    let out = guard(|| match read_bytes(env, input) {
        Some(b) => f(&b),
        None => null_array(),
    });
    to_jbytes(env, &out)
}

unsafe fn stateful(
    env: *mut JNIEnv,
    h: jlong,
    input: jbyteArray,
    f: impl FnOnce(&mut drive_core::Session, &[u8]) -> Vec<u8>,
) -> jbyteArray {
    let out = guard(|| match read_bytes(env, input) {
        Some(b) => ops::with_session(handle(h), |s| f(s, &b)),
        None => null_array(),
    });
    to_jbytes(env, &out)
}

#[no_mangle]
pub extern "system" fn Java_dev_slmmap_core_DcNative_abiVersion(
    _env: *mut JNIEnv,
    _c: jclass,
) -> jint {
    crate::abi_version_word() as jint
}

/// `sessionNew(config: ByteArray, outHandle: LongArray): ByteArray` — outHandle[0] = handle or 0.
#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_sessionNew(
    env: *mut JNIEnv,
    _c: jclass,
    config: jbyteArray,
    out_handle: jlongArray,
) -> jbyteArray {
    let mut created: Option<Box<DcSession>> = None;
    let out = guard(|| match read_bytes(env, config) {
        Some(b) => {
            let (e, s) = ops::session_new(&b);
            created = s;
            e
        }
        None => null_array(),
    });
    let h: jlong = created.map_or(0, |s| Box::into_raw(s) as usize as jlong);
    if !out_handle.is_null() {
        if let Some(set) = (**env).SetLongArrayRegion {
            set(env, out_handle, 0, 1, &h);
        }
    } else if h != 0 {
        drop(Box::from_raw(h as usize as *mut DcSession)); // nowhere to return it
    }
    to_jbytes(env, &out)
}

/// Explicit close from the NavigationSession actor (never a finalizer). 0 is safe.
#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_sessionFree(
    _env: *mut JNIEnv,
    _c: jclass,
    h: jlong,
) {
    if h != 0 {
        let _ = std::panic::catch_unwind(|| drop(Box::from_raw(h as usize as *mut DcSession)));
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_tick(
    env: *mut JNIEnv,
    _c: jclass,
    h: jlong,
    input: jbyteArray,
) -> jbyteArray {
    stateful(env, h, input, ops::tick)
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_validateOverride(
    env: *mut JNIEnv,
    _c: jclass,
    h: jlong,
    input: jbyteArray,
) -> jbyteArray {
    stateful(env, h, input, ops::validate_override)
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_routeCommit(
    env: *mut JNIEnv,
    _c: jclass,
    h: jlong,
    input: jbyteArray,
) -> jbyteArray {
    stateful(env, h, input, ops::route_commit)
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_avoidOfferEval(
    env: *mut JNIEnv,
    _c: jclass,
    h: jlong,
    input: jbyteArray,
) -> jbyteArray {
    stateful(env, h, input, |_, b| {
        ops::unimplemented("dc_avoid_offer_eval", b)
    })
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_planCandidates(
    env: *mut JNIEnv,
    _c: jclass,
    h: jlong,
    input: jbyteArray,
) -> jbyteArray {
    stateful(env, h, input, ops::plan_candidates)
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_rank(
    env: *mut JNIEnv,
    _c: jclass,
    input: jbyteArray,
) -> jbyteArray {
    stateless(env, input, |b| ops::unimplemented("dc_rank", b))
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_tripScore(
    env: *mut JNIEnv,
    _c: jclass,
    input: jbyteArray,
) -> jbyteArray {
    stateless(env, input, |b| ops::unimplemented("dc_trip_score", b))
}

#[no_mangle]
pub unsafe extern "system" fn Java_dev_slmmap_core_DcNative_exchangeValidate(
    env: *mut JNIEnv,
    _c: jclass,
    input: jbyteArray,
) -> jbyteArray {
    stateless(env, input, |b| {
        ops::unimplemented("dc_exchange_validate", b)
    })
}
