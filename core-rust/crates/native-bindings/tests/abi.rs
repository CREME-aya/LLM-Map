//! C ABI v1 through the exported symbols (same code path as a C caller).
use dc_core::abi::*;
use serde_json::Value;
use std::ptr;

fn din(s: &[u8]) -> DcIn {
    DcIn {
        ptr: s.as_ptr(),
        len: s.len(),
    }
}

/// Copy out, free, parse.
unsafe fn take(o: DcOut) -> Value {
    assert!(!o.ptr.is_null());
    let v = serde_json::from_slice(std::slice::from_raw_parts(o.ptr, o.len)).unwrap();
    dc_buf_free(o);
    v
}

#[test]
fn abi_version_word() {
    assert_eq!(dc_abi_version(), 0x0001_0001);
}

#[test]
fn session_lifecycle_and_tick() {
    unsafe {
        let mut s = ptr::null_mut();
        let v = take(dc_session_new(din(b"{}"), &mut s));
        assert_eq!(v, serde_json::json!({"ok": true, "data": {}}));
        assert!(!s.is_null());
        // input is NOT NUL-terminated: pass a slice of a larger buffer
        let buf = br#"{"now_mono_ms":42}garbage"#;
        let v = take(dc_tick(
            s,
            DcIn {
                ptr: buf.as_ptr(),
                len: 18,
            },
        ));
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["data"]["planner_cmds"], serde_json::json!([]));
        assert_eq!(v["data"]["next_wake_mono_ms"], Value::Null);
        let v = take(dc_validate_override(s, din(br#"{"code_line":"OVR rc=H rc=L","contract":{},"utterance_id":"u1","utterance_mono_ms":1,"ref_frame_seq":1}"#)));
        assert_eq!(
            v["data"],
            serde_json::json!({"result":"rejected","code":"SYNTAX"})
        );
        // utterance_id is required (02 §2.1 / 03 §4.2)
        let v = take(dc_validate_override(
            s,
            din(br#"{"code_line":"YES","contract":{},"utterance_mono_ms":1,"ref_frame_seq":1}"#),
        ));
        assert_eq!(v["error"]["code"], "E_SCHEMA", "{v}");
        dc_session_free(s);
    }
}

#[test]
fn session_new_failure_sets_null() {
    unsafe {
        let mut s = ptr::dangling_mut();
        let v = take(dc_session_new(din(br#"{"unknown":1}"#), &mut s));
        assert_eq!(v["error"]["code"], "E_SCHEMA");
        assert!(s.is_null());
        let v = take(dc_session_new(din(b"{}"), ptr::null_mut()));
        assert_eq!(v["error"]["code"], "E_INVALID_HANDLE");
    }
}

#[test]
fn null_handle_and_bad_json() {
    unsafe {
        let v = take(dc_tick(ptr::null_mut(), din(br#"{"now_mono_ms":1}"#)));
        assert_eq!(v["error"]["code"], "E_INVALID_HANDLE");
        let mut s = ptr::null_mut();
        take(dc_session_new(din(b"{}"), &mut s));
        for bad in [
            &b""[..],
            b"{",
            br#"{"now_mono_ms":"1"}"#,
            br#"{"now_mono_ms":1,"events":[{"at_mono_ms":1,"kind":"search_cancelled"}]}"#,
        ] {
            let v = take(dc_tick(s, din(bad)));
            assert_eq!(
                v["error"]["code"],
                "E_SCHEMA",
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let v = take(dc_tick(
            s,
            DcIn {
                ptr: ptr::null(),
                len: 0,
            },
        ));
        assert_eq!(v["error"]["code"], "E_SCHEMA");
        dc_session_free(s);
        dc_session_free(ptr::null_mut()); // NULL is safe
        dc_buf_free(DcOut {
            ptr: ptr::null_mut(),
            len: 0,
        }); // no-op
    }
}

#[test]
fn unimplemented_functions_are_explicit() {
    unsafe {
        for v in [
            take(dc_plan_candidates(din(b"{}"))),
            take(dc_rank(din(b"{}"))),
            take(dc_trip_score(din(b"{}"))),
            take(dc_exchange_validate(din(b"{}"))),
        ] {
            assert_eq!(v["error"]["code"], "E_UNIMPLEMENTED");
        }
        assert_eq!(take(dc_rank(din(b"nope")))["error"]["code"], "E_SCHEMA");
        assert_eq!(
            take(dc_route_commit(ptr::null_mut(), din(b"{}")))["error"]["code"],
            "E_INVALID_HANDLE"
        );
        assert_eq!(
            take(dc_avoid_offer_eval(ptr::null_mut(), din(b"{}")))["error"]["code"],
            "E_INVALID_HANDLE"
        );
    }
}

#[test]
fn same_input_same_bytes() {
    // Determinism: identical input + identical now_mono_ms → identical output bytes.
    unsafe fn run() -> Vec<u8> {
        let mut s = ptr::null_mut();
        take(dc_session_new(din(b"{}"), &mut s));
        let o = dc_tick(s, din(br#"{"now_mono_ms":7,"events":[{"at_mono_ms":5,"kind":"thermal","status":"critical"}]}"#));
        let v = std::slice::from_raw_parts(o.ptr, o.len).to_vec();
        dc_buf_free(o);
        dc_session_free(s);
        v
    }
    unsafe { assert_eq!(run(), run()) };
}
