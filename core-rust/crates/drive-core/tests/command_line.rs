//! The Rust parser must accept exactly the language of the (provisional) GBNF.
//! Exhaustive check: enumerate every sentence the grammar generates and compare with
//! a set of near-miss strings that it must reject.
use drive_core::command_line::{parse, Command};

/// All sentences of `cmd` (without the trailing "\n"), generated from the grammar shape.
fn grammar_language() -> Vec<String> {
    let mut out: Vec<String> = ["CXL", "WHY", "BMK", "YES", "NO", "NOOP"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let slots: [(&str, &[&str]); 5] = [
        ("rc", &["L", "N", "H"]),
        ("nr", &["L", "N", "H"]),
        ("mg", &["L", "N", "H"]),
        ("dt", &["T", "S"]),
        ("jp", &["last", "next"]),
    ];
    // Every non-empty subset of keys in fixed order, every value combination.
    for mask in 1u32..32 {
        let mut acc = vec![String::from("OVR")];
        for (i, (k, vals)) in slots.iter().enumerate() {
            if mask & (1 << i) != 0 {
                acc = acc
                    .iter()
                    .flat_map(|p| vals.iter().map(move |v| format!("{p} {k}={v}")))
                    .collect();
            }
        }
        out.extend(acc);
    }
    out
}

/// The grammar the parser is checked against: the canonical `schema/slm/override.gbnf`
/// if it exists, otherwise the provisional copy of docs 03 §4.1.
/// TODO(gbnf): drop the fallback once schema/slm/override.gbnf is on the branch.
fn gbnf_source() -> (String, &'static str) {
    let canonical =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../schema/slm/override.gbnf");
    match std::fs::read_to_string(&canonical) {
        Ok(g) => (g, "schema/slm/override.gbnf"),
        Err(_) => (
            include_str!("fixtures/command_line.provisional.gbnf").to_owned(),
            "provisional copy",
        ),
    }
}

#[test]
fn gbnf_matches_the_shape_the_parser_implements() {
    let (g, src) = gbnf_source();
    let norm: String = g
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for needle in [
        r#"cmd ::= override | "CXL" | "WHY" | "BMK" | "YES" | "NO" | "NOOP""#,
        r#"override ::= "OVR" ( rc nr? mg? dt? jp? | nr mg? dt? jp? | mg dt? jp? | dt jp? | jp )"#,
        r#"dt ::= " dt=" ("T" | "S")"#,
        r#"jp ::= " jp=" ("last" | "next")"#,
        r#"level ::= "L" | "N" | "H""#,
    ] {
        assert!(norm.contains(needle), "{src}: missing {needle}");
    }
}

#[test]
fn accepts_whole_language() {
    let lang = grammar_language();
    assert_eq!(lang.len(), 6 + (4 * 4 * 4 * 3 * 3 - 1)); // 6 bare + (∏(n+1) − 1) OVR forms
    for s in &lang {
        assert!(parse(s).is_ok(), "should accept {s:?}");
        assert!(
            parse(&format!("{s}\n")).is_ok(),
            "should accept {s:?} + \\n"
        );
    }
}

#[test]
fn rejects_near_misses() {
    for s in [
        "",
        "OVR",
        "OVR ",
        "OVR rc=H ",
        "OVR  rc=H",
        " OVR rc=H",
        "OVR rc=H rc=L",
        "OVR nr=H rc=H",
        "OVR jp=next dt=T",
        "OVR dt=R",
        "OVR rc=X",
        "OVR rc=h",
        "OVR jp=prev",
        "OVR xx=H",
        "OVR rc",
        "OVRrc=H",
        "yes",
        "YES ",
        "NOOP\n\n",
        "OVR rc=H\r\n",
        "CXL WHY",
    ] {
        assert!(parse(s).is_err(), "should reject {s:?}");
    }
}

#[test]
fn parses_values() {
    match parse("OVR rc=H dt=T jp=next").unwrap() {
        Command::Ovr(a) => {
            assert!(a.rc.is_some() && a.nr.is_none() && a.dt.is_some() && a.jp.is_some());
        }
        c => panic!("{c:?}"),
    }
}
