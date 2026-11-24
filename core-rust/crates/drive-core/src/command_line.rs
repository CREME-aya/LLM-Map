//! Parser for the driving-time 1-line command code emitted by the SLM.
//!
//! Must accept exactly the same language as Hydrogenくん's leading-key-branch GBNF
//! (devteam #23/#25): keys in fixed order rc → nr → mg → dt → jp, each at most once,
//! `dt` is `T|S` only, empty `OVR` is not allowed.
//!
//! TODO(gbnf): the canonical grammar is `schema/slm/override.gbnf` (owned by Hydrogenくん,
//! not placed yet). `tests/fixtures/command_line.provisional.gbnf` is a provisional copy;
//! point the conformance test at the canonical file once it exists.

use contracts::override_types::{Detour, Level};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpRef {
    Last,
    Next,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OvrArgs {
    pub rc: Option<Level>,
    pub nr: Option<Level>,
    pub mg: Option<Level>,
    pub dt: Option<Detour>,
    pub jp: Option<JpRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ovr(OvrArgs),
    Cxl,
    Why,
    Bmk,
    Yes,
    No,
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError(pub &'static str);

/// `root ::= cmd "\n"`. A single trailing `\n` is accepted and optional
/// (TODO(gbnf): confirm whether Kotlin strips it before dc_validate_override).
pub fn parse(line: &str) -> Result<Command, SyntaxError> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    match line {
        "CXL" => return Ok(Command::Cxl),
        "WHY" => return Ok(Command::Why),
        "BMK" => return Ok(Command::Bmk),
        "YES" => return Ok(Command::Yes),
        "NO" => return Ok(Command::No),
        "NOOP" => return Ok(Command::Noop),
        _ => {}
    }
    let rest = line
        .strip_prefix("OVR")
        .ok_or(SyntaxError("unknown command"))?;
    if rest.is_empty() {
        return Err(SyntaxError("empty OVR"));
    }
    let mut args = OvrArgs::default();
    // Position in the fixed order; each key must come strictly after the previous one.
    let mut next_min = 0usize;
    for tok in rest.split(' ').skip(1) {
        let (key, val) = tok
            .split_once('=')
            .ok_or(SyntaxError("expected key=value"))?;
        let pos = match key {
            "rc" => 0,
            "nr" => 1,
            "mg" => 2,
            "dt" => 3,
            "jp" => 4,
            _ => return Err(SyntaxError("unknown key")),
        };
        if pos < next_min {
            return Err(SyntaxError("key out of order or duplicated"));
        }
        next_min = pos + 1;
        match pos {
            0 => args.rc = Some(level(val)?),
            1 => args.nr = Some(level(val)?),
            2 => args.mg = Some(level(val)?),
            3 => {
                args.dt = Some(match val {
                    "T" => Detour::Tight,
                    "S" => Detour::Standard,
                    _ => return Err(SyntaxError("dt must be T|S")),
                })
            }
            _ => {
                args.jp = Some(match val {
                    "last" => JpRef::Last,
                    "next" => JpRef::Next,
                    _ => return Err(SyntaxError("jp must be last|next")),
                })
            }
        }
    }
    // `rest` must be exactly " k=v" repeated: reject leading/trailing/double spaces.
    if !rest.starts_with(' ') || rest.ends_with(' ') || rest.contains("  ") || next_min == 0 {
        return Err(SyntaxError("bad separator"));
    }
    Ok(Command::Ovr(args))
}

fn level(v: &str) -> Result<Level, SyntaxError> {
    match v {
        "L" => Ok(Level::Low),
        "N" => Ok(Level::Normal),
        "H" => Ok(Level::High),
        _ => Err(SyntaxError("level must be L|N|H")),
    }
}
