//! Determinism guard (docs 04 §3.1): alignment, bearing comparison and Horizon generation
//! use integers only — no f32 / f64, no float literals, no float math functions. The
//! alignment modules also carry `#![deny(clippy::float_arithmetic, ...)]`, so
//! `cargo clippy -D warnings` fails on any float arithmetic there.
use std::path::Path;

/// Files on the alignment / horizon path: fixed-point conversion and projection (geo),
/// route index and ETA (route), alignment and bearing comparison (align), horizon (horizon),
/// junction classes for the complex window (junction), and the session glue that feeds them
/// (session, planner).
const FILES: [&str; 7] = [
    "geo.rs",
    "route.rs",
    "align.rs",
    "horizon.rs",
    "junction.rs",
    "session.rs",
    "planner.rs",
];
const LINT_GUARDED: [&str; 5] = [
    "geo.rs",
    "route.rs",
    "align.rs",
    "horizon.rs",
    "junction.rs",
];

/// Source without comments and string / char literals (good enough for this code base).
fn code_only(src: &str) -> String {
    let mut out = String::new();
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' {
            i += 1;
            while i < b.len() && b[i] != '"' {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push_str("\"\"");
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn tokens(code: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut cur = String::new();
    for c in code.chars() {
        if c.is_alphanumeric() || c == '_' || c == '.' {
            cur.push(c);
        } else if !cur.is_empty() {
            v.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        v.push(cur);
    }
    v
}

/// `1.4`, `0.5`, `2.` style literals: a token starting with digits followed by `.digit`
/// (tuple fields such as `c0.0` or `p.1` and ranges such as `0..3` are not floats).
fn is_float_literal(t: &str) -> bool {
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() > 1
        && !parts[0].is_empty()
        && parts[0].chars().all(|c| c.is_ascii_digit() || c == '_')
        && parts[1].starts_with(|c: char| c.is_ascii_digit())
}

#[test]
fn alignment_and_horizon_code_has_no_floating_point() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let banned_words = ["f32", "f64"];
    let banned_methods = [
        "sqrt",
        "sin",
        "cos",
        "tan",
        "atan",
        "atan2",
        "asin",
        "acos",
        "hypot",
        "to_radians",
        "to_degrees",
        "powf",
        "powi",
        "exp",
        "ln",
        "log10",
        "round",
        "floor",
        "ceil",
    ];
    for f in FILES {
        let src = std::fs::read_to_string(src_dir.join(f)).unwrap();
        let code = code_only(&src);
        for t in tokens(&code) {
            for part in t.split('.') {
                let typed = banned_words.contains(&part)
                    || (part.starts_with(|c: char| c.is_ascii_digit())
                        && (part.ends_with("f32") || part.ends_with("f64")));
                assert!(!typed, "{f}: `{t}` uses {part}");
            }
            assert!(!is_float_literal(&t), "{f}: float literal in `{t}`");
            let segs: Vec<&str> = t.split('.').collect();
            for m in segs.iter().skip(1) {
                assert!(
                    !banned_methods.contains(m),
                    "{f}: float method `.{m}(` in `{t}`"
                );
            }
        }
        // `as f…` casts would be caught above (f32 / f64 tokens).
    }
    for f in LINT_GUARDED {
        let src = std::fs::read_to_string(src_dir.join(f)).unwrap();
        assert!(
            src.contains("#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]"),
            "{f}: missing the float lint guard"
        );
    }
}

/// The checker itself catches what it should.
#[test]
fn checker_catches_floats() {
    let bad = [
        "let x = 1.4;",
        "let y: f64 = 0;",
        "let z = a as f32;",
        "let d = (x * x).sqrt();",
        "let b = y.atan2(x);",
    ];
    for b in bad {
        let toks = tokens(&code_only(b));
        let hit = toks.iter().any(|t| {
            t.split('.')
                .any(|p| p == "f32" || p == "f64" || p == "sqrt" || p == "atan2")
                || is_float_literal(t)
        });
        assert!(hit, "{b}");
    }
    // comments and strings are ignored; integer ranges and tuple fields are fine
    for ok in [
        "// 1.4 m/s",
        "let s = \"1.4\";",
        "for i in 0..3 {}",
        "let x = c0.0 + p.1;",
    ] {
        let toks = tokens(&code_only(ok));
        assert!(
            toks.iter()
                .all(|t| !is_float_literal(t) && !t.contains("f64")),
            "{ok}"
        );
    }
}
