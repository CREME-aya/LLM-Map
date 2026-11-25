//! Junction classification, `next_complex_window` and the P3 available time (team decision
//! 2026-11-26; 04 §4.1 preparation leads, 05 §6.2 formula).
use contracts::horizon::{
    Arrow, ComplexWindow, Evidence, HorizonStatus, Maneuver, ManeuverHorizon,
};
use contracts::route::{RouteBundle, RouteCommitRequest, EDGE_FLAG_MOTORWAY};
use contracts::tick::{Fix, TickInput, TickOutput};
use drive_core::horizon::{p3_available_in, p3_available_ms};
use drive_core::junction::{classify, is_avoid_target, is_complex_trigger, JunctionClass};
use drive_core::{Session, SessionConfig};
use serde_json::{json, Value};

use JunctionClass::*;

// ---- helpers ---------------------------------------------------------------------------

/// Main line of the Gate 7 fixtures: 4 km east at 15 m/s, left 1.5 km, right 2.5 km
/// (all Evidence unknown), left 3.5 km, arrive 4 km.
fn main_line() -> RouteBundle {
    let p = format!(
        "{}/../../tests/fixtures/route/gate7-side-road-20m.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    serde_json::from_value(v["bundle"].clone()).unwrap()
}

fn ev(kind: &str, value: Value) -> Evidence {
    let v = match kind {
        "unknown" => json!({"kind": "unknown", "reason": "missing"}),
        "confirmed" => json!({"kind": "confirmed", "value": value, "source_ref": "t"}),
        _ => {
            json!({"kind": "inferred", "value": value, "rule_id": "r", "source_refs": ["osm:way/1"]})
        }
    };
    serde_json::from_value(v).unwrap()
}

fn c(v: &str) -> Evidence {
    ev("confirmed", json!(v))
}

fn unknown() -> Evidence {
    ev("unknown", Value::Null)
}

fn fix_x(b: &RouteBundle, at: u64, x_cm: i64) -> Fix {
    let ch = &b.legs[0].chunks[0];
    Fix {
        at_mono_ms: at,
        lat_e7: ch.origin_e7.lat,
        lon_e7: ch.origin_e7.lon + (x_cm * 65_536 / i64::from(ch.k_lon_q16)) as i32,
        acc_cm: 500,
        bearing_cdeg: Some(9_000),
        speed_cm_s: Some(1_500),
    }
}

fn tick(s: &mut Session, now: u64, fix: Option<Fix>) -> TickOutput {
    s.tick(TickInput {
        now_mono_ms: now,
        fix,
        events: vec![],
    })
}

fn session(b: &RouteBundle) -> Session {
    let mut s = Session::new(SessionConfig {
        fix_stale_ms: 1_000_000,
        ..Default::default()
    });
    s.route_commit(&RouteCommitRequest {
        expected_route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    s
}

fn make_right_unprotected(m: &mut Maneuver) {
    m.arrow = Arrow::Right;
    m.crossing = c("across_oncoming");
    m.approach_signal = c("absent");
}

fn plain(m: &mut Maneuver, arrow: Arrow) {
    m.arrow = arrow;
    m.crossing = unknown();
    m.approach_signal = unknown();
    m.right_turn_protection = unknown();
    m.approach_width_mm = unknown();
    m.departure_width_mm = unknown();
}

/// The window expected from entries `first` and `last` of a tracking frame.
fn expected(h: &ManeuverHorizon, first: usize, last: usize, prep: u64) -> ComplexWindow {
    let s = h.sampled_at_monotonic_ms;
    let e0 = h.upcoming[first].eta_range_ms.unwrap();
    let e1 = h.upcoming[last].eta_range_ms.unwrap();
    ComplexWindow {
        start_monotonic_ms: (s + u64::from(e0.earliest)).saturating_sub(prep),
        end_monotonic_ms: Some(s + u64::from(e1.latest) + 5_000),
    }
}

// ---- classification ----------------------------------------------------------------------

/// Oracle for the crossing class, written as the decision table (asymmetric Evidence rule:
/// inferred `absent` / `unprotected` are used, inferred `present` / `protected` are unknown).
fn expected_crossing(signal: (&str, &str), protection: (&str, &str)) -> JunctionClass {
    match (signal, protection) {
        ((_, "absent"), _) | (_, (_, "unprotected")) => RightUnprotected,
        (("confirmed", "present"), ("confirmed", "protected")) => RightProtected,
        (("confirmed", "present"), _) => RightSignalUnknownProtection,
        _ => RightSignalUnknown,
    }
}

fn evidence(kv: (&str, &str)) -> Evidence {
    if kv.0 == "unknown" {
        unknown()
    } else {
        ev(kv.0, json!(kv.1))
    }
}

#[test]
fn classification_covers_every_evidence_combination() {
    let base = main_line().legs[0].maneuvers[0].clone();
    // None: unknown by the asymmetric rule → crossing only on a right-hand arrow.
    let crossings: [((&str, &str), Option<bool>); 7] = [
        (("confirmed", "across_oncoming"), Some(true)),
        (("inferred", "across_oncoming"), Some(true)),
        (("confirmed", "no_crossing"), Some(false)),
        (("confirmed", "not_applicable"), Some(false)),
        // inferred "no crossing" is the safe side: treated as unknown
        (("inferred", "no_crossing"), None),
        (("inferred", "not_applicable"), None),
        (("unknown", ""), None),
    ];
    let signals = [
        ("confirmed", "present"),
        ("confirmed", "absent"),
        ("inferred", "present"),
        ("inferred", "absent"),
        ("unknown", ""),
    ];
    let protections = [
        ("confirmed", "protected"),
        ("confirmed", "unprotected"),
        ("inferred", "protected"),
        ("inferred", "unprotected"),
        ("unknown", ""),
    ];
    let arrows = [
        (Arrow::Right, true),
        (Arrow::SlightRight, true),
        (Arrow::SharpRight, true),
        (Arrow::UTurnRight, true),
        (Arrow::Left, false),
        (Arrow::KeepRight, false),
        (Arrow::ExitRight, false),
        (Arrow::StraightDecision, false),
        (Arrow::Merge, false),
        (Arrow::MergeLeft, false),
        (Arrow::MergeRight, false),
        (Arrow::Arrive, false),
    ];
    let widths: [((&str, u64), bool); 8] = [
        (("confirmed", 3_999), true),
        (("confirmed", 4_000), false),
        (("inferred", 2_500), true),
        (("inferred", 3_999), true),
        // inferred >= 4.0 m is the safe side: width unknown, not narrow
        (("inferred", 4_000), false),
        (("inferred", 4_001), false),
        (("unknown", 0), false),
        // approach narrow only: not entering a narrow road
        (("approach", 2_000), false),
    ];
    let mut n = 0;
    for (cr, cr_fixed) in crossings {
        for sig in signals {
            for prot in protections {
                for (arrow, right_hand) in arrows {
                    for (w, narrow) in widths {
                        let mut m = base.clone();
                        m.arrow = arrow;
                        m.crossing = evidence(cr);
                        m.approach_signal = evidence(sig);
                        m.right_turn_protection = evidence(prot);
                        m.approach_width_mm = unknown();
                        m.departure_width_mm = match w.0 {
                            "unknown" => unknown(),
                            "approach" => {
                                m.approach_width_mm = ev("confirmed", json!(w.1));
                                unknown()
                            }
                            k => ev(k, json!(w.1)),
                        };
                        let crosses = cr_fixed.unwrap_or(right_hand);
                        let mut want = Vec::new();
                        if crosses {
                            want.push(if arrow == Arrow::Arrive {
                                RightEntry
                            } else {
                                expected_crossing(sig, prot)
                            });
                        }
                        if matches!(arrow, Arrow::Merge | Arrow::MergeLeft | Arrow::MergeRight) {
                            want.push(Merge);
                        }
                        if narrow {
                            want.push(Narrow);
                        }
                        let got = classify(&m);
                        assert_eq!(got, want, "{arrow:?} {cr:?} {sig:?} {prot:?} {w:?}");
                        assert_eq!(is_complex_trigger(&m), !want.is_empty());
                        let avoid = want.iter().any(|k| {
                            matches!(
                                k,
                                RightUnprotected
                                    | RightSignalUnknownProtection
                                    | RightSignalUnknown
                                    | Narrow
                            )
                        });
                        assert_eq!(is_avoid_target(&m), avoid, "{got:?}");
                        n += 1;
                    }
                }
            }
        }
    }
    assert_eq!(n, 7 * 5 * 5 * 12 * 8);
}

/// The two predicates per class (team decision 2026-11-26): every class starts a complex
/// window; merge and right_protected / right_entry are not avoid-chip targets. Names follow
/// CandidateFacts.counts.
#[test]
fn predicates_and_names_per_class() {
    let base = main_line().legs[0].maneuvers[0].clone();
    let one = |f: &dyn Fn(&mut Maneuver)| {
        let mut m = base.clone();
        plain(&mut m, Arrow::Left);
        f(&mut m);
        m
    };
    let cases: Vec<(JunctionClass, &str, Maneuver, bool)> = vec![
        (
            RightUnprotected,
            "right_unprotected",
            one(&make_right_unprotected),
            true,
        ),
        (
            RightSignalUnknownProtection,
            "right_signal_unknown_protection",
            one(&|m| {
                m.arrow = Arrow::Right;
                m.crossing = c("across_oncoming");
                m.approach_signal = c("present");
            }),
            true,
        ),
        (
            RightSignalUnknown,
            "right_signal_unknown",
            one(&|m| m.arrow = Arrow::Right),
            true,
        ),
        (
            RightProtected,
            "right_protected",
            one(&|m| {
                m.arrow = Arrow::Right;
                m.crossing = c("across_oncoming");
                m.approach_signal = c("present");
                m.right_turn_protection = c("protected");
            }),
            false,
        ),
        (
            RightEntry,
            "right_entry",
            one(&|m| {
                m.arrow = Arrow::Arrive;
                m.crossing = c("across_oncoming");
            }),
            false,
        ),
        (Merge, "merge", one(&|m| m.arrow = Arrow::MergeRight), false),
        (
            Narrow,
            "narrow",
            one(&|m| m.departure_width_mm = ev("confirmed", json!(3_000))),
            true,
        ),
    ];
    for (class, name, m, avoid) in cases {
        assert_eq!(classify(&m), vec![class], "{name}");
        assert_eq!(class.as_str(), name);
        assert!(is_complex_trigger(&m), "{name}");
        assert_eq!(is_avoid_target(&m), avoid, "{name}");
    }
    // ordinary maneuvers: no class, neither predicate
    for arrow in [
        Arrow::Left,
        Arrow::KeepRight,
        Arrow::ForkRight,
        Arrow::StraightDecision,
    ] {
        let m = one(&|m| m.arrow = arrow);
        assert!(classify(&m).is_empty(), "{arrow:?}");
        assert!(!is_complex_trigger(&m) && !is_avoid_target(&m));
    }
    let m = one(&|m| {
        m.arrow = Arrow::Right;
        m.crossing = c("no_crossing");
    });
    assert!(classify(&m).is_empty());
    // a right turn into a narrow road: both classes
    let m = one(&|m| {
        make_right_unprotected(m);
        m.departure_width_mm = ev("inferred", json!(3_500));
    });
    assert_eq!(classify(&m), vec![RightUnprotected, Narrow]);
}

/// The asymmetric Evidence rule, one field at a time (team decision 2026-11-26, 04 §2.2):
/// an inferred value on the dangerous side is used, on the safe side it equals unknown.
#[test]
fn inferred_is_used_only_on_the_dangerous_side() {
    let base = main_line().legs[0].maneuvers[0].clone();
    let inf = |v: &str| ev("inferred", json!(v));
    let right = |f: &dyn Fn(&mut Maneuver)| {
        let mut m = base.clone();
        plain(&mut m, Arrow::Right);
        f(&mut m);
        classify(&m)
    };
    // crossing: inferred across_oncoming crosses (even on a left arrow) ...
    let mut m = base.clone();
    plain(&mut m, Arrow::Left);
    m.crossing = inf("across_oncoming");
    assert_eq!(classify(&m), vec![RightSignalUnknown]);
    // ... inferred no_crossing / not_applicable is unknown: a right arrow still crosses,
    // a left arrow does not; only a confirmed value rules the crossing out.
    for v in ["no_crossing", "not_applicable"] {
        assert_eq!(
            right(&|m| m.crossing = inf(v)),
            right(&|m| m.crossing = unknown())
        );
        assert_eq!(
            right(&|m| m.crossing = inf(v)),
            vec![RightSignalUnknown],
            "{v}"
        );
        assert!(right(&|m| m.crossing = c(v)).is_empty(), "{v}");
        let mut m = base.clone();
        plain(&mut m, Arrow::Left);
        m.crossing = inf(v);
        assert!(classify(&m).is_empty(), "{v}");
    }
    // approach_signal: inferred absent → no signal → right_unprotected;
    // inferred present = unknown → right_signal_unknown (not ..._protection).
    assert_eq!(
        right(&|m| m.approach_signal = inf("absent")),
        vec![RightUnprotected]
    );
    assert_eq!(
        right(&|m| m.approach_signal = inf("present")),
        right(&|m| m.approach_signal = unknown())
    );
    assert_eq!(
        right(&|m| m.approach_signal = inf("present")),
        vec![RightSignalUnknown]
    );
    // right_turn_protection: inferred unprotected is used (with any signal evidence) ...
    for sig in [c("present"), inf("present"), unknown()] {
        let got = right(&|m| {
            m.approach_signal = sig.clone();
            m.right_turn_protection = inf("unprotected");
        });
        assert_eq!(got, vec![RightUnprotected]);
    }
    // ... inferred protected = unknown: a confirmed signal stays "protection unknown".
    let got = right(&|m| {
        m.approach_signal = c("present");
        m.right_turn_protection = inf("protected");
    });
    assert_eq!(got, vec![RightSignalUnknownProtection]);
    let got = right(&|m| {
        m.approach_signal = c("present");
        m.right_turn_protection = c("protected");
    });
    assert_eq!(got, vec![RightProtected]);
    // width: inferred < 4.0 m is narrow, inferred >= 4.0 m is unknown (not narrow).
    let mut m = base.clone();
    plain(&mut m, Arrow::Left);
    m.departure_width_mm = ev("inferred", json!(3_999));
    assert_eq!(classify(&m), vec![Narrow]);
    m.departure_width_mm = ev("inferred", json!(4_000));
    assert!(classify(&m).is_empty());
}

// ---- next_complex_window ---------------------------------------------------------------

/// Ordinary road: start = earliest of the first trigger − 30 s, end = latest + 5 s. The
/// first upcoming maneuver (left, ordinary) does not start it; the right turn with unknown
/// Evidence (right_signal_unknown) does.
#[test]
fn window_on_an_ordinary_road() {
    let b = main_line();
    let mut s = session(&b);
    let h = tick(&mut s, 1_000, Some(fix_x(&b, 1_000, 10_000)))
        .horizon
        .unwrap();
    assert_eq!(h.status, HorizonStatus::Tracking);
    assert_eq!(h.upcoming[0].maneuver.arrow, Arrow::Left);
    assert_eq!(h.next_complex_window, Some(expected(&h, 1, 1, 30_000)));
    // the first maneuver as right_unprotected: the window moves to it
    let mut b = main_line();
    make_right_unprotected(&mut b.legs[0].maneuvers[0]);
    let mut s = session(&b);
    let h = tick(&mut s, 1_000, Some(fix_x(&b, 1_000, 10_000)))
        .horizon
        .unwrap();
    let w = expected(&h, 0, 0, 30_000);
    assert_eq!(h.next_complex_window, Some(w));
    // numbers: 1.5 km - 0.1 km at 15 m/s = 93.3 s typical; start from earliest (× 0.85),
    // end from latest (× 1.25)
    let r = h.upcoming[0].eta_range_ms.unwrap();
    let typical = u64::from(r.typical);
    assert!((93_000..93_700).contains(&typical), "{typical}");
    assert_eq!(u64::from(r.earliest), typical * 850 / 1_000);
    assert_eq!(u64::from(r.latest), (typical * 1_250).div_ceil(1_000));
    assert_eq!(w.start_monotonic_ms, 1_000 + typical * 850 / 1_000 - 30_000);
    assert_eq!(
        w.end_monotonic_ms,
        Some(1_000 + (typical * 1_250).div_ceil(1_000) + 5_000)
    );
}

/// `eta_range_ms` is a band around the remaining time (04 §2.2, provisional 0.85 / 1.25 in
/// per-mille, set through SessionConfig); the factors apply to the remaining time from the
/// current position, not to `cum_time_ms` from the route start. The window follows the band.
#[test]
fn eta_range_is_a_band_on_the_remaining_time() {
    let b = grouped_line(12_000); // 2 ms per cm
    let mut s = session(&b);
    // at ~500 m: 1000 m (200 s) and 1120 m (224 s) from the route start, ~100 s / ~124 s
    // left; the factors multiply what is left
    let h = tick(&mut s, 50_000, Some(fix_x(&b, 50_000, 50_000)))
        .horizon
        .unwrap();
    let band = |t: u32| {
        let t = u64::from(t);
        ((t * 850 / 1_000) as u32, (t * 1_250).div_ceil(1_000) as u32)
    };
    let r0 = h.upcoming[0].eta_range_ms.unwrap();
    let r1 = h.upcoming[1].eta_range_ms.unwrap();
    assert!((99_990..100_010).contains(&r0.typical), "{r0:?}");
    assert_eq!(r1.typical - r0.typical, 24_000);
    assert_eq!((r0.earliest, r0.latest), band(r0.typical));
    assert_eq!((r1.earliest, r1.latest), band(r1.typical));
    assert!(r0.earliest < r0.typical && r0.typical < r0.latest);
    let w = h.next_complex_window.unwrap();
    assert_eq!(
        w.start_monotonic_ms,
        50_000 + u64::from(r0.earliest) - 30_000
    );
    assert_eq!(
        w.end_monotonic_ms,
        Some(50_000 + u64::from(r1.latest) + 5_000)
    );
    // other factors from the config; rounding: earliest down, latest up
    let mut s = Session::new(SessionConfig {
        fix_stale_ms: 1_000_000,
        eta_earliest_permille: 901,
        eta_latest_permille: 1_001,
        ..Default::default()
    });
    s.route_commit(&RouteCommitRequest {
        expected_route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    let h = tick(&mut s, 50_000, Some(fix_x(&b, 50_000, 50_000)))
        .horizon
        .unwrap();
    let r = h.upcoming[0].eta_range_ms.unwrap();
    let t = u64::from(r.typical);
    assert_eq!(u64::from(r.earliest), t * 901 / 1_000);
    assert_eq!(u64::from(r.latest), (t * 1_001).div_ceil(1_000));
    if t % 1_000 != 0 {
        assert!(u64::from(r.latest) * 1_000 > t * 1_001, "rounded up");
    }
    let w = h.next_complex_window.unwrap();
    assert_eq!(
        w.start_monotonic_ms,
        50_000 + u64::from(r.earliest) - 30_000
    );
    // factors 1000 / 1000: one value
    let mut s = Session::new(SessionConfig {
        fix_stale_ms: 1_000_000,
        eta_earliest_permille: 1_000,
        eta_latest_permille: 1_000,
        ..Default::default()
    });
    s.route_commit(&RouteCommitRequest {
        expected_route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    let h = tick(&mut s, 50_000, Some(fix_x(&b, 50_000, 50_000)))
        .horizon
        .unwrap();
    let r0 = h.upcoming[0].eta_range_ms.unwrap();
    assert_eq!((r0.earliest, r0.latest), (r0.typical, r0.typical));
    // factors on the wrong side of 1000 are rejected
    for cfg in [
        r#"{"eta_earliest_permille": 1001}"#,
        r#"{"eta_latest_permille": 999}"#,
    ] {
        assert!(Session::from_json(cfg.as_bytes()).is_err(), "{cfg}");
    }
}

/// Motorway (approach edge with the motorway flag): the lead is 60 s. Only the edge that
/// approaches the anchor decides.
#[test]
fn window_on_a_motorway() {
    let mut b = main_line();
    make_right_unprotected(&mut b.legs[0].maneuvers[0]);
    let anchor = b.legs[0].maneuvers[0].anchor.along_route_cm;
    let mut seg_along = Vec::new();
    for ch in &b.legs[0].chunks {
        seg_along.extend_from_slice(&ch.seg_along_cm);
    }
    for e in &mut b.legs[0].edges {
        let (a0, a1) = (seg_along[e.from_pt as usize], seg_along[e.to_pt as usize]);
        if a0 < anchor && anchor <= a1 {
            e.flags |= EDGE_FLAG_MOTORWAY;
        }
    }
    let mut s = session(&b);
    let h = tick(&mut s, 1_000, Some(fix_x(&b, 1_000, 10_000)))
        .horizon
        .unwrap();
    assert_eq!(h.next_complex_window, Some(expected(&h, 0, 0, 60_000)));
    // the start never goes below 0 (an anchor closer than the lead)
    let mut s = session(&b);
    let h = tick(&mut s, 1_000, Some(fix_x(&b, 1_000, 140_000)))
        .horizon
        .unwrap();
    let w = h.next_complex_window.unwrap();
    assert_eq!(w.start_monotonic_ms, 0);
    assert_eq!(w, expected(&h, 0, 0, 60_000));
}

/// Main line with the first three maneuvers turned into ordinary left turns, the second
/// `gap` cm after the first (city, 2 ms per cm: groups at <= 120 m).
fn grouped_line(gap: u32) -> RouteBundle {
    let mut b = main_line();
    let leg = &mut b.legs[0];
    let mut cumulative_cm = 0;
    for edge in &mut leg.edges {
        cumulative_cm += edge.length_cm;
        edge.cum_time_ms = cumulative_cm * 2;
    }
    b.eta_ms = leg.edges.last().unwrap().cum_time_ms;
    let mut seg_along = Vec::new();
    for ch in &leg.chunks {
        seg_along.extend_from_slice(&ch.seg_along_cm);
    }
    for (m, along) in leg
        .maneuvers
        .iter_mut()
        .zip([100_000, 100_000 + gap, 300_000])
    {
        plain(m, Arrow::Left);
        m.anchor.leg_shape_index = (seg_along.partition_point(|a| *a <= along) - 1) as u32;
        m.anchor.along_route_cm = along;
    }
    b
}

/// Two maneuvers in one group_id: one window from the first one's start to the second
/// one's end, even when neither has a class. Not grouped (gap > 120 m and > 12 s): none.
#[test]
fn a_group_is_one_window() {
    let b = grouped_line(12_000);
    let mut s = session(&b);
    let h = tick(&mut s, 50_000, Some(fix_x(&b, 50_000, 0)))
        .horizon
        .unwrap();
    assert!(h.upcoming[0].group_id.is_some());
    assert_eq!(h.upcoming[1].group_id, h.upcoming[0].group_id);
    assert!(classify(&h.upcoming[0].maneuver).is_empty());
    assert_eq!(h.next_complex_window, Some(expected(&h, 0, 1, 30_000)));
    // a right turn inside the group does not split it
    let mut b2 = grouped_line(12_000);
    make_right_unprotected(&mut b2.legs[0].maneuvers[1]);
    let mut s = session(&b2);
    let h = tick(&mut s, 50_000, Some(fix_x(&b2, 50_000, 0)))
        .horizon
        .unwrap();
    assert_eq!(h.next_complex_window, Some(expected(&h, 0, 1, 30_000)));
    // not grouped, no class: no window
    let b = grouped_line(12_001 + 12_000);
    let mut s = session(&b);
    let h = tick(&mut s, 50_000, Some(fix_x(&b, 50_000, 0)))
        .horizon
        .unwrap();
    assert!(h.upcoming[0].group_id.is_none());
    assert_eq!(h.next_complex_window, None);
}

/// Once its last maneuver is passed the window stays until its end (inclusive), with a
/// wake-up at end + 1; then the next window (none here) follows. The window kept is the
/// last one emitted before the passage (windows follow the ETA on every tracking frame).
#[test]
fn window_stays_until_its_end_after_passage() {
    let b = grouped_line(12_000);
    let mut s = session(&b);
    let h = tick(&mut s, 50_000, Some(fix_x(&b, 50_000, 0)))
        .horizon
        .unwrap();
    let w0 = h.next_complex_window.unwrap();
    // typical 224 s to the second anchor; the end uses latest = 224 s × 1.25 = 280 s
    assert_eq!(h.upcoming[1].eta_range_ms.unwrap().typical, 224_000);
    assert_eq!(w0.end_monotonic_ms, Some(50_000 + 280_000 + 5_000));
    // beyond the second anchor (1120 m) once: not passed yet, the window follows the ETA
    let h = tick(&mut s, 100_000, Some(fix_x(&b, 100_000, 113_000)))
        .horizon
        .unwrap();
    let w = h.next_complex_window.unwrap();
    assert_eq!(w, expected(&h, 0, 1, 30_000));
    let end = w.end_monotonic_ms.unwrap();
    assert_eq!(end, 100_000 + 5_000, "ETA 0 to the anchor behind");
    // twice: both maneuvers passed, the window is kept
    let o = tick(&mut s, 101_000, Some(fix_x(&b, 101_000, 113_100)));
    let h = o.horizon.unwrap();
    assert_eq!(h.upcoming[0].maneuver.anchor.along_route_cm, 300_000);
    assert_eq!(h.next_complex_window, Some(w), "kept after passage");
    assert_eq!(o.next_wake_mono_ms, Some(end + 1));
    assert!(
        tick(&mut s, end, None).horizon.is_none(),
        "still kept at end"
    );
    let h = tick(&mut s, end + 1, None).horizon.unwrap();
    assert_eq!(h.next_complex_window, None);
}

/// Only while tracking: uncertain, off_route / rerouting and finished frames carry none.
#[test]
fn window_only_while_tracking() {
    let mut b = main_line();
    make_right_unprotected(&mut b.legs[0].maneuvers[0]);
    let mut s = Session::new(SessionConfig::default());
    s.route_commit(&RouteCommitRequest {
        expected_route_key: None,
        bundle: b.clone(),
    })
    .unwrap();
    let mut frames = vec![tick(&mut s, 0, None).horizon.unwrap()];
    frames.push(
        tick(&mut s, 1_000, Some(fix_x(&b, 1_000, 10_000)))
            .horizon
            .unwrap(),
    );
    // fresh limit 2.5 s → uncertain
    frames.push(tick(&mut s, 3_600, None).horizon.unwrap());
    let mut off = fix_x(&b, 4_000, 10_000);
    for t in [4_000u64, 5_000, 6_000] {
        off.at_mono_ms = t;
        off.lat_e7 = b.legs[0].chunks[0].origin_e7.lat + 100_000;
        if let Some(h) = tick(&mut s, t, Some(off)).horizon {
            frames.push(h);
        }
    }
    for h in &frames {
        assert_eq!(
            h.next_complex_window.is_some(),
            h.status == HorizonStatus::Tracking,
            "{:?}",
            h.status
        );
    }
    let statuses: Vec<_> = frames.iter().map(|h| h.status).collect();
    for st in [
        HorizonStatus::Uncertain,
        HorizonStatus::Tracking,
        HorizonStatus::Rerouting,
    ] {
        assert!(statuses.contains(&st), "{statuses:?}");
    }
}

// ---- P3 available time (05 §6.2) ---------------------------------------------------------

#[test]
fn p3_available_time_formula() {
    let w = |start| ComplexWindow {
        start_monotonic_ms: start,
        end_monotonic_ms: Some(start + 60_000),
    };
    // min(P1 deadline, window start) - TTS preparation - 5 s, from now
    assert_eq!(
        p3_available_ms(1_000, Some(41_000), Some(&w(61_000)), 2_000),
        Some(33_000)
    );
    assert_eq!(
        p3_available_ms(1_000, Some(61_000), Some(&w(41_000)), 2_000),
        Some(33_000)
    );
    assert_eq!(p3_available_ms(1_000, Some(41_000), None, 0), Some(35_000));
    assert_eq!(
        p3_available_ms(1_000, None, Some(&w(41_000)), 0),
        Some(35_000)
    );
    // nothing ahead: not bounded by the horizon
    assert_eq!(p3_available_ms(1_000, None, None, 2_000), None);
    // not enough left, or inside the window: 0
    assert_eq!(p3_available_ms(1_000, Some(7_000), None, 2_000), Some(0));
    assert_eq!(p3_available_ms(50_000, None, Some(&w(41_000)), 0), Some(0));
    // the 12 s short version fits in 12 s exactly
    assert_eq!(p3_available_ms(0, Some(19_000), None, 2_000), Some(12_000));
}

/// From a horizon frame: the window start bounds the time before the P1 deadline does.
#[test]
fn p3_available_time_from_a_frame() {
    let mut b = main_line();
    make_right_unprotected(&mut b.legs[0].maneuvers[2]);
    for m in &mut b.legs[0].maneuvers[..2] {
        plain(m, Arrow::Left);
    }
    let mut s = session(&b);
    let h = tick(&mut s, 1_000, Some(fix_x(&b, 1_000, 10_000)))
        .horizon
        .unwrap();
    let w = h.next_complex_window.unwrap();
    let p1 = h.next_p1_deadline_monotonic_ms.unwrap();
    assert!(p1 < w.start_monotonic_ms);
    assert_eq!(
        p3_available_in(&h, 1_000, 1_500),
        Some(p1 - 1_000 - 1_500 - 5_000)
    );
    // a right_unprotected first maneuver: the window start (30 s lead) comes first
    let mut b = main_line();
    make_right_unprotected(&mut b.legs[0].maneuvers[0]);
    let mut s = session(&b);
    let h = tick(&mut s, 1_000, Some(fix_x(&b, 1_000, 10_000)))
        .horizon
        .unwrap();
    let w = h.next_complex_window.unwrap();
    assert!(w.start_monotonic_ms < h.next_p1_deadline_monotonic_ms.unwrap());
    assert_eq!(
        p3_available_in(&h, 1_000, 1_500),
        Some(w.start_monotonic_ms - 1_000 - 1_500 - 5_000)
    );
}
