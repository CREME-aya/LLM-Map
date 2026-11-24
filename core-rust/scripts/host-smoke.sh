#!/usr/bin/env bash
# Host (macOS/Linux) smoke tests for the native boundary:
#   1. C: compile tests/c/abi_smoke.c against include/dc_core.h, link libdc_core.
#   2. JNI: compile a Java stand-in of DcNative and call the Rust JNI shim.
# The repository path contains spaces: every path below is quoted on purpose.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NB="$ROOT/crates/native-bindings"
OUT="$ROOT/target/host-smoke"
mkdir -p "$OUT"

cargo build --manifest-path "$ROOT/Cargo.toml" -p native-bindings
case "$(uname -s)" in
  Darwin) LIB="$ROOT/target/debug/libdc_core.dylib" ;;
  *)      LIB="$ROOT/target/debug/libdc_core.so" ;;
esac

echo "== C ABI smoke"
cc -std=c11 -Wall -Wextra -Werror -D_GNU_SOURCE -I "$NB/include" "$NB/tests/c/abi_smoke.c" \
   -L "$ROOT/target/debug" -ldc_core -o "$OUT/abi_smoke"
DYLD_LIBRARY_PATH="$ROOT/target/debug" LD_LIBRARY_PATH="$ROOT/target/debug" "$OUT/abi_smoke"

echo "== JNI smoke"
if command -v javac >/dev/null 2>&1; then
  javac -d "$OUT/classes" "$NB/tests/jni/dev/slmmap/core/DcNative.java" "$NB/tests/jni/JniSmoke.java"
  java -Xcheck:jni -cp "$OUT/classes" JniSmoke "$LIB"
else
  echo "SKIP: javac not found"
fi
