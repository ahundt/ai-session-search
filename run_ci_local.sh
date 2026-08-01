#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

# Run the locally reproducible subset of .github/workflows/ci.yml.
# Hosted OS/Python matrices remain CI-owned; this script verifies the current host.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

FAILED_COUNT=0
PASSED_COUNT=0
FAILED_NAMES=""
LOCK_KEY="$(printf '%s' "$SCRIPT_DIR" | cksum | awk '{print $1}')"
LOCAL_CI_LOCK="${TMPDIR:-/tmp}/ai-session-search-local-ci.${UID:-unknown}.$LOCK_KEY.lock"
if ! mkdir "$LOCAL_CI_LOCK" 2>/dev/null; then
    printf 'error: another local CI run owns %s\n' "$LOCAL_CI_LOCK" >&2
    if [ -r "$LOCAL_CI_LOCK/owner" ]; then
        printf 'owner: %s\n' "$(cat "$LOCAL_CI_LOCK/owner")" >&2
    fi
    printf 'If that process no longer exists, inspect its recovery state before removing the lock.\n' >&2
    exit 1
fi
STATE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ai-session-search-local-ci.XXXXXX")" || {
    rmdir "$LOCAL_CI_LOCK"
    exit 1
}
if ! printf 'pid=%s state=%s\n' "$$" "$STATE_ROOT" >"$LOCAL_CI_LOCK/owner"; then
    rm -rf -- "$STATE_ROOT"
    rmdir "$LOCAL_CI_LOCK"
    exit 1
fi
NATIVE_MODULE_DIR="$SCRIPT_DIR/ai_session_search"
NATIVE_QUARANTINE="$STATE_ROOT/source-native-originals"
ORIGINAL_NATIVE_MANIFEST="$STATE_ROOT/source-native-originals.tsv"
FRESH_NATIVE_ARTIFACTS=()
FRESH_NATIVE_CHECKSUMS=()
CURRENT_PYTHON_EXTENSION_READY=false
LOCAL_CI_CLEANED=false

source_native_artifacts() {
    shopt -s nullglob
    SOURCE_NATIVE_ARTIFACTS=(
        "$NATIVE_MODULE_DIR"/_native*.so
        "$NATIVE_MODULE_DIR"/_native*.pyd
    )
    shopt -u nullglob
    return 0
}

quarantine_source_native_modules() {
    local artifact name checksum
    mkdir -p "$NATIVE_QUARANTINE"
    : >"$ORIGINAL_NATIVE_MANIFEST"
    source_native_artifacts
    if [ "${#SOURCE_NATIVE_ARTIFACTS[@]}" -gt 0 ]; then
        for artifact in "${SOURCE_NATIVE_ARTIFACTS[@]}"; do
            name="$(basename "$artifact")"
            case "$name" in
                *$'\t'*|*$'\n'*)
                    printf 'error: native-module name cannot be recorded safely: %q\n' "$name" >&2
                    return 1
                    ;;
            esac
            checksum="$(cksum <"$artifact")" || return
            printf '%s\t%s\n' "$checksum" "$name" >>"$ORIGINAL_NATIVE_MANIFEST" || return
            mv -- "$artifact" "$NATIVE_QUARANTINE/$name" || return
        done
    fi
}

restore_source_native_modules() {
    local artifact name destination conflict suffix checksum expected_checksum index
    local restore_failed=false
    for ((index = 0; index < ${#FRESH_NATIVE_ARTIFACTS[@]}; index++)); do
        artifact="${FRESH_NATIVE_ARTIFACTS[$index]}"
        if [ -e "$artifact" ] || [ -L "$artifact" ]; then
            checksum="$(cksum <"$artifact")" || restore_failed=true
            if [ "$checksum" = "${FRESH_NATIVE_CHECKSUMS[$index]}" ]; then
                rm -f -- "$artifact" || restore_failed=true
            else
                printf 'warning: preserving modified native module: %s\n' "$artifact" >&2
            fi
        fi
    done
    if [ ! -r "$ORIGINAL_NATIVE_MANIFEST" ]; then
        printf 'error: missing native-module recovery manifest: %s\n' "$ORIGINAL_NATIVE_MANIFEST" >&2
        return 1
    fi
    while IFS=$'\t' read -r expected_checksum name; do
        [ -n "$name" ] || continue
            artifact="$NATIVE_QUARANTINE/$name"
            destination="$NATIVE_MODULE_DIR/$name"
            if ! { [ -e "$artifact" ] || [ -L "$artifact" ]; }; then
                if [ -e "$destination" ] || [ -L "$destination" ]; then
                    checksum="$(cksum <"$destination")" || restore_failed=true
                    if [ "$checksum" = "$expected_checksum" ]; then
                        continue
                    fi
                fi
                printf 'error: original native module is missing or changed: %s\n' "$name" >&2
                restore_failed=true
                continue
            fi
            if [ -e "$destination" ] || [ -L "$destination" ]; then
                suffix=0
                conflict="$destination.local-ci-conflict.$$"
                while [ -e "$conflict" ] || [ -L "$conflict" ]; do
                    suffix=$((suffix + 1))
                    conflict="$destination.local-ci-conflict.$$.$suffix"
                done
                mv -- "$destination" "$conflict" || {
                    restore_failed=true
                    continue
                }
                printf 'warning: preserved concurrently created native module as %s\n' "$conflict" >&2
            fi
            if mv -- "$artifact" "$destination"; then
                checksum="$(cksum <"$destination")" || restore_failed=true
                if [ "$checksum" != "$expected_checksum" ]; then
                    printf 'error: restored native module failed checksum verification: %s\n' "$destination" >&2
                    restore_failed=true
                fi
            else
                restore_failed=true
            fi
    done <"$ORIGINAL_NATIVE_MANIFEST"
    if find "$NATIVE_QUARANTINE" -mindepth 1 -print -quit | grep -q .; then
        printf 'error: unhandled native-module recovery artifacts remain in %s\n' "$NATIVE_QUARANTINE" >&2
        restore_failed=true
    fi
    [ "$restore_failed" = false ]
}

cleanup_local_ci() {
    if [ "$LOCAL_CI_CLEANED" = true ]; then
        return
    fi
    LOCAL_CI_CLEANED=true
    trap - HUP INT TERM
    if restore_source_native_modules; then
        rm -rf -- "$STATE_ROOT"
        rm -f -- "$LOCAL_CI_LOCK/owner"
        rmdir "$LOCAL_CI_LOCK" || printf 'warning: retained non-empty local CI lock: %s\n' "$LOCAL_CI_LOCK" >&2
    else
        printf 'error: native-module restoration incomplete; preserved recovery state %s and lock %s\n' "$STATE_ROOT" "$LOCAL_CI_LOCK" >&2
    fi
}

trap cleanup_local_ci EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

quarantine_source_native_modules || {
    printf 'error: failed to quarantine source native modules\n' >&2
    exit 1
}

mkdir -p "$STATE_ROOT/config"
cat >"$STATE_ROOT/config/config.toml" <<EOF
[index]
db_path = "$STATE_ROOT/index.db"

[providers.claude]
enabled = false
[providers.claude-desktop]
enabled = false
[providers.codex]
enabled = false
[providers.cursor]
enabled = false
[providers.antigravity]
enabled = false
[providers.pi]
enabled = false
[providers.aistudio]
enabled = false
[providers.gemini-cli]
enabled = false
EOF

export AI_SESSION_SEARCH_CONFIG="$STATE_ROOT/config/config.toml"
export AI_SESSION_SEARCH_CACHE_DIR="$STATE_ROOT/cache"
export CLAUDE_CONFIG_DIR="$SCRIPT_DIR/tests/aise-demo"
export NO_COLOR=1
# Reuse one workspace build graph and avoid multi-gigabyte incremental state in the full gate.
# Every value remains caller-overridable. AI_SESSION_SEARCH_RUSTC_WRAPPER exists for callers that
# need to override (including explicitly disable) an inherited machine-wide compiler wrapper.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$SCRIPT_DIR/target}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
if [ "${AI_SESSION_SEARCH_RUSTC_WRAPPER+x}" = x ]; then
    export RUSTC_WRAPPER="$AI_SESSION_SEARCH_RUSTC_WRAPPER"
fi

step() {
    local name="$1"
    shift
    printf '\n%b=== %s ===%b\n' "$BOLD" "$name" "$NC"
    if "$@"; then
        printf '%bPASSED: %s%b\n' "$GREEN" "$name" "$NC"
        PASSED_COUNT=$((PASSED_COUNT + 1))
    else
        printf '%bFAILED: %s%b\n' "$RED" "$name" "$NC"
        FAILED_COUNT=$((FAILED_COUNT + 1))
        FAILED_NAMES="$FAILED_NAMES\n  $name"
    fi
}

build_and_verify_python_artifacts() {
    local output="$STATE_ROOT/dist"
    mkdir -p "$output"
    uv run maturin build --release --locked --out "$output" || return
    uv run maturin sdist --out "$output" || return
    uv run python -m scripts.verify_release_artifacts "$output"/* || return
    local wheel
    wheel="$(find "$output" -maxdepth 1 -name '*.whl' -print -quit)"
    local python
    python="$(python_for_rust_host)" || return
    uv run python scripts/verify_python_install_methods.py \
        --artifact "$wheel" --source-root "$SCRIPT_DIR" --python "$python"
}

build_current_python_extension() {
    local artifact build_status=0
    uv run maturin develop --uv || build_status=$?
    source_native_artifacts
    if [ "${#SOURCE_NATIVE_ARTIFACTS[@]}" -gt 0 ]; then
        FRESH_NATIVE_ARTIFACTS=("${SOURCE_NATIVE_ARTIFACTS[@]}")
    else
        FRESH_NATIVE_ARTIFACTS=()
    fi
    FRESH_NATIVE_CHECKSUMS=()
    if [ "${#FRESH_NATIVE_ARTIFACTS[@]}" -gt 0 ]; then
        for artifact in "${FRESH_NATIVE_ARTIFACTS[@]}"; do
            FRESH_NATIVE_CHECKSUMS+=("$(cksum <"$artifact")") || return
        done
    fi
    if [ "$build_status" -ne 0 ]; then
        return "$build_status"
    fi
    if [ "${#SOURCE_NATIVE_ARTIFACTS[@]}" -eq 0 ]; then
        printf 'maturin did not publish a source-tree Python native module\n' >&2
        return 1
    fi
    uv run python scripts/verify_installed_distribution.py \
        --source-root "$SCRIPT_DIR" --source-native-import || return
    CURRENT_PYTHON_EXTENSION_READY=true
}

python_for_rust_host() {
    local rust_host rust_arch version candidate python_arch
    rust_host="$(rustc -vV | sed -n 's/^host: //p')"
    case "$rust_host" in
        aarch64-*) rust_arch="arm64" ;;
        x86_64-*) rust_arch="x86_64" ;;
        *)
            printf 'unsupported Rust host architecture for wheel verification: %s\n' "$rust_host" >&2
            return 1
            ;;
    esac
    for version in 3.12 3.13 3.14; do
        candidate="$(uv python find "$version" 2>/dev/null)" || continue
        python_arch="$("$candidate" -c 'import platform; print(platform.machine().lower())')" || continue
        case "$rust_arch:$python_arch" in
            arm64:arm64|arm64:aarch64|x86_64:x86_64|x86_64:amd64)
                printf '%s\n' "$candidate"
                return 0
                ;;
        esac
    done
    printf 'no installed CPython 3.12-3.14 matches Rust host %s; install one with uv python install\n' "$rust_host" >&2
    return 1
}

RELEASE_SCHEMA_CANARY=""

reject_retired_release_schema() {
    local field="$1"
    case "$RELEASE_SCHEMA_CANARY" in
        *"\"$field\""*)
            printf 'release executable still advertises retired MCP field: %s\n' "$field" >&2
            return 1
            ;;
    esac
}

build_and_verify_release_executable() {
    cargo build --release --locked --bin aise || return
    local executable="$CARGO_TARGET_DIR/release/aise"
    case "$(rustc -vV | sed -n 's/^host: //p')" in
        *-windows-*) executable="${executable}.exe" ;;
    esac
    if [ ! -f "$executable" ]; then
        printf 'cargo did not publish the release executable at %s\n' "$executable" >&2
        return 1
    fi
    uv run --no-project python scripts/render_message_search_docs.py --check --aise "$executable" || return

    local show_help
    show_help="$("$executable" show --help)" || return
    case "$show_help" in
        *--summary-items*--preview-chars*) ;;
        *)
            printf 'release executable show help is stale: expected --summary-items and --preview-chars\n' >&2
            return 1
            ;;
    esac

    RELEASE_SCHEMA_CANARY="$(
        printf '%s\n%s\n' \
            '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"local-ci","version":"1"}}}' \
            '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
            | "$executable" mcp serve
    )" || return
    case "$RELEASE_SCHEMA_CANARY" in
        *'"truncated_evidence"'*'"next_offset"'*) ;;
        *)
            printf 'release executable MCP schema is stale: expected truncated_evidence and next_offset\n' >&2
            return 1
            ;;
    esac
    reject_retired_release_schema "evidence_truncation" || return
    reject_retired_release_schema "row_truncated" || return
}

printf '%b=== AI Session Search local CI ===%b\n' "$BOLD" "$NC"
printf 'Working directory: %s\nIsolated state: %s\n' "$SCRIPT_DIR" "$STATE_ROOT"
# A cold full rebuild and a cache-warm one look identical until they finish, so state which
# one this is up front. Disabling an installed wrapper is the difference between minutes and
# tens of minutes on this workspace.
if [ -n "${RUSTC_WRAPPER:-}" ]; then
    printf 'Compiler wrapper: %s\n' "$RUSTC_WRAPPER"
elif [ "${AI_SESSION_SEARCH_RUSTC_WRAPPER+x}" = x ]; then
    printf '%bCompiler wrapper: disabled by AI_SESSION_SEARCH_RUSTC_WRAPPER%b\n' "$YELLOW" "$NC"
    printf 'Rerun as ./run_ci_local.sh to use the configured wrapper, if one works here.\n'
else
    printf 'Compiler wrapper: none configured\n'
fi
printf 'Incremental compilation: %s\n' "$([ "$CARGO_INCREMENTAL" = 0 ] && echo 'off (full rebuild)' || echo "$CARGO_INCREMENTAL")"

step "Check committed uv lockfile" uv lock --check
step "Sync locked Python development environment" uv sync --locked --all-extras
step "Build current ABI3 Python extension" build_current_python_extension
if [ "$CURRENT_PYTHON_EXTENSION_READY" != true ]; then
    printf '%berror: refusing to run Python gates without the current native extension%b\n' "$RED" "$NC" >&2
    exit 1
fi
step "Verify Python version" uv run python --version
step "Ruff" uv run ruff check .
step "mypy" uv run mypy ai_session_search tests
step "Native runtime/stub parity" uv run python -m mypy.stubtest ai_session_search --concise --ignore-disjoint-bases
step "Python tests" uv run pytest -m "not integration" --tb=short
step "Rust formatting" cargo fmt --all --check
step "Rust check" cargo check --workspace --all-targets --all-features --locked
step "Rust Clippy" cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
step "Rust tests" cargo test --workspace --all-targets --all-features --locked
step "Rust public API doctests" cargo test -p ai-session-search -p ai-session-search-api-consumer --doc --all-features --locked
step "Release executable and MCP schema" build_and_verify_release_executable
step "Python artifacts and install pathways" build_and_verify_python_artifacts

# No file arguments, so a workflow added later is checked without editing this line.
if command -v actionlint >/dev/null 2>&1; then
    step "GitHub workflow syntax" actionlint
else
    printf '\n%bSKIPPED: actionlint is not installed%b\n' "$YELLOW" "$NC"
    printf 'The workflow-security CI job runs it and blocks the merge, so a syntax error\n'
    printf 'found there costs a round trip. Install it to see it here first:\n'
    printf '  go install github.com/rhysd/actionlint/cmd/actionlint@latest\n'
fi

printf '\n%b=== Summary ===%b\nPassed: %s\n' "$BOLD" "$NC" "$PASSED_COUNT"
if [ "$FAILED_COUNT" -gt 0 ]; then
    printf '%bFailed: %s%b\n%b\n' "$RED" "$FAILED_COUNT" "$NC" "$FAILED_NAMES"
    # Disk-pressure (ENOSPC) recovery is deliberately opt-in: this gate never deletes
    # shared caches. These are the project-owned paths safe to reclaim by hand.
    printf '\nIf a step failed with "No space left on device", reclaimable project-owned paths:\n' >&2
    printf '  cargo build graph: %s\n' "$CARGO_TARGET_DIR" >&2
    printf '    cargo sweep --installed   drop artifacts from uninstalled toolchains\n' >&2
    printf '    cargo sweep --time 30     drop artifacts untouched for 30 days\n' >&2
    printf '    cargo clean               full rebuild costs minutes, not hours\n' >&2
    printf '  isolated gate state: %s (removed automatically on exit)\n' "$STATE_ROOT" >&2
    printf 'Shared caches (~/.cargo, uv cache) are never deleted by this script.\n' >&2
    exit 1
fi
printf '%bAll executed checks passed.%b\n' "$GREEN" "$NC"
