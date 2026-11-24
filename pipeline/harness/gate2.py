"""検証済み署名のマニフェストを対象にしたローカル Gate 2 リハーサル。"""

import hashlib
import os
import re
import subprocess
import tempfile
import zipfile
from collections import Counter
from pathlib import Path

from common import cli, decode, integer, label, read_json, require, unique
from sidecar import inspect

PREFS = {f"{i:02}" for i in range(1, 48)}
KINDS = {"overview", "detail", "style", "L0", "L1", "L2", "address", "history"}
OD_COUNTS = {"border": 4, "regional": 3, "ferry": 2, "mountain": 3}
GB, MB = 10**9, 10**6


def sha(value):
    require(re.fullmatch("[0-9a-f]{64}", value), "SHA-256 が不正")
    return value


def check_manifest(manifest, store):
    version = label(manifest["graph_version"])
    sha(manifest["pbf_sha256"])
    require(
        re.fullmatch("[0-9a-f]{40}", manifest["valhalla_commit"]), "生成コミット不明"
    )
    require(manifest["build_scope"] == "japan-single-build", "全国一括生成でない")
    build_id = label(manifest["build_id"])
    chunks = unique(manifest["chunks"], lambda c: sha(c["sha256"]))
    require({c["kind"] for c in chunks.values()} == KINDS, "全国成果物欠落・未知種別")
    tiles, parts = set(), set()
    for digest, chunk in chunks.items():
        require(
            chunk["graph_version"] == version and chunk["build_id"] == build_id,
            "チャンクの版不一致",
        )
        blob = store / digest
        require(blob.stat().st_size == integer(chunk["bytes"], 1), "容量不一致")
        with blob.open("rb") as stream:
            require(
                hashlib.file_digest(stream, "sha256").hexdigest() == digest,
                "チャンク破損",
            )
        kind = chunk["kind"]
        if kind in {"L0", "L1"}:
            require(chunk["scope"] == "national", "幹線は全国共通")
        if kind == "L2":
            cell = chunk["cell"]
            require(
                len(cell) == 2
                and all(type(x) is int for x in cell)
                and -90 <= cell[0] < 90
                and -180 <= cell[1] < 180,
                "固定1度セル不正",
            )
            part = (*cell, integer(chunk["part"]))
            require(part not in parts, "セル内分割番号重複")
            parts.add(part)
        if kind in {"L0", "L1", "L2"}:
            verify_tiles(blob, chunk, version, tiles)
    source_tiles = manifest["builder_tile_inventory"]
    require(
        len(source_tiles) == len(tiles) and set(source_tiles) == tiles,
        "全国ビルドのタイル欠損",
    )
    index = manifest["prefectures"]
    require(set(index) == PREFS, "47 都道府県索引欠落")
    shared = {
        key
        for key, c in chunks.items()
        if c["kind"] in {"L0", "L1", "overview", "style"}
    }
    indexed = set()
    for refs in index.values():
        require(
            isinstance(refs, list)
            and set(refs) <= chunks.keys()
            and shared <= set(refs)
            and {"L2", "detail", "address"} <= {chunks[key]["kind"] for key in refs},
            "県の共有タイル参照不正",
        )
        indexed.update(refs)
    require(
        all(key in indexed for key, c in chunks.items() if c["kind"] == "L2"),
        "孤立した L2",
    )
    regions = manifest["regions"]
    require(
        len(regions) == 8
        and sorted(p for group in regions.values() for p in group) == sorted(PREFS)
        and all(regions.values()),
        "8 地方索引不正",
    )
    return chunks


def verify_tiles(blob, chunk, version, seen):
    with zipfile.ZipFile(blob) as archive:
        members = set()
        require(bool(chunk["tiles"]), "空タイルチャンク")
        for tile in chunk["tiles"]:
            key = integer(tile["tile"])
            level = int(chunk["kind"][1])
            require(
                key < (1 << 25) and (key & 7) == level, "GraphId 階層・タイルキー不正"
            )
            if level == 2:
                row, column = divmod(key >> 3, 1440)
                require(
                    [row // 4 - 90, column // 4 - 180] == chunk["cell"],
                    "L2 タイルの1度セル不一致",
                )
            require(key not in seen, "重複タイル")
            seen.add(key)
            for suffix in ("gph", "dsc"):
                name = f"{key}.{suffix}"
                members.add(name)
                with archive.open(name) as stream:
                    require(
                        hashlib.file_digest(stream, "sha256").hexdigest()
                        == sha(tile[suffix]),
                        "タイル・sidecar ハッシュ不一致",
                    )
                if suffix == "dsc":
                    require(
                        inspect(archive.read(name), version, key)
                        == integer(tile["edge_count"], 1),
                        "有向辺数不一致",
                    )
        require(
            sorted(archive.namelist()) == sorted(members),
            "未申告または重複の ZIP エントリ",
        )


def budget(old, new, region):
    require(region and set(region) <= new.keys(), "初回地域索引欠損")
    require(
        all(
            old[k] == new[k]
            or (
                old[k].get("sha256") == new[k].get("sha256")
                and old[k]["bytes"] == new[k]["bytes"]
            )
            for k in old.keys() & new.keys()
        ),
        "旧版の実体を上書き",
    )
    union = {**old, **new}
    total = lambda records: sum(row["bytes"] for row in records)
    normal = total(new.values())
    peak = total(union.values())
    first = total(new[key] for key in set(region))
    largest = max(row["bytes"] for row in union.values())
    return {
        "normal_bytes": normal,
        "peak_bytes": peak,
        "initial_bytes": first,
        "largest_bytes": largest,
        "reused_bytes": total(new[k] for k in new.keys() & old.keys()),
        "within_budget": normal <= 7.5 * GB
        and peak <= 9 * GB
        and first <= 750 * MB
        and largest <= 256 * MB,
    }


def route_checks(records, plan):
    unique(records, lambda r: label(r["od_id"]))
    require(Counter(r["kind"] for r in records) == OD_COUNTS, "12 OD の構成不一致")
    expected = unique(plan, lambda r: label(r["od_id"]))
    require(
        len(expected) == len(records)
        and all(
            r["od_id"] in expected and expected[r["od_id"]]["kind"] == r["kind"]
            for r in records
        ),
        "事前固定した 12 OD に一致しない",
    )
    for row in records:
        integer(row["download_bytes"], 1)
        integer(row["requests"], 1)
    return all(
        r["fragment_fetch_ok"] is True and r["airplane_recalculated"] is True
        for r in records
    )


def signed_manifest(root, name, trusted_key):
    raw = (root / name).read_bytes()
    result = subprocess.run(
        [
            "openssl",
            "dgst",
            "-sha256",
            "-verify",
            str(trusted_key),
            "-signature",
            str(root / (name + ".sig")),
        ],
        input=raw,
        capture_output=True,
        check=False,
    )
    require(result.returncode == 0, f"署名不一致: {name}")
    return decode(raw.decode("utf-8"))


def expanded(chunks, refs, store):
    installed = {}
    for key in set(refs):
        chunk = chunks[key]
        if "tiles" not in chunk:
            installed[key] = chunk["bytes"]
            continue
        with zipfile.ZipFile(store / key) as archive:
            for tile in chunk["tiles"]:
                for ext in ("dsc", "gph"):
                    installed[tile[ext]] = archive.getinfo(
                        f"{tile['tile']}.{ext}"
                    ).file_size
    return installed


def objects(root, name, manifest, chunks):
    inventory = dict(chunks)
    extras = manifest["additional_r2_objects"]
    model = manifest["model"]
    require(model["storage"] in {"r2", "external-pinned"}, "モデル配布元不明")
    label(model["revision"])
    sha(model["sha256"])
    model_parts = []
    for obj in extras:
        path = root / "blobs" / sha(obj["sha256"])
        with path.open("rb") as stream:
            require(
                hashlib.file_digest(stream, "sha256").hexdigest() == obj["sha256"]
                and path.stat().st_size == integer(obj["bytes"], 1),
                "追加 R2 実体不一致",
            )
        if obj["kind"] == "model":
            model_parts.append(path)
        key = label(obj["object_key"])
        require(
            key not in inventory or inventory[key]["sha256"] == obj["sha256"],
            "同じ R2 キーで内容が競合",
        )
        inventory[key] = obj
    require(
        bool(model_parts) == (model["storage"] == "r2"),
        "R2 モデル実体と配布設定が不一致",
    )
    if model_parts:
        joined = hashlib.sha256()
        for path in model_parts:
            with path.open("rb") as stream:
                while data := stream.read(1024 * 1024):
                    joined.update(data)
        require(joined.hexdigest() == model["sha256"], "モデル全体の SHA 不一致")
    for suffix in ("", ".sig"):
        data = (root / (name + suffix)).read_bytes()
        key = f"manifests/{manifest['graph_version']}/manifest.json{suffix}"
        require(key not in inventory, "予約済みマニフェストキー")
        inventory[key] = {
            "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }
    return inventory


def region_refs(manifest, region):
    return {k for p in manifest["regions"][region] for k in manifest["prefectures"][p]}


def rehearse(root, trusted_key, pointer):
    root = Path(root)
    run = read_json(root / "run.json")
    old, new = (
        signed_manifest(root, name, trusted_key) for name in ("old.json", "new.json")
    )
    pinned = pointer.read_text()
    require(pinned == old["graph_version"], "現行世代不一致")
    require(
        label(old["graph_version"]) != new["graph_version"]
        and new["previous_graph_version"] == old["graph_version"],
        "連続2版が必要",
    )
    require(
        new["valhalla_commit"] == run["app_valhalla_commit"], "アプリのコミット不一致"
    )
    store = root / "blobs"
    before, after = (check_manifest(m, store) for m in (old, new))
    region = run["initial_region"]
    old_objects = objects(root, "old.json", old, before)
    new_objects = objects(root, "new.json", new, after)
    report = budget(old_objects, new_objects, region_refs(new, region))
    installed, incoming = (
        expanded(chunks, region_refs(m, region), store)
        for chunks, m in ((before, old), (after, new))
    )
    added = sum(incoming[h] for h in incoming.keys() - installed.keys())
    largest = max(after[k]["bytes"] for k in region_refs(new, region))
    required = added + largest + integer(run["reserve_bytes"], 1)
    report["required_free_bytes"] = required
    report["device_has_space"] = integer(run["device_free_bytes"]) >= required
    ods = run["od_runs"]
    passed = route_checks(ods, run["od_plan"])
    reroutes = run["reroutes"]
    for field in ("inside_ok", "outside_safe"):
        require(
            len(reroutes[field]) == 10
            and all(type(v) is bool for v in reroutes[field]),
            "逸脱試行不正",
        )
    passed &= sum(reroutes["inside_ok"]) >= 9 and all(reroutes["outside_safe"])
    # 切替は一時領域だけで再現し、読み取り済みの世代をセッションに保持する。
    if report["within_budget"] and report["device_has_space"] and passed:
        staged = pointer.with_suffix(".next")
        staged.write_text(new["graph_version"])
        os.replace(staged, pointer)
    active = pointer.read_text()
    return dict(
        **report,
        od_runs=ods,
        pinned_version=pinned,
        active_version=active,
        criteria_met=(active == new["graph_version"]),
        scope="ホスト配布リハーサル",
    )


def evaluate(root, trusted_key):
    root = Path(root)
    with tempfile.TemporaryDirectory() as directory:
        pointer = Path(directory) / "current"
        pointer.write_text(
            signed_manifest(root, "old.json", trusted_key)["graph_version"]
        )
        return rehearse(root, trusted_key, pointer)


if __name__ == "__main__":
    raise SystemExit(cli(evaluate))
