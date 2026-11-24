#!/usr/bin/env bash
set -euo pipefail

# Gate 0 build environment & repository structure audit script.
# Outputs a Markdown summary by default, or JSON with --json.

FORMAT="markdown"
STRICT_GATE0=0
SUNEAST_MOUNT="${SUNEAST_MOUNT:-/Volumes/suneast}"

for arg in "$@"; do
    case "$arg" in
        --json) FORMAT="json" ;;
        --markdown) FORMAT="markdown" ;;
        --strict-gate0) STRICT_GATE0=1 ;;
        -h|--help)
            cat <<'EOF'
Usage: bash scripts/check-env.sh [--markdown|--json] [--strict-gate0]
EOF
            exit 0
            ;;
        *)
            echo "Unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

python3 - "$REPO_ROOT" "$SUNEAST_MOUNT" "$FORMAT" "$STRICT_GATE0" <<'PY'
import json
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

repo_root = Path(sys.argv[1])
suneast_mount = Path(sys.argv[2])
out_format = sys.argv[3]
strict_gate0 = sys.argv[4] == "1"


def run_cmd(cmd, cwd=None, timeout=10):
    try:
        proc = subprocess.run(
            cmd,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
        return proc.returncode, (proc.stdout or "").strip(), (proc.stderr or "").strip()
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        return 127, "", str(exc)


def disk_gib(path: Path):
    if not path.exists():
        return None
    usage = shutil.disk_usage(path)
    return {
        "total_gib": round(usage.total / (1024 ** 3), 1),
        "free_gib": round(usage.free / (1024 ** 3), 1),
    }


def tool_version(name, args):
    exe = shutil.which(name)
    if not exe:
        return {"installed": False, "path": None, "version": None}
    rc, out, err = run_cmd([exe, *args])
    line = (out or err).splitlines()[0] if (out or err) else ""
    return {"installed": rc == 0, "path": exe, "version": line}


# 1. Host & Disk
ram_gib = None
if platform.system() == "Darwin":
    rc, out, _ = run_cmd(["sysctl", "-n", "hw.memsize"])
    if rc == 0 and out.isdigit():
        ram_gib = round(int(out) / (1024 ** 3), 1)

root_disk = disk_gib(Path("/"))
suneast_mounted = suneast_mount.is_dir() and os.path.ismount(suneast_mount)
suneast_disk = disk_gib(suneast_mount) if suneast_mounted else None
suneast_case_sensitive = None
if suneast_mounted and platform.system() == "Darwin":
    rc, out, _ = run_cmd(["diskutil", "info", str(suneast_mount)])
    if rc == 0:
        suneast_case_sensitive = "Case-sensitive" in out

# 2. External SSD env vars
tracked_env_vars = [
    "DEV_EXT",
    "ANDROID_HOME",
    "ANDROID_USER_HOME",
    "ANDROID_AVD_HOME",
    "GRADLE_USER_HOME",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "RUSTUP_HOME",
    "SCCACHE_DIR",
    "npm_config_cache",
]
env_status = {k: os.environ.get(k) for k in tracked_env_vars}

# 3. Toolchains
tools = {
    "rustc": tool_version("rustc", ["--version"]),
    "cargo": tool_version("cargo", ["--version"]),
    "rustup": tool_version("rustup", ["--version"]),
    "node": tool_version("node", ["--version"]),
    "npm": tool_version("npm", ["--version"]),
    "python3": tool_version("python3", ["--version"]),
    "java": tool_version("java", ["-version"]),
    "clang": tool_version("clang", ["--version"]),
    "lld": tool_version("ld.lld", ["--version"]) if shutil.which("ld.lld") else tool_version("lld", ["-flavor", "gnu", "--version"]),
    "adb": tool_version("adb", ["version"]),
    "cmake": tool_version("cmake", ["--version"]),
    "ninja": tool_version("ninja", ["--version"]),
    "gradle": tool_version("gradle", ["--version"]),
    "docker": tool_version("docker", ["--version"]),
    "gh": tool_version("gh", ["--version"]),
    "jq": tool_version("jq", ["--version"]),
    "bundletool": tool_version("bundletool", ["version"]),
}

rust_targets = []
if tools["rustup"]["installed"]:
    rc, out, _ = run_cmd(["rustup", "target", "list", "--installed"])
    if rc == 0:
        rust_targets = [line.strip() for line in out.splitlines() if line.strip()]

cargo_ndk_rc, cargo_ndk_out, _ = run_cmd(["cargo", "ndk", "--version"])
tools["cargo-ndk"] = {
    "installed": cargo_ndk_rc == 0,
    "path": shutil.which("cargo-ndk"),
    "version": cargo_ndk_out.splitlines()[0] if cargo_ndk_rc == 0 and cargo_ndk_out else None,
}

# 4. Android SDK probe
sdk_candidates = [
    os.environ.get("ANDROID_HOME"),
    os.environ.get("ANDROID_SDK_ROOT"),
    str(suneast_mount / "dev" / "android-sdk"),
    str(Path.home() / "Library" / "Android" / "sdk"),
]
android_sdk_path = None
for cand in sdk_candidates:
    if cand and Path(cand).is_dir():
        android_sdk_path = Path(cand)
        break

android_sdk = {"path": str(android_sdk_path) if android_sdk_path else None}
if android_sdk_path:
    def list_subdirs(sub):
        d = android_sdk_path / sub
        return sorted(p.name for p in d.iterdir() if p.is_dir()) if d.is_dir() else []

    android_sdk["platforms"] = list_subdirs("platforms")
    android_sdk["build_tools"] = list_subdirs("build-tools")
    android_sdk["ndk"] = list_subdirs("ndk")
    android_sdk["system_images"] = list_subdirs("system-images")
    zipaligns = sorted(str(p) for p in android_sdk_path.glob("build-tools/*/zipalign"))
    android_sdk["zipalign"] = zipaligns[-1] if zipaligns else None
    emulator_bin = android_sdk_path / "emulator" / "emulator"
    android_sdk["emulator"] = str(emulator_bin) if emulator_bin.is_file() else None
    if emulator_bin.is_file():
        rc, out, _ = run_cmd([str(emulator_bin), "-list-avds"])
        android_sdk["avds"] = [l.strip() for l in out.splitlines() if l.strip()] if rc == 0 else []
    else:
        android_sdk["avds"] = []

# 5. Repository layout & git
expected_dirs = ["docs", "schema", "core-rust", "android", "web", "pipeline", "scripts"]
repo_dirs = {d: (repo_root / d).is_dir() for d in expected_dirs}
_, branch, _ = run_cmd(["git", "branch", "--show-current"], cwd=repo_root)
_, remotes, _ = run_cmd(["git", "remote", "-v"], cwd=repo_root)

# 6. Gate 0 readiness gaps
gaps = []
if root_disk and root_disk["free_gib"] < 25.0:
    gaps.append(f"Internal SSD free space is low ({root_disk['free_gib']} GiB); keep caches on {suneast_mount}.")
if not suneast_mounted:
    gaps.append(f"External SSD {suneast_mount} is not mounted.")
elif not (suneast_mount / "dev").is_dir():
    gaps.append(f"{suneast_mount / 'dev'} is not initialized (run `bash scripts/env-suneast.sh --init-dirs`).")
if not env_status.get("CARGO_TARGET_DIR"):
    gaps.append("External SSD env vars (CARGO_TARGET_DIR, GRADLE_USER_HOME, etc.) are unset in current shell.")
if "aarch64-linux-android" not in rust_targets:
    gaps.append("Rust target `aarch64-linux-android` not installed (`rustup target add aarch64-linux-android`).")
if not tools["cargo-ndk"]["installed"]:
    gaps.append("`cargo-ndk` not installed (needed for Android Rust cdylib build).")
if not android_sdk.get("ndk"):
    gaps.append("Android NDK not found under Android SDK (needed for Valhalla C++ / llama.cpp / cargo-ndk).")
for req in ("cmake", "ninja"):
    if not tools[req]["installed"]:
        gaps.append(f"`{req}` not installed (needed for Valhalla C++ / llama.cpp native builds).")

report = {
    "host": {
        "os": platform.system(),
        "arch": platform.machine(),
        "ram_gib": ram_gib,
        "root_disk": root_disk,
        "suneast": {
            "path": str(suneast_mount),
            "mounted": suneast_mounted,
            "case_sensitive": suneast_case_sensitive,
            "disk": suneast_disk,
            "dev_dir_exists": (suneast_mount / "dev").is_dir() if suneast_mounted else False,
        },
    },
    "env": env_status,
    "tools": tools,
    "rust_targets": rust_targets,
    "android_sdk": android_sdk,
    "repo": {
        "root": str(repo_root),
        "branch": branch,
        "has_remote": bool(remotes),
        "directories": repo_dirs,
    },
    "gate0_gaps": gaps,
}

if out_format == "json":
    print(json.dumps(report, indent=2, ensure_ascii=False))
else:
    print("# Gate 0 Build Environment & Repository Audit")
    print()
    print(f"- **Host**: `{report['host']['os']} {report['host']['arch']}` / RAM `{ram_gib}` GiB")
    if root_disk:
        print(f"- **Internal SSD (`/`)**: `{root_disk['free_gib']} GiB` free / `{root_disk['total_gib']} GiB` total")
    se = report["host"]["suneast"]
    if se["mounted"]:
        print(
            f"- **External SSD (`{se['path']}`)**: mounted, "
            f"case-sensitive=`{se['case_sensitive']}`, "
            f"free=`{se['disk']['free_gib']} GiB`, `dev/` exists=`{se['dev_dir_exists']}`"
        )
    else:
        print(f"- **External SSD (`{se['path']}`)**: NOT MOUNTED")
    print(f"- **Git**: branch=`{branch}`, remote configured=`{bool(remotes)}`")
    print()
    print("## Monorepo Directories")
    for d, exists in repo_dirs.items():
        mark = "present" if exists else "absent"
        print(f"- `{d}/`: {mark}")
    print()
    print("## Toolchains")
    for name, info in tools.items():
        status = f"OK ({info['version']})" if info["installed"] else "MISSING"
        print(f"- `{name}`: {status}")
    print(f"- `rustup targets`: {', '.join(rust_targets) if rust_targets else '(none)'}")
    print()
    print("## Android SDK")
    print(f"- Path: `{android_sdk.get('path')}`")
    if android_sdk_path:
        print(f"- Platforms: `{android_sdk['platforms']}`")
        print(f"- Build-Tools: `{android_sdk['build_tools']}` (zipalign: `{android_sdk['zipalign']}`)")
        print(f"- NDK: `{android_sdk['ndk']}`")
        print(f"- System Images: `{android_sdk['system_images']}` / AVDs: `{android_sdk['avds']}`")
    print()
    print("## Gate 0 Action Items")
    if gaps:
        for g in gaps:
            print(f"- [ ] {g}")
    else:
        print("- All Gate 0 environment checks passed.")

if strict_gate0 and gaps:
    sys.exit(1)
PY
