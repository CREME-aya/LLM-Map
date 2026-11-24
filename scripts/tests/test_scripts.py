"""Unit tests for repository foundation and integration verification scripts."""

from __future__ import annotations

import json
import os
import struct
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
LIB_DIR = REPO_ROOT / "scripts" / "lib"
sys.path.insert(0, str(LIB_DIR))

import check_16k_elf  # noqa: E402
import repo_checks  # noqa: E402


def build_minimal_elf64(pt_load_aligns: list[int], e_machine: int = check_16k_elf.EM_AARCH64) -> bytes:
    """Synthesize a minimal valid ELF64 binary with the given PT_LOAD p_align values."""
    e_ehsize = 64
    e_phentsize = 56
    e_phnum = len(pt_load_aligns)
    e_phoff = e_ehsize

    e_ident = (
        b"\x7fELF"
        + bytes([check_16k_elf.ELFCLASS64, check_16k_elf.ELFDATA2LSB, 1, 0])
        + b"\x00" * 8
    )
    header = e_ident + struct.pack(
        "<HHIQQQIHHHHHH",
        3,            # e_type: ET_DYN
        e_machine,    # e_machine
        1,            # e_version
        0,            # e_entry
        e_phoff,      # e_phoff
        0,            # e_shoff
        0,            # e_flags
        e_ehsize,     # e_ehsize
        e_phentsize,  # e_phentsize
        e_phnum,      # e_phnum
        0,            # e_shentsize
        0,            # e_shnum
        0,            # e_shstrndx
    )
    assert len(header) == 64

    phdrs = bytearray()
    for p_align in pt_load_aligns:
        phdrs.extend(
            struct.pack(
                "<IIQQQQQQ",
                check_16k_elf.PT_LOAD,  # p_type
                5,                      # p_flags (PF_R | PF_X)
                0,                      # p_offset
                0,                      # p_vaddr
                0,                      # p_paddr
                64,                     # p_filesz
                64,                     # p_memsz
                p_align,                # p_align
            )
        )
    return bytes(header + phdrs)


class TestCheck16kElf(unittest.TestCase):
    def test_accepts_16k_and_64k_aligned_segments(self) -> None:
        elf = build_minimal_elf64([16384, 65536])
        res = check_16k_elf.inspect_elf64_bytes(elf, source="libok.so")
        self.assertEqual(res.status, "PASS")
        self.assertEqual(res.machine, "aarch64")
        self.assertEqual([s.p_align for s in res.load_segments], [16384, 65536])

    def test_rejects_4k_aligned_segment(self) -> None:
        elf = build_minimal_elf64([16384, 4096])
        res = check_16k_elf.inspect_elf64_bytes(elf, source="libbad.so")
        self.assertEqual(res.status, "FAIL")
        self.assertIn("4096", res.reason or "")

    def test_rejects_non_power_of_two_alignment(self) -> None:
        elf = build_minimal_elf64([24576])
        res = check_16k_elf.inspect_elf64_bytes(elf, source="libweird.so")
        self.assertEqual(res.status, "FAIL")

    def test_rejects_non_elf_bytes(self) -> None:
        res = check_16k_elf.inspect_elf64_bytes(b"not an elf binary" * 8, source="junk.so")
        self.assertEqual(res.status, "ERROR")
        self.assertIn("Not an ELF", res.reason or "")

    def test_inspects_apk_archive_and_filters_abi(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            apk = Path(tmp) / "sample.apk"
            ok_elf = build_minimal_elf64([16384])
            bad_elf = build_minimal_elf64([4096])
            with zipfile.ZipFile(apk, "w", compression=zipfile.ZIP_DEFLATED) as zf:
                zf.writestr("lib/arm64-v8a/libcore.so", ok_elf)
                zf.writestr("lib/armeabi-v7a/liblegacy.so", bad_elf)

            rc = check_16k_elf.main(["--abi", "arm64-v8a", str(apk)])
            self.assertEqual(rc, 0)

            rc_all = check_16k_elf.main([str(apk)])
            self.assertEqual(rc_all, 1)

    def test_check_zip_offset_detects_unaligned_stored_so(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            apk = Path(tmp) / "unaligned.apk"
            ok_elf = build_minimal_elf64([16384])
            with zipfile.ZipFile(apk, "w", compression=zipfile.ZIP_STORED) as zf:
                zf.writestr("lib/arm64-v8a/libcore.so", ok_elf)

            results = check_16k_elf.inspect_archive(
                apk,
                abi_filter="arm64-v8a",
                check_zip_offset=True,
                min_align=16384,
            )
            self.assertEqual(len(results), 1)
            self.assertEqual(results[0].status, "FAIL")
            self.assertFalse(results[0].zip_offset_aligned)

    def test_case_collision_helper_passes_on_clean_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "alpha.txt").write_text("a\n", encoding="utf-8")
            (root / "beta.txt").write_text("b\n", encoding="utf-8")
            self.assertEqual(repo_checks.check_case_collisions(root), [])


class TestRepoChecks(unittest.TestCase):
    def test_json_validation_detects_invalid_json(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "valid.json").write_text(json.dumps({"ok": True}), encoding="utf-8")
            (root / "broken.json").write_text("{not-json", encoding="utf-8")
            checked, errors = repo_checks.check_json_files(root)
            self.assertEqual(checked, 2)
            self.assertEqual(len(errors), 1)
            self.assertIn("broken.json", errors[0])

    def test_markdown_link_checker_catches_broken_and_ignores_code_fence(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "exists.md").write_text("# Target\n", encoding="utf-8")
            (root / "doc.md").write_text(
                "\n".join(
                    [
                        "[ok](exists.md)",
                        "[ext](https://example.com)",
                        "```markdown",
                        "[ignored](missing-inside-fence.md)",
                        "```",
                        "[broken](does-not-exist.md#sec)",
                    ]
                ),
                encoding="utf-8",
            )
            checked, errors = repo_checks.check_markdown_links(root)
            self.assertEqual(checked, 2)
            self.assertEqual(len(errors), 1)
            self.assertIn("does-not-exist.md", errors[0])

    def test_secret_scanner_flags_private_key(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "clean.txt").write_text("hello world\n", encoding="utf-8")
            (root / "leak.txt").write_text("-----BEGIN " + "PRIVATE KEY-----\nabc\n", encoding="utf-8")
            _, errors = repo_checks.check_secrets(root)
            self.assertEqual(len(errors), 1)
            self.assertIn("leak.txt", errors[0])


class TestEnvSuneastScript(unittest.TestCase):
    def test_fails_fast_when_mount_missing(self) -> None:
        env = os.environ.copy()
        env["SUNEAST_MOUNT"] = "/nonexistent/mount/point/suneast"
        proc = subprocess.run(
            ["bash", str(REPO_ROOT / "scripts" / "env-suneast.sh"), "--print"],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("not found", proc.stderr)

    def test_prints_exports_when_mount_available(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            env = os.environ.copy()
            env["SUNEAST_MOUNT"] = tmp
            env["SUNEAST_SKIP_MOUNT_CHECK"] = "1"
            proc = subprocess.run(
                ["bash", str(REPO_ROOT / "scripts" / "env-suneast.sh"), "--print"],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertIn(f'export DEV_EXT="{tmp}/dev"', proc.stdout)
            self.assertIn(f'export CARGO_TARGET_DIR="{tmp}/dev/cargo-target"', proc.stdout)
            self.assertIn(f'export GRADLE_USER_HOME="{tmp}/dev/gradle"', proc.stdout)


if __name__ == "__main__":
    unittest.main()
