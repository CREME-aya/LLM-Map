use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    schema_version: u8,
    plan_request_id: String,
    graph_version: String,
    candidates: Vec<Candidate>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    candidate_id: String,
    label: String,
    legal: bool,
    complete: bool,
    eta_ms: u32,
    legs: Vec<Vec<Edge>>,
    maneuvers: Vec<Maneuver>,
}
// この検体が扱う操作を閉じた型にして、未知の操作をゼロ件と誤集計しない。
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Maneuver {
    CrossingRightSignalUnknown {
        leg: usize,
        incoming: usize,
        reason: String,
    },
}

fn junction_facts(c: &Candidate) -> Result<Vec<Value>, String> {
    let mut junctions = Vec::new();
    let mut visited = HashSet::new();
    for maneuver in &c.maneuvers {
        let Maneuver::CrossingRightSignalUnknown {
            leg,
            incoming,
            reason,
        } = maneuver;
        let edges = c.legs.get(*leg).ok_or("leg 範囲外")?;
        let index = incoming.checked_add(1).ok_or("操作位置不正")?;
        let outgoing = edges.get(index).ok_or("操作位置不正")?;
        if !visited.insert((leg, incoming))
            || !["missing", "conflict", "direction_unclear"].contains(&reason.as_str())
        {
            return Err("操作重複・根拠不正".into());
        }
        junctions.push(
            json!({"ref":format!("j{}",junctions.len()+1),"edge_in":edges[*incoming].edge_id,
            "edge_out":outgoing.edge_id,"kind":"crossing_right_signal_unknown",
            "evidence":{"kind":"unknown","reason":reason}}),
        );
    }
    Ok(junctions)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edge {
    edge_id: String,
    direction: String,
    start_ppm: u32,
    end_ppm: u32,
    length_cm: u32,
    two_way: bool,
    residential: bool,
    width_cm: Width,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Width {
    Confirmed { value: u16, source_ref: String },
    Inferred { value: bool, rule_id: String },
    Unknown { reason: String },
}

fn facts(c: &Candidate, search: &Search) -> Result<(Value, u64), String> {
    if !c.legal
        || !c.complete
        || c.eta_ms == 0
        || c.legs.is_empty()
        || ![
            "baseline",
            "engine_alternate",
            "costing_variant",
            "through_anchor",
        ]
        .contains(&c.label.as_str())
    {
        return Err("合法・完全な候補が必要".into());
    }
    let (mut total, mut narrow, mut inferred, mut unknown, mut unknown_res) = (0u64, 0, 0, 0, 0);
    let mut fingerprint = format!("{}|", search.graph_version);
    for leg in &c.legs {
        if leg.is_empty() {
            return Err("空 leg".into());
        }
        fingerprint.push('/');
        for edge in leg {
            let id = edge.edge_id.parse::<u64>().map_err(|_| "有向辺 ID")?;
            if id.to_string() != edge.edge_id
                || !["forward", "reverse"].contains(&edge.direction.as_str())
                || edge.start_ppm >= edge.end_ppm
                || edge.end_ppm > 1_000_000
                || edge.length_cm == 0
            {
                return Err("有向辺・部分区間不正".into());
            }
            fingerprint.push_str(&format!(
                "{}:{}:{}-{},",
                id, edge.direction, edge.start_ppm, edge.end_ppm
            ));
            let cm = (u64::from(edge.length_cm) * u64::from(edge.end_ppm - edge.start_ppm)
                + 500_000)
                / 1_000_000;
            total += cm;
            match &edge.width_cm {
                Width::Confirmed { value, source_ref } if *value > 0 && !source_ref.is_empty() => {
                    if edge.two_way && *value < 400 {
                        narrow += cm;
                    }
                }
                Width::Inferred { value, rule_id } if !rule_id.is_empty() => {
                    if edge.two_way && *value {
                        inferred += cm;
                    }
                }
                Width::Unknown { reason }
                    if ["missing", "conflict", "direction_unclear"].contains(&reason.as_str()) =>
                {
                    unknown += cm;
                    if edge.residential {
                        unknown_res += cm;
                    }
                }
                _ => return Err("根拠欠落・版不一致".into()),
            }
        }
    }
    let m = |cm: u64| (cm + 50) / 100;
    let junctions = junction_facts(c)?;
    let facts = json!({"candidate_id":c.candidate_id,"label":c.label,"plan_request_id":search.plan_request_id,
        "graph_version":search.graph_version,"fingerprint":fingerprint,"legal":true,"complete":true,
        "eta_ms":c.eta_ms,"length_m":m(total),"counts":{"right_unprotected":0,"right_signal_unknown_protection":0,
        "right_signal_unknown":junctions.len(),"right_protected":0,"right_entry":0,"merges":0},
        "narrow_m":{"confirmed":m(narrow),"inferred":m(inferred)},"unknown_width_residential_m":m(unknown_res),
        "unknown_length_m":m(unknown),"junctions":junctions,"through_anchors":[]});
    let score_ms = u64::from(c.eta_ms)
        + 3 * narrow
        + (3 * unknown_res).div_ceil(2)
        + 45_000 * junctions.len() as u64;
    Ok((facts, score_ms))
}

fn score(line: &str) -> Result<Value, String> {
    let search: Search = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if search.schema_version != 1
        || search.plan_request_id.is_empty()
        || search.graph_version.is_empty()
        || !(1..=7).contains(&search.candidates.len())
    {
        return Err("版・要求・候補件数不正".into());
    }
    let mut ids = HashSet::new();
    let mut values = Vec::new();
    for c in &search.candidates {
        if c.candidate_id.is_empty() || !ids.insert(&c.candidate_id) {
            return Err("候補 ID 不正".into());
        }
        let (facts, score_ms) = facts(c, &search)?;
        values.push(json!({"facts":facts,"score_ms":score_ms}));
    }
    Ok(json!(values))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        writeln!(stdout, "{}", score(&line?)?)?;
        stdout.flush()?;
    }
    Ok(())
}
