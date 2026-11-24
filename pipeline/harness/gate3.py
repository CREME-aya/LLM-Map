import math
from collections import Counter, defaultdict
from datetime import datetime
from pathlib import Path

from common import cli, integer, label, read_json, require, rows, unique

CATEGORIES = {"urban_right": 12, "suburban_narrow": 12, "entrance": 8, "remote": 8}
OUTCOMES = {"ok", "no_route", "incomplete_data", "timeout", "error"}
TAGS = {
    "junction": ("traffic_signals",),
    "section": ("width", "lanes"),
    "approach": ("turn:lanes",),
}


def tag_report(root, meta, reader):
    expected = unique(
        meta["strata"], lambda r: (label(r["pref"]), label(r["road_class"]))
    )
    require(
        {p for p, _ in expected} == {f"{i:02}" for i in range(1, 48)}, "全国の層が必要"
    )
    counts, totals = Counter(), defaultdict(Counter)
    previous = None
    for obs in rows(root / "tags.ndjson"):
        group = (obs["pref"], obs["road_class"])
        kind = obs["unit"]
        key = (*group, kind, label(obs["unit_id"]))
        require(previous is None or previous < key, "タグ母集団の順序不正または重複")
        previous = key
        require(
            group in expected
            and kind in TAGS
            and obs["graph_version"] == meta["graph_version"]
            and obs["pbf_sha256"] == meta["pbf_sha256"],
            "層・入力版不一致",
        )
        require(obs["sidecar_status"] == "verified", "sidecar 欠落・破損")
        counts[(*group, kind)] += 1
        distance = integer(obs["length_cm"], 1) if kind == "section" else 0
        require(set(obs["source"]) == set(TAGS[kind]), "タグ照合列欠損")
        refs = obs["edges"]
        require(isinstance(refs, list) and refs, "対応有向辺なし")
        destinations = [reader.attributes(ref) for ref in refs]
        for tag in TAGS[kind]:
            source = obs["source"][tag]
            if source is not None:
                label(source)
            arrived = all(dest[tag] == source for dest in destinations)
            wrong = any(
                dest[tag] is not None and dest[tag] != source for dest in destinations
            )
            counter = totals[(*group, tag)]
            counter["units"] += 1
            counter["detected"] += source is not None
            counter["delivered"] += source is not None and arrived
            counter["mismatch"] += wrong
            counter["eligible_cm"] += distance
            counter["detected_cm"] += distance * (source is not None)
            counter["delivered_cm"] += distance * (source is not None and arrived)
    for group, stratum in expected.items():
        for kind in TAGS:
            require(
                counts[(*group, kind)] == integer(stratum[kind]),
                "固定母集団の行数不一致",
            )
    require(
        all(sum(v for (*_, k), v in counts.items() if k == kind) > 0 for kind in TAGS),
        "全国タグ母集団が空",
    )
    ratio = lambda a, b: a / b if b else None
    result = []
    for (pref, road, tag), c in sorted(totals.items()):
        result.append(
            dict(
                pref=pref,
                road_class=road,
                tag=tag,
                **c,
                detection_rate=ratio(c["detected"], c["units"]),
                transfer_rate=ratio(c["delivered"], c["detected"]),
                distance_detection_rate=ratio(c["detected_cm"], c["eligible_cm"]),
                distance_transfer_rate=ratio(c["delivered_cm"], c["detected_cm"]),
            )
        )
    return result


def check_case(case, version):
    require(case["graph_version"] == version, "OD 版不一致")
    label(case["vehicle"])
    require(
        datetime.fromisoformat(case["departure"]).utcoffset() is not None,
        "出発時刻の時差欠落",
    )
    for point in (case["origin"], case["destination"]):
        require(
            len(point) == 2
            and all(type(x) in (int, float) and math.isfinite(x) for x in point)
            and abs(point[0]) <= 90
            and abs(point[1]) <= 180,
            "座標不正",
        )


def check_audit(audit):
    label(audit["reviewer"])
    require(
        audit["blind"] is True and type(audit["disputed"]) is bool, "盲検監査未確認"
    )
    if audit["disputed"]:
        require(
            label(audit["second_reviewer"]) != audit["reviewer"]
            and audit["resolved"] is True,
            "独立した再監査が必要",
        )
    require(
        isinstance(audit["sources"], list)
        and audit["sources"]
        and all(label(s) for s in audit["sources"]),
        "監査出典欠落",
    )
    require(
        isinstance(audit["unknowns"], list)
        and all(label(s) for s in audit["unknowns"]),
        "未確認列不正",
    )
    require(
        audit["legality"] in {"legal", "illegal", "unknown"}
        and audit["entrance"] in {"left", "right", "unknown"},
        "監査値不正",
    )
    for field in ("crossings", "narrow_cm"):
        if audit[field] is not None:
            integer(audit[field])


def audit_report(root, meta):
    version = label(meta["graph_version"])
    cases = unique(rows(root / "cases.ndjson"), lambda r: label(r["od_id"]))
    require(
        Counter(c["category"] for c in cases.values()) == CATEGORIES, "40 OD の構成不正"
    )
    for case in cases.values():
        check_case(case, version)
    results, routes = {}, defaultdict(dict)
    for r in rows(root / "routes.ndjson"):
        od = r["od_id"]
        require(od in cases and r["graph_version"] == version, "未知 OD・版不一致")
        if r["kind"] == "result":
            require(
                od not in results and r["outcome"] in OUTCOMES, "実行結果不正・重複"
            )
            results[od] = r["outcome"]
        else:
            route = label(r["route_id"])
            require(r["kind"] == "candidate" and route not in routes[od], "候補行不正")
            label(r["route_ref"])
            label(r["fingerprint"])
            require(
                all(type(r[k]) is bool for k in ("baseline", "adopted", "complete"))
                and r["entrance_display"] in {"left", "right", "unknown"},
                "候補属性不正",
            )
            routes[od][route] = r
    audits = unique(rows(root / "audits.ndjson"), lambda r: (r["od_id"], r["route_id"]))
    for (od, route), a in audits.items():
        require(
            route in routes.get(od, {}) and a["graph_version"] == version,
            "監査対応不正",
        )
        check_audit(a)
    pending, eligible, no_alt, violations, assertions = {}, [], [], 0, 0
    base_total, chosen_total = Counter(), Counter()
    for od in cases:
        if od not in results:
            pending[od] = "missing_result"
            continue
        if results.get(od) != "ok":
            require(not routes.get(od), "結果と候補が矛盾")
            pending[od] = results.get(od, "missing_result")
            continue
        candidates = routes[od]
        if not candidates:
            pending[od] = "missing_candidate"
            continue
        baseline, chosen = (
            [k for k, r in candidates.items() if r[f]] for f in ("baseline", "adopted")
        )
        require(len(baseline) == len(chosen) == 1, "標準・採用を一意に指定")
        confirmed = set()
        for key, route in candidates.items():
            a = audits.get((od, key))
            if not a:
                pending[od] = "missing_audit"
                continue
            violations += a["legality"] == "illegal"
            assertions += (
                route["entrance_display"] == "left" and a["entrance"] != "left"
            )
            if (
                a["legality"] == "unknown"
                or a["crossings"] is None
                or a["narrow_cm"] is None
            ):
                pending[od] = "unverified_audit"
            elif a["legality"] == "legal" and route["complete"]:
                confirmed.add(key)
        if baseline[0] not in confirmed or chosen[0] not in confirmed:
            pending.setdefault(od, "unverified_baseline_or_adopted")
        if od in pending:
            continue
        if len({candidates[k]["fingerprint"] for k in confirmed}) < 2:
            no_alt.append(od)
            continue
        eligible.append(od)
        for counter, key in ((base_total, baseline[0]), (chosen_total, chosen[0])):
            for metric in ("crossings", "narrow_cm"):
                counter[metric] += audits[od, key][metric]
    reductions = {
        k: (base_total[k] - chosen_total[k]) / base_total[k] if base_total[k] else None
        for k in ("crossings", "narrow_cm")
    }
    meets = (
        base_total["crossings"] > 0
        and chosen_total["crossings"] * 100 <= base_total["crossings"] * 70
        and base_total["narrow_cm"] > 0
        and chosen_total["narrow_cm"] * 100 <= base_total["narrow_cm"] * 80
    )
    return {
        "criteria_met": bool(
            meets and no_alt and not pending and not violations and not assertions
        ),
        "pending": pending,
        "audited_od_count": len(cases) - len(pending),
        "paired_od_ids": eligible,
        "no_alternative": no_alt,
        "observed_illegal": violations,
        "observed_false_left": assertions,
        "baseline": dict(base_total),
        "adopted": dict(chosen_total),
        "reductions": reductions,
    }


def evaluate(directory):
    from gate2 import sha

    root = Path(directory)
    meta = read_json(root / "survey.json")
    sha(meta["pbf_sha256"])
    report = audit_report(root, meta)
    from sidecar import TileReader

    with TileReader(
        root / "sidecars", meta["graph_version"], meta["sidecars"]
    ) as reader:
        report["tag_strata"] = tag_report(root, meta, reader)
    if any(r["mismatch"] for r in report["tag_strata"]):
        report["criteria_met"] = False
    report["scope"] = "固定版入力の監査・到達率集計"
    return report


if __name__ == "__main__":
    raise SystemExit(cli(evaluate))
