"""一回の C++ 生成結果を Kotlin→Rust に供給して部分辺集計・負例・版付き契約を検証。"""

import argparse
import json
import subprocess
import tempfile
from hashlib import sha256
from pathlib import Path

from common import read_json, require
from sidecar import inspect

ROOT = Path(__file__).resolve().parent / "handoff"
TARGET = ROOT / "target"


def schema_check(schema_dir, results):
    from jsonschema import Draft202012Validator
    from referencing import Registry, Resource

    common = read_json(schema_dir / "common.schema.json")
    schema = read_json(schema_dir / "candidate-facts.schema.json")
    registry = Registry().with_resource(common["$id"], Resource.from_contents(common))
    validator = Draft202012Validator(schema, registry=registry)
    for result in results:
        validator.validate(result["facts"])


def verify(schema_dir=None):
    fixture = ROOT / "fixture.dsc"
    raw = fixture.read_bytes()
    require(
        sha256(raw).hexdigest()
        == "d22bda0c9198f91ca149e655d5c53a1975c40f881c7ad8b0c54cb610a49b27c6",
        "共有検体の SHA 不一致",
    )
    require(inspect(raw, "jp-test-1", 0) == 2, "共有検体の辺数不正")
    subprocess.run(["cargo", "build", "--release", "--locked"], cwd=ROOT, check=True)
    subprocess.run(
        [
            "mvn",
            "-q",
            "package",
            "dependency:build-classpath",
            "-Dmdep.outputFile=target/classpath",
        ],
        cwd=ROOT,
        check=True,
    )
    with tempfile.TemporaryDirectory() as d:
        work = Path(d)
        cpp = work / "replay"
        subprocess.run(
            [
                "c++",
                "-std=c++17",
                "-Wall",
                "-Wextra",
                "-Werror",
                str(ROOT / "replay.cpp"),
                "-o",
                str(cpp),
            ],
            check=True,
        )
        payload = subprocess.check_output([str(cpp), str(fixture)])
        path = work / "search.ndjson"
        path.write_bytes(payload)
        cp = str(TARGET / "classes") + ":" + (TARGET / "classpath").read_text().strip()
        scorer = str(TARGET / "release/score")
        base = ["java", "-cp", cp, "PassKt", str(path)]
        call = lambda req, ver, proc, n: subprocess.run(
            base + [req, ver, proc, str(n)], capture_output=True, check=False
        )
        run = call("計画-1", "jp-test-1", scorer, 50)
        require(run.returncode == 0, run.stderr.decode())
        scores = json.loads(run.stdout)
        require(
            len(scores) == 7
            and scores[0]["facts"]["narrow_m"]["confirmed"] == 150
            and scores[0]["facts"]["unknown_width_residential_m"] == 150
            and scores[0]["score_ms"] == 352500
            and scores[0]["facts"]["junctions"][0]["edge_out"] == "33554432"
            and scores[6]["facts"]["length_m"] == 240,
            "部分辺集計または秒換算誤り",
        )
        if schema_dir:
            schema_check(schema_dir, scores)
        print(run.stderr.decode().strip())
        # 別プロセスで Kotlin を出た正確なバイト列を照合（UTF-8、空白、キー順を含む）。
        echo = work / "echo"
        echo.write_text(
            "#!/usr/bin/env python3\nimport sys,hashlib\nfor line in sys.stdin.buffer: print(hashlib.sha256(line[:-1]).hexdigest(),flush=True)\n"
        )
        echo.chmod(0o700)
        forwarded = call("計画-1", "jp-test-1", str(echo), 1)
        require(
            forwarded.returncode == 0
            and forwarded.stdout.decode().strip() == sha256(payload).hexdigest(),
            "Kotlin がペイロードを変更した",
        )
        for request, version in (("old", "jp-test-1"), ("計画-1", "old")):
            require(
                call(request, version, scorer, 1).returncode != 0,
                "要求・版不一致を受理",
            )
        for mutation in (
            "missing",
            "invalid_fraction",
            "invalid_evidence",
            "overflow_id",
            "incomplete",
        ):
            search = json.loads(payload)
            candidate = search["candidates"][0]
            edge = candidate["legs"][0][0]
            if mutation == "missing":
                del edge["direction"]
            elif mutation == "invalid_fraction":
                edge["end_ppm"] = 0
            elif mutation == "invalid_evidence":
                edge["width_cm"] = {
                    "kind": "unknown",
                    "value": 200,
                    "reason": "missing",
                }
            elif mutation == "overflow_id":
                edge["edge_id"] = str(2**64)
            else:
                candidate["complete"] = False
            path.write_bytes(json.dumps(search, ensure_ascii=False).encode())
            require(call("計画-1", "jp-test-1", scorer, 1).returncode != 0, mutation)
    print("ホスト受け渡し・7候補採点・生バイト照合・拒否7ケース 成功")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--schema-dir", type=Path)
    verify(parser.parse_args().schema_dir)
