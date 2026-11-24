#!/usr/bin/env bash
# Configure external SSD (/Volumes/suneast) cache/build paths so internal SSD is not exhausted.
# Usage:
#   source scripts/env-suneast.sh
#   bash scripts/env-suneast.sh --print
#   bash scripts/env-suneast.sh --init-dirs

_suneast_is_sourced() {
    if [ -n "${ZSH_VERSION:-}" ]; then
        case "${ZSH_EVAL_CONTEXT:-}" in
            *:file) return 0 ;;
            *) return 1 ;;
        esac
    else
        [ "${BASH_SOURCE[0]:-}" != "$0" ]
    fi
}

_suneast_fail() {
    echo "ERROR: $1" >&2
    if _suneast_is_sourced; then
        return 1
    fi
    exit 1
}

_suneast_main() {
    local mount_root="${SUNEAST_MOUNT:-/Volumes/suneast}"
    local init_dirs=0
    local print_only=0
    local arg

    for arg in "$@"; do
        case "$arg" in
            --init-dirs) init_dirs=1 ;;
            --print) print_only=1 ;;
            -h|--help)
                cat <<'EOF'
Usage:
  source scripts/env-suneast.sh          Export DEV_EXT and tool cache variables
  bash scripts/env-suneast.sh --print    Print export statements without modifying current shell
  bash scripts/env-suneast.sh --init-dirs Create $DEV_EXT subdirectories on /Volumes/suneast
EOF
                return 0
                ;;
            *)
                _suneast_fail "Unknown option: $arg"
                return 1
                ;;
        esac
    done

    # Fail fast when the external SSD is not mounted to prevent cache spill onto internal SSD.
    if [ ! -d "$mount_root" ]; then
        _suneast_fail "External SSD path not found: $mount_root (mount /Volumes/suneast before building)"
        return 1
    fi

    if [ "${SUNEAST_SKIP_MOUNT_CHECK:-0}" != "1" ] && command -v mount >/dev/null 2>&1; then
        if ! mount | grep -Fq " on ${mount_root} "; then
            _suneast_fail "$mount_root exists as a directory but is not a mounted volume. Refusing to write to internal SSD."
            return 1
        fi
    fi

    local dev_ext="${mount_root}/dev"

    if [ "$init_dirs" = "1" ]; then
        mkdir -p \
            "$dev_ext/android-sdk" \
            "$dev_ext/android-user" \
            "$dev_ext/android-avd" \
            "$dev_ext/gradle" \
            "$dev_ext/cargo" \
            "$dev_ext/cargo-target" \
            "$dev_ext/rustup" \
            "$dev_ext/sccache" \
            "$dev_ext/npm-cache"
    fi

    if [ "$print_only" = "1" ]; then
        cat <<EOF
export DEV_EXT="${dev_ext}"
export ANDROID_HOME="${dev_ext}/android-sdk"
export ANDROID_USER_HOME="${dev_ext}/android-user"
export ANDROID_AVD_HOME="${dev_ext}/android-avd"
export GRADLE_USER_HOME="${dev_ext}/gradle"
export CARGO_HOME="${dev_ext}/cargo"
export CARGO_TARGET_DIR="${dev_ext}/cargo-target"
export RUSTUP_HOME="${dev_ext}/rustup"
export SCCACHE_DIR="${dev_ext}/sccache"
export npm_config_cache="${dev_ext}/npm-cache"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
export CMAKE_BUILD_PARALLEL_LEVEL="${CMAKE_BUILD_PARALLEL_LEVEL:-3}"
EOF
        return 0
    fi

    export DEV_EXT="${dev_ext}"
    export ANDROID_HOME="${dev_ext}/android-sdk"
    export ANDROID_USER_HOME="${dev_ext}/android-user"
    export ANDROID_AVD_HOME="${dev_ext}/android-avd"
    export GRADLE_USER_HOME="${dev_ext}/gradle"
    export CARGO_HOME="${dev_ext}/cargo"
    export CARGO_TARGET_DIR="${dev_ext}/cargo-target"
    export RUSTUP_HOME="${dev_ext}/rustup"
    export SCCACHE_DIR="${dev_ext}/sccache"
    export npm_config_cache="${dev_ext}/npm-cache"
    export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
    export CMAKE_BUILD_PARALLEL_LEVEL="${CMAKE_BUILD_PARALLEL_LEVEL:-3}"

    if [ ! -d "$ANDROID_HOME" ] && [ -d "${HOME}/Library/Android/sdk" ]; then
        echo "NOTE: $ANDROID_HOME does not exist yet; existing Android SDK found at ${HOME}/Library/Android/sdk." >&2
        echo "      Migrate or symlink it to $ANDROID_HOME, or override ANDROID_HOME explicitly if desired." >&2
    fi
}

_suneast_main "$@"
