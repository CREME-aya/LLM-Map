import hashlib
import mmap
import struct
from collections import OrderedDict

from common import require

HEADER = struct.Struct("<4sHHQ32sI")
RECORD = struct.Struct("<HBB")


def inspect(raw, version, tile):
    require(len(raw) >= HEADER.size, "sidecar ヘッダ欠損")
    magic, fmt, size, key, digest, count = HEADER.unpack_from(raw)
    require(
        (magic, fmt, size, key) == (b"DSC1", 1, RECORD.size, tile),
        "sidecar 形式・タイル不一致",
    )
    require(
        digest == hashlib.sha256(version.encode()).digest(), "sidecar グラフ版不一致"
    )
    require(count > 0 and len(raw) == HEADER.size + count * size, "sidecar 件数不一致")
    for width, lanes, tags in RECORD.iter_unpack(memoryview(raw)[HEADER.size :]):
        require(
            tags < 16
            and bool(tags & 2) == bool(width)
            and bool(tags & 4) == bool(lanes),
            "sidecar 属性ビット不整合",
        )
    return count


class TileReader:
    """全国調査で mmap を保持するタイル数を制限する。入力ディレクトリは固定スナップショット。"""

    def __init__(self, root, version, tiles):
        from common import integer, unique

        self.root = root
        self.version = version
        self.tiles = unique(tiles, lambda t: integer(t["tile"]))
        self.cache = OrderedDict()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        for data in self.cache.values():
            data.close()

    def attributes(self, ref):
        import re

        from common import integer

        tile, index = integer(ref["tile"]), integer(ref["index"])
        meta = self.tiles[tile]
        if tile not in self.cache:
            digest = meta["sha256"]
            require(re.fullmatch("[0-9a-f]{64}", digest), "sidecar SHA 不正")
            with (self.root / digest).open("rb") as stream:
                data = mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_READ)
            try:
                require(
                    hashlib.sha256(data).hexdigest() == digest, "sidecar SHA 不一致"
                )
                require(
                    inspect(data, self.version, tile) == integer(meta["edge_count"], 1),
                    "sidecar 有向辺数不一致",
                )
            except (ValueError, KeyError, TypeError):
                data.close()
                raise
            self.cache[tile] = data
            if len(self.cache) > 8:
                self.cache.popitem(last=False)[1].close()
        self.cache.move_to_end(tile)
        require(index < meta["edge_count"], "sidecar 有向辺索引不正")
        width, lanes, mask = RECORD.unpack_from(
            self.cache[tile], HEADER.size + index * RECORD.size
        )
        return {
            "width": str(width) if width else None,
            "lanes": str(lanes) if lanes else None,
            "traffic_signals": "present" if mask & 1 else None,
            "turn:lanes": "present" if mask & 8 else None,
        }
