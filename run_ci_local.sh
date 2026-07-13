#!/usr/bin/env bash
# run_ci_local.sh - Run CI checks locally (mirrors .github/workflows/ci.yml)
#
# Usage:
#   ./run_ci_local.sh
#
# Runs the same steps as CI in order:
#   1. uv sync --all-extras (install deps including dev extras)
#   2. Python version verification
#   3. uv build (verify hatchling build)
#   4. aise --version (smoke test CLI)
#   5. ruff critical errors (blocking)
#   6. ruff full check (non-blocking, informational)
#   7. actionlint (validate CI workflow syntax)
#   8. pytest unit tests with CLAUDE_CONFIG_DIR isolation

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Terminal colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

FAILED_COUNT=0
PASSED_COUNT=0
FAILED_NAMES=""

step() {
    local name="$1"
    shift
    echo ""
    echo -e "${BOLD}=== $name ===${NC}"
    if "$@"; then
        echo -e "${GREEN}✓ PASSED: $name${NC}"
        PASSED_COUNT=$((PASSED_COUNT + 1))
    else
        echo -e "${RED}✗ FAILED: $name${NC}"
        FAILED_COUNT=$((FAILED_COUNT + 1))
        FAILED_NAMES="$FAILED_NAMES\n  ✗ $name"
    fi
}

step_nonblocking() {
    local name="$1"
    shift
    echo ""
    echo -e "${BOLD}=== $name (non-blocking) ===${NC}"
    if "$@"; then
        echo -e "${GREEN}✓ PASSED: $name${NC}"
    else
        echo -e "${YELLOW}⚠ ISSUES: $name (informational only, not blocking)${NC}"
    fi
    PASSED_COUNT=$((PASSED_COUNT + 1))
}

# Isolate tests from real ~/.claude -- use committed synthetic fixtures only
export CLAUDE_CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$SCRIPT_DIR/tests/aise-demo}"

echo -e "${BOLD}=== ai_session_search Local CI ===${NC}"
echo "Working directory: $SCRIPT_DIR"
echo "CLAUDE_CONFIG_DIR: $CLAUDE_CONFIG_DIR"

# Step 1: Install dependencies
# uv sync creates the venv automatically -- no explicit uv venv step needed.
# --all-extras installs [project.optional-dependencies] (pytest, ruff, mypy).
# --dev installs [tool.uv.dev-dependencies] if present (none currently, but correct per uv docs).
# uv.lock is NOT committed (.gitignore), so --locked is intentionally omitted.
step "Install dependencies (uv sync --all-extras --dev)" \
    uv sync --all-extras --dev

# Step 2: Verify Python version
step "Verify Python version" \
    uv run python --version

# Step 3: Build package (verify hatchling produces wheel/sdist)
step "Build package (uv build)" \
    uv build

# Step 4: Smoke test CLI entrypoint
step "Smoke test CLI (aise --version)" \
    uv run aise --version

# Step 5: Lint - critical errors only (blocking: syntax errors, undefined names)
# E9=syntax, F63/F7/F82=undefined names/imports
step "Lint - critical errors (blocking)" \
    uvx ruff check --select E9,F63,F7,F82 .

# Step 6: Lint - full check (non-blocking: style, imports, etc.)
step_nonblocking "Lint - full check (informational)" \
    uvx ruff check .

# Step 7: Validate CI workflow syntax
if command -v actionlint &>/dev/null; then
    step "Validate CI workflow (actionlint)" \
        actionlint .github/workflows/ci.yml
else
    echo ""
    echo -e "${YELLOW}⚠ actionlint not found -- skipping workflow validation${NC}"
    echo "  Install with: brew install actionlint"
fi

# Step 8: Unit tests with CLAUDE_CONFIG_DIR isolation
# -m 'not integration' skips tests that scan real ~/.claude (already default in pyproject.toml)
step "Unit tests (pytest -m 'not integration')" \
    uv run pytest tests/ -m 'not integration' \
        --tb=short -v \
        --junitxml=test_results_local.xml

# Summary
echo ""
echo -e "${BOLD}=== Summary ===${NC}"
echo -e "${GREEN}Passed: $PASSED_COUNT${NC}"
if [ "$FAILED_COUNT" -gt 0 ]; then
    echo -e "${RED}Failed: $FAILED_COUNT${NC}"
    echo -e "${RED}$FAILED_NAMES${NC}"
    echo ""
    exit 1
else
    echo -e "${GREEN}All checks passed!${NC}"
fi
