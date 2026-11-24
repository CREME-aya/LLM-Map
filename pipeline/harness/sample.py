"""架空データで入出力の形式を再現する。パスは新規ディレクトリに限定する。"""

import hashlib
import io
import json
import subprocess
import zipfile
from pathlib import Path

from gate2 import KINDS, OD_COUNTS, PREFS
from gate3 import CATEGORIES, TAGS
from gate5 import schedule
from sidecar import HEADER, RECORD


def write(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")


def ndjson(path, records):
    with path.open("w", encoding="utf-8") as stream:
        for r in records:
            stream.write(json.dumps(r, ensure_ascii=False) + "\n")


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def tile_zip(tile, version):
    raw = HEADER.pack(
        b"DSC1", 1, RECORD.size, tile, hashlib.sha256(version.encode()).digest(), 2
    )
    raw += RECORD.pack(350, 1, 6) + RECORD.pack(0, 0, 0)
    files = {"dsc": raw, "gph": f"synthetic graph {tile} {version}".encode()}
    record = {"tile": tile, "edge_count": 2}
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as archive:
        for ext, data in files.items():
            archive.writestr(f"{tile}.{ext}", data)
            record[ext] = digest(data)
    return buf.getvalue(), record


def manifest(store, version, previous):
    chunks = []
    for i, kind in enumerate(sorted(KINDS)):
        c = {"kind": kind, "graph_version": version, "build_id": "build-" + version}
        if kind in ("L0", "L1", "L2"):
            level = int(kind[1])
            tile_base = (
                (((35 + 90) * 4 * 1440 + (135 + 180) * 4) << 3 | level)
                if level == 2
                else level
            )
            raw, tile = tile_zip(tile_base, version)
            c["tiles"] = [tile]
            c.update(
                {"cell": [35, 135], "part": 0}
                if kind == "L2"
                else {"scope": "national"}
            )
        else:
            raw = kind.encode()
        c.update(sha256=digest(raw), bytes=len(raw))
        (store / c["sha256"]).write_bytes(raw)
        chunks.append(c)
    return {
        "graph_version": version,
        "previous_graph_version": previous,
        "build_scope": "japan-single-build",
        "build_id": "build-" + version,
        "builder_tile_inventory": [
            t["tile"] for c in chunks for t in c.get("tiles", [])
        ],
        "pbf_sha256": digest(version.encode()),
        "valhalla_commit": "a" * 40,
        "model": {
            "storage": "external-pinned",
            "revision": "test",
            "sha256": digest(b"model"),
        },
        "additional_r2_objects": [],
        "regions": {
            f"r{i}": sorted(PREFS)[a:b]
            for i, (a, b) in enumerate(
                (
                    (0, 1),
                    (1, 7),
                    (7, 14),
                    (14, 23),
                    (23, 30),
                    (30, 35),
                    (35, 39),
                    (39, 47),
                )
            )
        },
        "chunks": chunks,
        "prefectures": {p: [c["sha256"] for c in chunks] for p in sorted(PREFS)},
    }


def openssl(*args):
    subprocess.run(["openssl", *map(str, args)], check=True, capture_output=True)


def gate2(root):
    store = root / "blobs"
    store.mkdir(parents=True)
    private, public = root / "test-key.pem", root / "test-public.pem"
    openssl(
        "genpkey",
        "-algorithm",
        "EC",
        "-pkeyopt",
        "ec_paramgen_curve:P-256",
        "-out",
        private,
    )
    openssl("pkey", "-in", private, "-pubout", "-out", public)
    for name, version, parent in (("old", "v1", "v0"), ("new", "v2", "v1")):
        path = root / f"{name}.json"
        write(path, manifest(store, version, parent))
        openssl("dgst", "-sha256", "-sign", private, "-out", str(path) + ".sig", path)
    private.unlink()
    ods = [
        {
            "od_id": f"{kind}-{i}",
            "kind": kind,
            "download_bytes": 250,
            "requests": 3,
            "fragment_fetch_ok": True,
            "airplane_recalculated": True,
        }
        for kind, n in OD_COUNTS.items()
        for i in range(n)
    ]
    write(
        root / "run.json",
        {
            "initial_region": "r0",
            "app_valhalla_commit": "a" * 40,
            "od_runs": ods,
            "od_plan": [{"od_id": r["od_id"], "kind": r["kind"]} for r in ods],
            "device_free_bytes": 100000,
            "reserve_bytes": 1000,
            "reroutes": {
                "inside_ok": [True] * 9 + [False],
                "outside_safe": [True] * 10,
            },
        },
    )


def gate3(root):
    root.mkdir()
    version, fixed = "jp-test-1", digest(b"synthetic-pbf")
    strata, tags, cases, routes, audits = [], [], [], [], []
    (root / "sidecars").mkdir()
    blob = HEADER.pack(
        b"DSC1", 1, RECORD.size, 0, hashlib.sha256(version.encode()).digest(), 2
    )
    blob += RECORD.pack(0, 1, 13) + RECORD.pack(350, 1, 15)
    (root / "sidecars" / digest(blob)).write_bytes(blob)
    for p in sorted(PREFS):
        strata.append(
            {
                "pref": p,
                "road_class": "residential",
                "junction": 1,
                "section": 1,
                "approach": 1,
            }
        )
        for unit in sorted(TAGS):
            source = {t: "present" for t in TAGS[unit]}
            if unit == "section":
                source.update(width=None if int(p) % 2 else "350", lanes="1")
            tags.append(
                {
                    "pref": p,
                    "road_class": "residential",
                    "unit": unit,
                    "unit_id": "1",
                    "length_cm": 1000,
                    "graph_version": version,
                    "pbf_sha256": fixed,
                    "sidecar_status": "verified",
                    "source": source,
                    "edges": [{"tile": 0, "index": 0 if int(p) % 2 else 1}],
                }
            )
    for category, n in CATEGORIES.items():
        for i in range(n):
            od = f"{category}-{i}"
            common = {"od_id": od, "graph_version": version}
            cases.append(
                dict(
                    **common,
                    origin=[35 + len(cases) * 0.001, 135],
                    destination=[35.1, 135.1],
                    departure="2026-01-01T10:00:00+09:00",
                    vehicle="普通車",
                    category=category,
                )
            )
            routes.append(dict(**common, kind="result", outcome="ok"))
            for j in range(1 if i == 0 else 2):
                key = f"r{j}"
                routes.append(
                    dict(
                        **common,
                        kind="candidate",
                        route_id=key,
                        route_ref=f"{od}/{key}",
                        fingerprint=f"edges-{j}",
                        baseline=j == 0,
                        adopted=j == 1 or i == 0,
                        complete=True,
                        entrance_display="unknown",
                    )
                )
                audits.append(
                    dict(
                        **common,
                        route_id=key,
                        blind=True,
                        reviewer="A",
                        disputed=False,
                        sources=["合成資料"],
                        unknowns=["入口方向"],
                        legality="legal",
                        entrance="unknown",
                        crossings=10 - 3 * j,
                        narrow_cm=1000 - 200 * j,
                    )
                )
    write(
        root / "survey.json",
        {
            "graph_version": version,
            "pbf_sha256": fixed,
            "strata": strata,
            "sidecars": [{"tile": 0, "sha256": digest(blob), "edge_count": 2}],
        },
    )
    for name, data in (
        ("tags", tags),
        ("cases", cases),
        ("routes", routes),
        ("audits", audits),
    ):
        ndjson(root / f"{name}.ndjson", data)


def dump(package, main=700000, inference=400000):
    return "".join(
        f"** MEMINFO in pid {pid} [{process}] **\n App Summary\n TOTAL PSS: {pss} TOTAL RSS: 1234\n"
        for pid, process, pss in (
            (11, package, main),
            (12, package + ":inference", inference),
        )
    )


def gate5(root):
    root.mkdir()
    events = [{"second": 112, "event": "inference_reload"}]
    package = "jp.example.nav"
    write(
        root / "run.json",
        {
            "package": package,
            "device": "fixture",
            "charging": "on",
            "events": events,
            "observed_duration_ms": 3600000,
            "abnormal_exits": [],
        },
    )
    samples = []
    for sec in schedule(events):
        (root / f"{sec}.txt").write_text(
            dump(package, inference=430000 if sec == 112 else 400000)
        )
        samples.append(
            {
                "second": sec,
                "start_ms": sec * 1000,
                "end_ms": sec * 1000 + 100,
                "status": "ok",
            }
        )
    ndjson(root / "samples.ndjson", samples)


if __name__ == "__main__":
    import sys

    root = Path(sys.argv[1])
    root.mkdir(parents=True, exist_ok=False)
    gate2(root / "gate2")
    gate3(root / "gate3")
    gate5(root / "gate5")
