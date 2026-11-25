//! C ABI v1 — must match `include/dc_core.h` exactly (checked by tests/c/abi_smoke.c).
#![allow(clippy::missing_safety_doc)]

use crate::ops::{self, guard, DcSession};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

/// Caller-owned input, valid until return. Not NUL-terminated.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DcIn {
    pub ptr: *const u8,
    pub len: usize,
}

/// Rust-owned output; release with `dc_buf_free`.
#[repr(C)]
pub struct DcOut {
    pub ptr: *mut u8,
    pub len: usize,
}

impl DcOut {
    fn from_vec(v: Vec<u8>) -> Self {
        let b: Box<[u8]> = v.into_boxed_slice();
        let len = b.len();
        let ptr = Box::into_raw(b) as *mut u8;
        DcOut { ptr, len }
    }
}

unsafe fn input<'a>(i: DcIn) -> &'a [u8] {
    if i.ptr.is_null() || i.len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(i.ptr, i.len)
    }
}

unsafe fn session<'a>(s: *mut DcSession) -> Option<&'a mut DcSession> {
    s.as_mut()
}

#[no_mangle]
pub extern "C" fn dc_abi_version() -> u32 {
    crate::abi_version_word()
}

#[no_mangle]
pub unsafe extern "C" fn dc_buf_free(buf: DcOut) {
    if buf.ptr.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
            buf.ptr, buf.len,
        )));
    }));
}

#[no_mangle]
pub unsafe extern "C" fn dc_session_new(
    config_json: DcIn,
    out_session: *mut *mut DcSession,
) -> DcOut {
    if !out_session.is_null() {
        *out_session = ptr::null_mut();
    }
    let mut created: Option<Box<DcSession>> = None;
    let env = guard(|| {
        let (env, s) = ops::session_new(input(config_json));
        created = s;
        env
    });
    match (created, out_session.is_null()) {
        (Some(s), false) => *out_session = Box::into_raw(s),
        (Some(_), true) => {
            return DcOut::from_vec(contracts::envelope_err(
                contracts::ErrorCode::InvalidHandle,
                "out_session is NULL",
            ))
        }
        (None, _) => {}
    }
    DcOut::from_vec(env)
}

#[no_mangle]
pub unsafe extern "C" fn dc_session_free(s: *mut DcSession) {
    if s.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| drop(Box::from_raw(s))));
}

#[no_mangle]
pub unsafe extern "C" fn dc_tick(s: *mut DcSession, tick_json: DcIn) -> DcOut {
    let inp = input(tick_json);
    DcOut::from_vec(guard(|| {
        ops::with_session(session(s), |ss| ops::tick(ss, inp))
    }))
}

#[no_mangle]
pub unsafe extern "C" fn dc_validate_override(s: *mut DcSession, req_json: DcIn) -> DcOut {
    let inp = input(req_json);
    DcOut::from_vec(guard(|| {
        ops::with_session(session(s), |ss| ops::validate_override(ss, inp))
    }))
}

#[no_mangle]
pub unsafe extern "C" fn dc_route_commit(s: *mut DcSession, route_bundle_json: DcIn) -> DcOut {
    let inp = input(route_bundle_json);
    DcOut::from_vec(guard(|| {
        ops::with_session(session(s), |ss| ops::route_commit(ss, inp))
    }))
}

#[no_mangle]
pub unsafe extern "C" fn dc_avoid_offer_eval(s: *mut DcSession, req_json: DcIn) -> DcOut {
    let inp = input(req_json);
    DcOut::from_vec(guard(|| {
        ops::with_session(session(s), |_| {
            ops::unimplemented("dc_avoid_offer_eval", inp)
        })
    }))
}

#[no_mangle]
pub unsafe extern "C" fn dc_plan_candidates(s: *mut DcSession, req_json: DcIn) -> DcOut {
    let inp = input(req_json);
    DcOut::from_vec(guard(|| {
        ops::with_session(session(s), |ss| ops::plan_candidates(ss, inp))
    }))
}

#[no_mangle]
pub unsafe extern "C" fn dc_rank(rank_req_json: DcIn) -> DcOut {
    let inp = input(rank_req_json);
    DcOut::from_vec(guard(|| ops::unimplemented("dc_rank", inp)))
}

#[no_mangle]
pub unsafe extern "C" fn dc_trip_score(trip_summary_json: DcIn) -> DcOut {
    let inp = input(trip_summary_json);
    DcOut::from_vec(guard(|| ops::unimplemented("dc_trip_score", inp)))
}

#[no_mangle]
pub unsafe extern "C" fn dc_exchange_validate(req_json: DcIn) -> DcOut {
    let inp = input(req_json);
    DcOut::from_vec(guard(|| ops::unimplemented("dc_exchange_validate", inp)))
}
