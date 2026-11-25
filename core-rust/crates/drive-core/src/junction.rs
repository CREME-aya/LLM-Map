//! Junction classification of a `Maneuver` from its Evidence (team decision 2026-11-26).
//!
//! No `kind` enum is added to `Maneuver`: the class is derived in Rust from `crossing`,
//! `approach_signal`, `right_turn_protection` and the widths. Class names follow
//! `CandidateFacts.counts` (schema/candidate-facts.schema.json; 03 §3.2 display axes 2 / 3):
//! `right_unprotected` / `right_signal_unknown_protection` / `right_signal_unknown` /
//! `right_protected` / `right_entry`, plus `merge` and `narrow` (CandidateFacts has
//! `counts.merges` and `narrow_m`, not these exact names).
//!
//! Evidence rule (team decision 2026-11-26, 04 §2.2): `confirmed` and `inferred` are used
//! asymmetrically. An `inferred` value is used only when it leans to the dangerous side; an
//! `inferred` value on the safe side is treated exactly like `unknown`.
//!
//! | field | dangerous side (inferred is used) | safe side (inferred = unknown) |
//! |---|---|---|
//! | `crossing` | `across_oncoming` → crosses | `no_crossing` / `not_applicable` → unknown |
//! | `approach_signal` | `absent` → no signal | `present` → unknown |
//! | `right_turn_protection` | `unprotected` → unprotected | `protected` → unknown |
//! | `departure_width_mm` | `< 4.0 m` → narrow | `>= 4.0 m` → unknown (not narrow) |
//!
//! - A maneuver crosses oncoming traffic when `crossing` is `across_oncoming` (confirmed or
//!   inferred), or when `crossing` is unknown by the rule above (`unknown`, or an inferred
//!   "no crossing") on a right-hand arrow (`right` / `slight_right` / `sharp_right` /
//!   `u_turn_right`): left-hand traffic, "対向横断が分からない右の矢印は横切る" (04 §2.2).
//!   Only a confirmed `no_crossing` / `not_applicable` rules the crossing out.
//! - Crossing maneuvers: `arrive` → `right_entry` (03 §3.2 "目的地への対向横断入口").
//!   Otherwise: signal absent (confirmed or inferred) or protection unprotected (confirmed
//!   or inferred) → `right_unprotected`; signal confirmed present and protection confirmed
//!   protected → `right_protected`; signal confirmed present otherwise →
//!   `right_signal_unknown_protection` (03 §3.2 "無タグ信号は保護不明"); signal not known
//!   (unknown, or inferred present) → `right_signal_unknown`.
//! - `merge` / `merge_left` / `merge_right` → `merge`.
//! - `departure_width_mm` < 4.0 m (03 §3.2 provisional narrow threshold), confirmed or
//!   inferred → `narrow` (entering a narrow road). A confirmed width >= 4.0 m is not narrow;
//!   an inferred width >= 4.0 m and an unknown width are "width unknown", not narrow either.
//!   TODO(04 §2.2): whether an unknown width on a residential road leans to narrow is
//!   undecided (`edges.flags` has no road class), so it is not narrow for now.
//!
//! A maneuver can be one crossing class and also `narrow` (e.g. a right turn into a narrow
//! road); `merge` and the crossing classes are independent too.
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

pub use contracts::horizon::JunctionClass;
use contracts::horizon::{Arrow, Evidence, EvidenceKind, Maneuver};

/// Narrow road threshold (03 §3.2: "狭い対面道路（暫定基準 4.0 m 未満）", provisional).
pub const NARROW_WIDTH_MM: u64 = 4_000;

fn value_is(e: &Evidence, kinds: &[EvidenceKind], value: &str) -> bool {
    kinds.contains(&e.kind) && e.value.as_ref().and_then(|v| v.as_str()) == Some(value)
}

/// `value` is established for a value on the safe side: confirmed only.
fn confirmed(e: &Evidence, value: &str) -> bool {
    value_is(e, &[EvidenceKind::Confirmed], value)
}

/// `value` is established for a value on the dangerous side: confirmed or inferred.
fn confirmed_or_inferred(e: &Evidence, value: &str) -> bool {
    value_is(e, &[EvidenceKind::Confirmed, EvidenceKind::Inferred], value)
}

fn right_hand(a: Arrow) -> bool {
    matches!(
        a,
        Arrow::Right | Arrow::SlightRight | Arrow::SharpRight | Arrow::UTurnRight
    )
}

/// Crosses oncoming traffic (see module docs): `across_oncoming` confirmed or inferred;
/// otherwise unless a confirmed "no crossing" rules it out, a right-hand arrow crosses.
fn crosses_oncoming(m: &Maneuver) -> bool {
    if confirmed_or_inferred(&m.crossing, "across_oncoming") {
        return true;
    }
    let ruled_out =
        confirmed(&m.crossing, "no_crossing") || confirmed(&m.crossing, "not_applicable");
    !ruled_out && right_hand(m.arrow)
}

fn crossing_class(m: &Maneuver) -> Option<JunctionClass> {
    if !crosses_oncoming(m) {
        return None;
    }
    Some(if m.arrow == Arrow::Arrive {
        JunctionClass::RightEntry
    } else if confirmed_or_inferred(&m.approach_signal, "absent")
        || confirmed_or_inferred(&m.right_turn_protection, "unprotected")
    {
        JunctionClass::RightUnprotected
    } else if confirmed(&m.approach_signal, "present") {
        if confirmed(&m.right_turn_protection, "protected") {
            JunctionClass::RightProtected
        } else {
            JunctionClass::RightSignalUnknownProtection
        }
    } else {
        JunctionClass::RightSignalUnknown
    })
}

/// Entering a narrow road: `departure_width_mm` < 4.0 m, confirmed or inferred (the
/// dangerous side). Any width >= 4.0 m, or none, is not narrow (see module docs).
fn narrow_entry(m: &Maneuver) -> bool {
    matches!(
        m.departure_width_mm.kind,
        EvidenceKind::Confirmed | EvidenceKind::Inferred
    ) && m
        .departure_width_mm
        .value
        .as_ref()
        .and_then(|v| v.as_u64())
        .is_some_and(|w| w < NARROW_WIDTH_MM)
}

/// All classes of `m`, in `JunctionClass` order (empty: an ordinary maneuver).
pub fn classify(m: &Maneuver) -> Vec<JunctionClass> {
    let mut out: Vec<JunctionClass> = crossing_class(m).into_iter().collect();
    if matches!(m.arrow, Arrow::Merge | Arrow::MergeLeft | Arrow::MergeRight) {
        out.push(JunctionClass::Merge);
    }
    if narrow_entry(m) {
        out.push(JunctionClass::Narrow);
    }
    out
}

/// Starts a complex window by itself (team decision 2026-11-26): a crossing of oncoming
/// traffic (every `right_*` class, `right_protected` and `right_entry` included), a merge,
/// or entering a narrow road. The other trigger — two or more maneuvers in one `group_id`
/// — needs the route and is applied by the horizon (`HorizonFrames`).
/// Every class is one of these, so this is "has any class".
pub fn is_complex_trigger(m: &Maneuver) -> bool {
    !classify(m).is_empty()
}

/// Target of the driving-time avoid chip (04 §4.3 "信号なし右折 / 狭路", widened by the team
/// decision 2026-11-26): `right_unprotected`, `right_signal_unknown_protection`,
/// `right_signal_unknown` or `narrow`. Merges are not targets.
pub fn is_avoid_target(m: &Maneuver) -> bool {
    classify(m).iter().any(|c| {
        matches!(
            c,
            JunctionClass::RightUnprotected
                | JunctionClass::RightSignalUnknownProtection
                | JunctionClass::RightSignalUnknown
                | JunctionClass::Narrow
        )
    })
}
