#!/usr/bin/env bash
set -euo pipefail

# Unified monorepo integration verification runner.
# Safely checks all present components and marks not-yet-merged components as SKIP
# unless --require-components is specified.

REQUIRE_COMPONENTS=0
REQUIRE_WEB=0
for arg in "$@"; do
    case "$arg" in
        --require-components) REQUIRE_COMPONENTS=1 ;;
        --require-web) REQUIRE_WEB=1 ;;
        -h|--help)
            cat <<'EOF'
Usage: bash scripts/verify-integration.sh [--require-components] [--require-web]

Options:
  --require-components  Fail if core components (docs/, schema/, core-rust/, pipeline/) are missing.
  --require-web         Fail if optional web/ companion (web/package.json) is missing.
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

cd "$REPO_ROOT"

echo "=== 1. Repository Hygiene Checks ==="
python3 scripts/lib/repo_checks.py check-case --root "$REPO_ROOT"
python3 scripts/lib/repo_checks.py check-json --root "$REPO_ROOT"
python3 scripts/lib/repo_checks.py check-md-links --root "$REPO_ROOT"
python3 scripts/lib/repo_checks.py check-secrets --root "$REPO_ROOT"

echo "=== 2. Shell Script Syntax Checks ==="
for sh_file in scripts/*.sh; do
    [ -f "$sh_file" ] || continue
    bash -n "$sh_file"
    echo "[PASS] bash -n $sh_file"
done

echo "=== 3. Integration Script Unit Tests ==="
python3 -m unittest discover -s scripts/tests -v

echo "=== 4. Component Checks ==="

# 4a. docs/
if [ -f "docs/README.md" ]; then
    echo "[PASS] docs/ detected"
else
    if [ "$REQUIRE_COMPONENTS" = "1" ]; then
        echo "[FAIL] docs/README.md missing" >&2
        exit 1
    fi
    echo "[SKIP] docs/ not present yet"
fi

# 4b. schema/
if [ -d "schema" ]; then
    echo "[RUN] schema/ detected"
    python3 scripts/lib/repo_checks.py check-json --root "$REPO_ROOT/schema"
else
    if [ "$REQUIRE_COMPONENTS" = "1" ]; then
        echo "[FAIL] schema/ directory missing" >&2
        exit 1
    fi
    echo "[SKIP] schema/ not present yet"
fi

# 4c. core-rust/
if [ -f "core-rust/Cargo.toml" ]; then
    echo "[RUN] core-rust/ detected"
    (
        cd core-rust
        cargo fmt --check
        cargo clippy --all-targets --all-features -- -D warnings
        cargo test
    )
else
    if [ "$REQUIRE_COMPONENTS" = "1" ]; then
        echo "[FAIL] core-rust/Cargo.toml missing" >&2
        exit 1
    fi
    echo "[SKIP] core-rust/ not present yet"
fi

# 4d. web/ (optional until Web companion lands; enforce with --require-web)
if [ -f "web/package.json" ]; then
    echo "[RUN] web/ detected"
    (
        cd web
        if [ -f package-lock.json ] && [ ! -d node_modules ]; then
            npm ci
        fi
        if node -e "const p=require('./package.json'); process.exit(p.scripts && p.scripts.typecheck ? 0 : 1)"; then
            npm run typecheck
        fi
        if node -e "const p=require('./package.json'); process.exit(p.scripts && p.scripts.test ? 0 : 1)"; then
            npm test
        fi
    )
else
    if [ "$REQUIRE_WEB" = "1" ]; then
        echo "[FAIL] web/package.json missing" >&2
        exit 1
    fi
    echo "[SKIP] web/ not present yet (pass --require-web to enforce)"
fi

# 4e. pipeline/
if [ -d "pipeline" ]; then
    PIPELINE_TEST_DIRS=()
    while IFS= read -r test_dir; do
        [ -n "$test_dir" ] && PIPELINE_TEST_DIRS+=("$test_dir")
    done < <(
        find pipeline \
            \( -name target -o -name __pycache__ -o -name .venv -o -name venv \) -prune \
            -o -type f -name 'test_*.py' -print \
        | while IFS= read -r f; do dirname "$f"; done \
        | LC_ALL=C sort -u
    )

    if [ "${#PIPELINE_TEST_DIRS[@]}" -gt 0 ]; then
        for test_dir in "${PIPELINE_TEST_DIRS[@]}"; do
            echo "[RUN] pipeline Python tests in $test_dir"
            python3 -m unittest discover -s "$test_dir" -p 'test_*.py' -v
        done
    else
        if [ "$REQUIRE_COMPONENTS" = "1" ]; then
            echo "[FAIL] pipeline/ present, but no test_*.py found" >&2
            exit 1
        fi
        echo "[SKIP] pipeline/ present, no test_*.py found yet"
    fi

    if [ -f "pipeline/harness/gate4/validate_gate4_dataset.py" ]; then
        echo "[RUN] Gate 4 dataset & GBNF validation"
        python3 pipeline/harness/gate4/validate_gate4_dataset.py
    elif [ "$REQUIRE_COMPONENTS" = "1" ]; then
        echo "[FAIL] pipeline/harness/gate4/validate_gate4_dataset.py missing" >&2
        exit 1
    fi
else
    if [ "$REQUIRE_COMPONENTS" = "1" ]; then
        echo "[FAIL] pipeline/ directory missing" >&2
        exit 1
    fi
    echo "[SKIP] pipeline/ not present yet"
fi

echo "=== Integration Verification Passed ==="
