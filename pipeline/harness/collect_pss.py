"""事前登録イベントのある GPX 試験用。診断用 smaps は読まない。"""

import argparse
import json
import re
import subprocess
import time
from pathlib import Path

from common import read_json, require
from gate5 import schedule


def collect(serial, plan, output):
    info = read_json(plan)
    package = info["package"]
    require(
        re.fullmatch(r"[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)+", package), "パッケージ名不正"
    )
    slots = schedule(info["events"])
    output.mkdir(parents=True, exist_ok=False)
    start = time.monotonic_ns()
    elapsed = lambda: (time.monotonic_ns() - start) // 1_000_000
    with (output / "samples.ndjson").open("x") as log:
        for second in slots:
            time.sleep(max(0, second - elapsed() / 1000))
            row = {"second": second, "start_ms": elapsed()}
            try:
                cmd = [
                    "adb",
                    "-s",
                    serial,
                    "shell",
                    "dumpsys",
                    "meminfo",
                    "-a",
                    "--package",
                    package,
                ]
                data = subprocess.run(cmd, capture_output=True, timeout=5, check=True)
                row.update(status="ok", end_ms=elapsed())
                (output / f"{second}.txt").write_bytes(data.stdout)
            except (OSError, subprocess.SubprocessError) as exc:
                row.update(status=str(exc), end_ms=elapsed())
            log.write(json.dumps(row) + "\n")
            log.flush()
    info["observed_duration_ms"] = elapsed()
    info["device"] = serial
    # 実走行後の異常終了記録は利用者が logcat 等で照合・確定する。
    info["abnormal_exits"] = None
    (output / "run.json").write_text(json.dumps(info, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("serial")
    parser.add_argument("plan", type=Path)
    parser.add_argument("new_directory", type=Path)
    args = parser.parse_args()
    collect(args.serial, args.plan, args.new_directory)
