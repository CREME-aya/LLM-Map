import json
from pathlib import Path
from zipfile import BadZipFile


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def integer(value, minimum=0):
    require(type(value) is int and value >= minimum, f"整数の範囲外: {value!r}")
    return value


def label(value):
    require(isinstance(value, str) and value.strip(), "識別子が空です")
    return value


def unique(items, key):
    indexed = {}
    for item in items:
        identifier = key(item)
        require(identifier not in indexed, f"重複: {identifier}")
        indexed[identifier] = item
    return indexed


def pairs(items):
    return {k: v for k, v in unique(items, lambda item: item[0]).values()}


def decode(raw):
    return json.loads(raw, object_pairs_hook=pairs, parse_constant=lambda x: integer(x))


def read_json(path):
    return decode(Path(path).read_text(encoding="utf-8"))


def rows(path):
    with Path(path).open(encoding="utf-8") as stream:
        for number, line in enumerate(stream, 1):
            try:
                yield decode(line)
            except ValueError as exc:
                raise ValueError(f"{path}:{number}: {exc}") from exc


def cli(function):
    import sys

    try:
        report = function(*sys.argv[1:])
        print(json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False))
        return 0 if report["criteria_met"] else 1
    except (ValueError, OSError, KeyError, TypeError, IndexError, BadZipFile) as exc:
        print(
            json.dumps(
                {"criteria_met": False, "input_error": str(exc)}, ensure_ascii=False
            )
        )
        return 2
