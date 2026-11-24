#!/usr/bin/env bash
set -euo pipefail

# Wrapper around scripts/lib/check_16k_elf.py with optional zipalign -c -P 16 -v 4 check for APKs.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ "$#" -lt 1 ]; then
    echo "Usage: bash scripts/check-16k-elf.sh [--abi arm64-v8a] [--check-zip-offset] [--json] <target...>" >&2
    exit 2
fi

python3 "${SCRIPT_DIR}/lib/check_16k_elf.py" "$@"
status=$?

# When inspecting an APK directly and zipalign is installed, also verify 16 KB zipalign.
if [ "$status" -eq 0 ]; then
    zipalign_bin="$(command -v zipalign || true)"
    if [ -z "$zipalign_bin" ] && [ -n "${ANDROID_HOME:-}" ]; then
        zipalign_bin="$(find "${ANDROID_HOME}/build-tools" -maxdepth 2 -name zipalign -type f 2>/dev/null | sort | tail -n 1 || true)"
    fi
    if [ -z "$zipalign_bin" ] && [ -d "${HOME}/Library/Android/sdk/build-tools" ]; then
        zipalign_bin="$(find "${HOME}/Library/Android/sdk/build-tools" -maxdepth 2 -name zipalign -type f 2>/dev/null | sort | tail -n 1 || true)"
    fi

    if [ -n "$zipalign_bin" ]; then
        for arg in "$@"; do
            case "$arg" in
                *.apk)
                    if [ -f "$arg" ]; then
                        "$zipalign_bin" -c -P 16 -v 4 "$arg" >/dev/null
                    fi
                    ;;
            esac
        done
    fi
fi

exit "$status"
