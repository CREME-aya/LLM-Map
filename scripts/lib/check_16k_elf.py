#!/usr/bin/env python3
"""Verify ELF64 PT_LOAD alignment >= 16 KB (16384 bytes) for Android 16 KB page support.

Works natively on macOS and Linux using only the Python standard library.
Supports inspecting individual .so files, directories, and Android archives (.apk/.aab/.aar).
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
import zipfile
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable

MIN_PAGE_ALIGNMENT = 16384  # 16 KB
ELF_MAGIC = b"\x7fELF"
ELFCLASS64 = 2
ELFDATA2LSB = 1
ELFDATA2MSB = 2
PT_LOAD = 1
EM_AARCH64 = 183
EM_X86_64 = 62


@dataclass
class SegmentCheck:
    index: int
    p_align: int
    ok: bool


@dataclass
class BinaryResult:
    source: str
    status: str  # PASS | FAIL | ERROR
    machine: str | None
    load_segments: list[SegmentCheck]
    zip_offset_aligned: bool | None = None
    reason: str | None = None


def _is_power_of_two(value: int) -> bool:
    return value > 0 and (value & (value - 1)) == 0


def _machine_name(e_machine: int) -> str:
    if e_machine == EM_AARCH64:
        return "aarch64"
    if e_machine == EM_X86_64:
        return "x86_64"
    return f"unknown({e_machine})"


def inspect_elf64_bytes(data: bytes, source: str, min_align: int = MIN_PAGE_ALIGNMENT) -> BinaryResult:
    if len(data) < 64:
        return BinaryResult(source=source, status="ERROR", machine=None, load_segments=[], reason="File too small to be ELF64")
    if data[:4] != ELF_MAGIC:
        return BinaryResult(source=source, status="ERROR", machine=None, load_segments=[], reason="Not an ELF binary")
    if data[4] != ELFCLASS64:
        return BinaryResult(
            source=source,
            status="ERROR",
            machine=None,
            load_segments=[],
            reason=f"Unsupported ELF class {data[4]} (expected 64-bit ELFCLASS64)",
        )

    ei_data = data[5]
    if ei_data == ELFDATA2LSB:
        endian = "<"
    elif ei_data == ELFDATA2MSB:
        endian = ">"
    else:
        return BinaryResult(source=source, status="ERROR", machine=None, load_segments=[], reason=f"Invalid ELF endianness {ei_data}")

    # ELF64 header fields we need:
    # e_machine (offset 18, H), e_phoff (offset 32, Q), e_phentsize (offset 54, H), e_phnum (offset 56, H)
    e_machine = struct.unpack_from(f"{endian}H", data, 18)[0]
    e_phoff = struct.unpack_from(f"{endian}Q", data, 32)[0]
    e_phentsize = struct.unpack_from(f"{endian}H", data, 54)[0]
    e_phnum = struct.unpack_from(f"{endian}H", data, 56)[0]

    if e_phnum == 0 or e_phentsize < 56:
        return BinaryResult(
            source=source,
            status="ERROR",
            machine=_machine_name(e_machine),
            load_segments=[],
            reason=f"Invalid program header table (phnum={e_phnum}, phentsize={e_phentsize})",
        )

    table_end = e_phoff + e_phentsize * e_phnum
    if table_end > len(data):
        return BinaryResult(
            source=source,
            status="ERROR",
            machine=_machine_name(e_machine),
            load_segments=[],
            reason="Program header table extends past EOF",
        )

    load_segments: list[SegmentCheck] = []
    for idx in range(e_phnum):
        offset = e_phoff + idx * e_phentsize
        # Elf64_Phdr: p_type (I), p_flags (I), p_offset (Q), p_vaddr (Q), p_paddr (Q),
        #             p_filesz (Q), p_memsz (Q), p_align (Q)
        p_type = struct.unpack_from(f"{endian}I", data, offset)[0]
        if p_type != PT_LOAD:
            continue
        p_align = struct.unpack_from(f"{endian}Q", data, offset + 48)[0]
        ok = _is_power_of_two(p_align) and p_align >= min_align
        load_segments.append(SegmentCheck(index=idx, p_align=p_align, ok=ok))

    if not load_segments:
        return BinaryResult(
            source=source,
            status="ERROR",
            machine=_machine_name(e_machine),
            load_segments=[],
            reason="No PT_LOAD segments found",
        )

    failed = [seg for seg in load_segments if not seg.ok]
    if failed:
        aligns = ", ".join(str(seg.p_align) for seg in failed)
        return BinaryResult(
            source=source,
            status="FAIL",
            machine=_machine_name(e_machine),
            load_segments=load_segments,
            reason=f"PT_LOAD p_align < {min_align}: [{aligns}]",
        )

    return BinaryResult(
        source=source,
        status="PASS",
        machine=_machine_name(e_machine),
        load_segments=load_segments,
    )


def _zip_entry_data_offset(zf: zipfile.ZipFile, info: zipfile.ZipInfo) -> int | None:
    """Compute the byte offset where an archive member's payload starts."""
    if not zf.fp:
        return None
    zf.fp.seek(info.header_offset)
    local_header = zf.fp.read(30)
    if len(local_header) < 30 or local_header[:4] != b"PK\x03\x04":
        return None
    name_len, extra_len = struct.unpack_from("<HH", local_header, 26)
    return info.header_offset + 30 + name_len + extra_len


def inspect_archive(archive_path: Path, abi_filter: str | None, check_zip_offset: bool, min_align: int) -> list[BinaryResult]:
    results: list[BinaryResult] = []
    with zipfile.ZipFile(archive_path, "r") as zf:
        so_members = [
            info
            for info in zf.infolist()
            if info.filename.endswith(".so")
            and (abi_filter is None or f"/{abi_filter}/" in f"/{info.filename}")
        ]
        if not so_members:
            results.append(
                BinaryResult(
                    source=str(archive_path),
                    status="ERROR",
                    machine=None,
                    load_segments=[],
                    reason=f"No matching .so entries found in archive (abi_filter={abi_filter})",
                )
            )
            return results

        for info in sorted(so_members, key=lambda x: x.filename):
            data = zf.read(info.filename)
            res = inspect_elf64_bytes(data, source=f"{archive_path}!/{info.filename}", min_align=min_align)
            if check_zip_offset and info.compress_type == zipfile.ZIP_STORED:
                payload_offset = _zip_entry_data_offset(zf, info)
                aligned = payload_offset is not None and (payload_offset % min_align == 0)
                res.zip_offset_aligned = aligned
                if not aligned and res.status == "PASS":
                    res.status = "FAIL"
                    res.reason = f"Uncompressed .so zip data offset ({payload_offset}) is not {min_align}-byte aligned"
            results.append(res)
    return results


def collect_targets(paths: Iterable[Path], abi_filter: str | None, check_zip_offset: bool, min_align: int) -> list[BinaryResult]:
    results: list[BinaryResult] = []
    for path in paths:
        if not path.exists():
            results.append(
                BinaryResult(source=str(path), status="ERROR", machine=None, load_segments=[], reason="Path does not exist")
            )
            continue
        if path.is_dir():
            so_files = sorted(
                p
                for p in path.rglob("*.so")
                if p.is_file() and (abi_filter is None or abi_filter in p.parts)
            )
            if not so_files:
                results.append(
                    BinaryResult(
                        source=str(path),
                        status="ERROR",
                        machine=None,
                        load_segments=[],
                        reason=f"No matching .so files found in directory (abi_filter={abi_filter})",
                    )
                )
                continue
            for so_file in so_files:
                results.append(inspect_elf64_bytes(so_file.read_bytes(), source=str(so_file), min_align=min_align))
        elif path.suffix.lower() in {".apk", ".aab", ".aar", ".zip"}:
            results.extend(inspect_archive(path, abi_filter=abi_filter, check_zip_offset=check_zip_offset, min_align=min_align))
        else:
            results.append(inspect_elf64_bytes(path.read_bytes(), source=str(path), min_align=min_align))
    return results


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Verify ELF64 16 KB LOAD alignment for Android arm64 binaries.")
    parser.add_argument("targets", nargs="+", type=Path, help=".so file(s), directories, or .apk/.aab/.aar archives")
    parser.add_argument("--abi", default=None, help="Filter .so paths by ABI name (e.g. arm64-v8a)")
    parser.add_argument("--min-align", type=int, default=MIN_PAGE_ALIGNMENT, help="Minimum PT_LOAD alignment in bytes (default: 16384)")
    parser.add_argument("--check-zip-offset", action="store_true", help="Also require uncompressed .so zip offsets to be 16 KB aligned")
    parser.add_argument("--json", action="store_true", dest="as_json", help="Emit JSON output")
    args = parser.parse_args(argv)

    results = collect_targets(args.targets, abi_filter=args.abi, check_zip_offset=args.check_zip_offset, min_align=args.min_align)
    all_ok = all(r.status == "PASS" for r in results) and len(results) > 0

    if args.as_json:
        payload = {
            "min_align": args.min_align,
            "passed": all_ok,
            "results": [asdict(r) for r in results],
        }
        print(json.dumps(payload, indent=2, ensure_ascii=False))
    else:
        for res in results:
            aligns = ",".join(str(s.p_align) for s in res.load_segments) or "-"
            extra = f" ({res.reason})" if res.reason else ""
            print(f"[{res.status}] {res.source} machine={res.machine or '-'} pt_load_align=[{aligns}]{extra}")

    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main())
