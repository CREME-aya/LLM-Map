import math
import re
from pathlib import Path

from common import cli, integer, label, read_json, require, rows, unique

DURATION = 3600
HEADER = re.compile(
    r"^\s*\*\* MEMINFO in pid (\d+) \[([^\]]+)\] \*\*\s*$", re.MULTILINE
)
TOTAL = re.compile(r"^\s*TOTAL PSS:\s*(\d+)\b", re.MULTILINE)


def parse_dump(raw, package):
    wanted = {package, package + ":inference"}
    sections = list(HEADER.finditer(raw))
    result = {}
    for i, match in enumerate(sections):
        pid, name = match.groups()
        if name in wanted:
            block = raw[
                match.end() : sections[i + 1].start()
                if i + 1 < len(sections)
                else len(raw)
            ]
            values = TOTAL.findall(block)
            require(
                len(values) == 1 and name not in result, f"TOTAL PSS 欠損・重複: {name}"
            )
            result[name] = (integer(int(pid), 1), integer(int(values[0]), 1))
    require(set(result) == wanted, "同じ dumpsys 出力に両プロセスが必要")
    return result


def schedule(events):
    require(isinstance(events, list) and events, "イベント記録欠落")
    slots = set(range(0, DURATION + 1, 5))
    for event in events:
        label(event["event"])
        sec = integer(event["second"], 1)
        require(sec < DURATION, "イベントは走行時間内に記録する")
        slots.update((sec - 1, sec, sec + 1))
    return sorted(slots)


def evaluate(directory):
    root = Path(directory)
    info = read_json(root / "run.json")
    package = label(info["package"])
    expected = schedule(info["events"])
    samples = unique(rows(root / "samples.ndjson"), lambda r: integer(r["second"]))
    require(samples.keys() <= set(expected), "採取予定外の行")
    gaps, steady, all_samples, restarts = {}, [], [], []
    last_pid = None
    for sec in expected:
        sample = samples.get(sec)
        if sample is None:
            gaps[sec] = "missing"
            continue
        if sample["status"] != "ok":
            gaps[sec] = label(sample["status"])
            continue
        start, end = (integer(sample[k]) for k in ("start_ms", "end_ms"))
        if not (sec * 1000 - 500 <= start <= end <= sec * 1000 + 1000):
            gaps[sec] = "sampling_window_exceeded"
            continue
        try:
            pair = parse_dump(
                (root / f"{sec}.txt").read_text(encoding="utf-8"), package
            )
        except (OSError, ValueError) as exc:
            gaps[sec] = str(exc)
            continue
        pids = tuple(pair[n][0] for n in (package, package + ":inference"))
        if last_pid is not None and pids != last_pid:
            restarts.append(sec)
        last_pid = pids
        # meminfo の kB は KiB。設計の GB は十進。
        size = sum(v for _, v in pair.values()) * 1024
        all_samples.append(size)
        if sec % 5 == 0:
            steady.append(size)
    p95 = sorted(steady)[math.ceil(0.95 * len(steady)) - 1] if steady else None
    peak = max(all_samples, default=None)
    abnormal = info["abnormal_exits"]
    require(
        isinstance(abnormal, list) and all(label(x) for x in abnormal), "終了記録不正"
    )
    duration = integer(info["observed_duration_ms"])
    require(info["charging"] in ("on", "off"), "充電条件欠落")
    return {
        "device": label(info["device"]),
        "charging": info["charging"],
        "criteria_met": bool(
            duration >= DURATION * 1000
            and not gaps
            and not abnormal
            and not restarts
            and p95 is not None
            and p95 <= 1_300_000_000
            and peak <= 1_500_000_000
        ),
        "p95_bytes": p95,
        "peak_bytes": peak,
        "steady_samples": len(steady),
        "all_samples": len(all_samples),
        "missing": gaps,
        "pid_changes": restarts,
        "abnormal_exits": abnormal,
        "duration_ms": duration,
        "scope": "1走行の PSS 条件",
    }


def matrix(*directories):
    reports = [evaluate(d) for d in directories]
    conditions = unique(reports, lambda r: (r["device"], r["charging"]))
    devices = {d for d, _ in conditions}
    complete = len(devices) == 2 and all(
        (d, c) in conditions for d in devices for c in ("on", "off")
    )
    return {
        "criteria_met": complete and all(r["criteria_met"] for r in reports),
        "scope": "実機2台×充電有無の PSS 条件",
        "runs": reports,
    }


if __name__ == "__main__":
    import sys

    raise SystemExit(cli(evaluate if len(sys.argv) == 2 else matrix))
