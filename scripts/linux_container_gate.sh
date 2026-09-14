#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
#
# Run this repository's own checks on Linux from any host that can run containers.
#
# It adds no checks. `run_ci_local.sh` and the registered benchmarks are the authorities; this
# script only supplies a Linux environment for them, because the local gate otherwise proves the
# host platform alone and the Linux, MSRV, and install-pathway evidence comes back hours later
# from hosted CI.
#
# The host checkout is never mounted. A source snapshot is streamed in, and Cargo, uv, and the
# virtual environment write to container volumes, so a Linux build cannot collide with the host
# toolchain's `target/` or replace the host `.venv`. Nothing in the container can reach the
# caller's real session index or configuration: `run_ci_local.sh` already redirects
# AI_SESSION_SEARCH_CONFIG and AI_SESSION_SEARCH_CACHE_DIR into a disposable state root, and the
# container cannot see the host home directory at all.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE_PREFIX="ai-session-search-linux"
ENGINE="${AI_SESSION_SEARCH_CONTAINER_ENGINE:-docker}"
RUST_TOOLCHAIN="stable"
PYTHON_VERSION="$(tr -d '[:space:]' <"$SCRIPT_DIR/.python-version")"
SOURCE_MODE="head"
REBUILD=0
MODE="gate"
PLATFORM=""

usage() {
    cat <<'USAGE'
Usage: scripts/linux_container_gate.sh [options] [mode] [-- command ...]

Modes:
  gate        run ./run_ci_local.sh on Linux (default)
  msrv        run the two commands the hosted msrv job runs, at the pinned toolchain
  bench-tui   build the release executable and run the registered TUI benchmark cases
  shell       start an interactive shell in the prepared container
  run         run the command after `--` in the prepared container

Options:
  --rust <toolchain>   rustup toolchain for the image (default: stable; use 1.88.0 for msrv)
  --python <version>   interpreter uv installs in the image (default: .python-version)
  --platform <target>  container platform (default: the host architecture). On Apple Silicon
                       that is linux/arm64, which the CI matrix covers with one runner; pass
                       linux/amd64 to reproduce the majority of the hosted jobs under emulation,
                       which is correct but several times slower.
  --dirty              snapshot the working tree instead of HEAD
  --rebuild            rebuild the image even when a matching tag exists
  --engine <name>      container engine (default: docker, or $AI_SESSION_SEARCH_CONTAINER_ENGINE)
  -h, --help           print this help

Concurrency comes from the engine's memory, not its CPU count: linking this crate's test
binaries takes gigabytes each, and the default job count exhausted a 7.75 GiB VM, whose OOM
killer took `rustc` and `ld` with signal 9. Set AI_SESSION_SEARCH_CONTAINER_JOBS to override.
Give the engine 8 GiB or more.

The image omits cargo-deny and actionlint: they inspect inputs that do not vary by host, and
`gate` reports each as a named skip. zizmor is resolved by run_ci_local.sh through pinned uv tool
and scans the streamed checkout offline. A green run therefore includes workflow-security evidence
alongside the Linux build, tests, packaging, and install pathways.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --rust) RUST_TOOLCHAIN="${2:?--rust needs a toolchain}"; shift 2 ;;
        --python) PYTHON_VERSION="${2:?--python needs a version}"; shift 2 ;;
        --platform) PLATFORM="${2:?--platform needs a target}"; shift 2 ;;
        --dirty) SOURCE_MODE="worktree"; shift ;;
        --rebuild) REBUILD=1; shift ;;
        --engine) ENGINE="${2:?--engine needs a name}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        --) shift; break ;;
        gate|msrv|bench-tui|shell|run) MODE="$1"; shift ;;
        *) printf 'unknown argument: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

if ! command -v "$ENGINE" >/dev/null 2>&1; then
    printf 'container engine %s is not on PATH\n' "$ENGINE" >&2
    printf 'install Docker Desktop or Podman, or pass --engine with one you have\n' >&2
    exit 1
fi
if ! "$ENGINE" info >/dev/null 2>&1; then
    printf '%s is installed but not responding; start it and retry\n' "$ENGINE" >&2
    exit 1
fi

# The uv version comes from the workflow rather than a second copy here, so a bump in
# .github/workflows/ci.yml reaches this image without anyone remembering to mirror it.
UV_VERSION="$(
    awk -F': *' '/^ *UV_VERSION:/ { gsub(/["\r]/, "", $2); print $2; exit }' \
        "$SCRIPT_DIR/.github/workflows/ci.yml"
)"
if [ -z "$UV_VERSION" ]; then
    printf 'could not read UV_VERSION from .github/workflows/ci.yml\n' >&2
    printf 'the workflow env block is the single source for it; restore the key and retry\n' >&2
    exit 1
fi

# The platform is part of every cache key. An arm64 `target/` and an amd64 one cannot share a
# directory, and an image tag that ignored the platform would silently serve the wrong one.
PLATFORM_TAG="$(printf '%s' "${PLATFORM:-host}" | tr '/' '-')"
IMAGE="$STATE_PREFIX-gate:rust-$RUST_TOOLCHAIN-uv-$UV_VERSION-py-$PYTHON_VERSION-$PLATFORM_TAG"
TARGET_VOLUME="$STATE_PREFIX-target-$RUST_TOOLCHAIN-$PLATFORM_TAG"
VENV_VOLUME="$STATE_PREFIX-venv-$PYTHON_VERSION-$PLATFORM_TAG"
UV_CACHE_VOLUME="$STATE_PREFIX-uv-cache-$PLATFORM_TAG"
CARGO_HOME_VOLUME="$STATE_PREFIX-cargo-home-$PLATFORM_TAG"

# Cap concurrent compiler and linker processes against the engine's memory, not its CPU count.
# Linking this crate's test binaries takes gigabytes each, and the first run at the default job
# count had the OOM killer take `rustc` and `ld` with signal 9 in a 7.75 GiB Docker Desktop VM.
# That surfaced as three failed gate steps with no mention of memory, so the cap is derived here
# rather than left to whoever reads the failure.
if [ -z "${AI_SESSION_SEARCH_CONTAINER_JOBS:-}" ]; then
    ENGINE_MEMORY_BYTES="$("$ENGINE" info --format '{{.MemTotal}}' 2>/dev/null || echo 0)"
    ENGINE_CPUS="$("$ENGINE" info --format '{{.NCPU}}' 2>/dev/null || echo 1)"
    case "$ENGINE_MEMORY_BYTES" in
        ''|*[!0-9]*) ENGINE_MEMORY_BYTES=0 ;;
    esac
    case "$ENGINE_CPUS" in
        ''|*[!0-9]*) ENGINE_CPUS=1 ;;
    esac
    # Two gibibytes per job is what kept the observed peak under the limit; below that the run
    # trades wall-clock for finishing at all, and the crate's serial type checking dominates
    # anyway, so a higher job count buys little.
    MEMORY_JOBS=$((ENGINE_MEMORY_BYTES / 2147483648))
    [ "$MEMORY_JOBS" -lt 1 ] && MEMORY_JOBS=1
    CONTAINER_JOBS="$MEMORY_JOBS"
    [ "$ENGINE_CPUS" -lt "$CONTAINER_JOBS" ] && CONTAINER_JOBS="$ENGINE_CPUS"
    if [ "$ENGINE_MEMORY_BYTES" -gt 0 ] && [ "$ENGINE_MEMORY_BYTES" -lt 8589934592 ]; then
        printf 'Engine memory is %s GiB; linking may still be tight. Raise it in the engine settings if a step is killed with signal 9.\n' \
            "$((ENGINE_MEMORY_BYTES / 1073741824))" >&2
    fi
else
    CONTAINER_JOBS="$AI_SESSION_SEARCH_CONTAINER_JOBS"
fi

PLATFORM_ARGS=()
if [ -n "$PLATFORM" ]; then
    PLATFORM_ARGS=(--platform "$PLATFORM")
fi

if [ "$REBUILD" -eq 1 ] || ! "$ENGINE" image inspect "$IMAGE" >/dev/null 2>&1; then
    printf 'Building %s\n' "$IMAGE"
    "$ENGINE" build ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
        --file "$SCRIPT_DIR/docker/linux-gate.Dockerfile" \
        --build-arg "RUST_TOOLCHAIN=$RUST_TOOLCHAIN" \
        --build-arg "UV_VERSION=$UV_VERSION" \
        --build-arg "PYTHON_VERSION=$PYTHON_VERSION" \
        --tag "$IMAGE" \
        "$SCRIPT_DIR/docker"
fi

STAGE="$(mktemp -d)"
cleanup() { rm -rf "$STAGE"; }
trap cleanup EXIT

case "$SOURCE_MODE" in
    head)
        # A committed snapshot, which is what the release gate is defined against. Uncommitted
        # work is reported rather than silently excluded, because a container run that quietly
        # tested a different tree than the caller is looking at is worse than no run.
        if [ -n "$(git -C "$SCRIPT_DIR" status --porcelain)" ]; then
            printf 'Working tree has uncommitted changes; running HEAD. Pass --dirty to include them.\n' >&2
        fi
        git -C "$SCRIPT_DIR" archive --format=tar HEAD >"$STAGE/source.tar"
        ;;
    worktree)
        # Git decides what ships, not an exclude list: tracked files plus new ones it would
        # accept, and nothing it ignores. Copying the directory instead dragged in macOS
        # `__pycache__`, agent state, and other ignored files, and the contract tests that scan
        # the checkout failed on them.
        #
        # COPYFILE_DISABLE=1: macOS tar writes an AppleDouble `._name` member beside every file
        # that carries extended attributes, and GNU tar in the container extracts those as
        # ordinary files. 203 of them landed in the tree, and the contract tests that read what
        # they find failed on `'utf-8' codec can't decode byte 0xa3 in position 45`.
        #
        # --no-xattrs: the same tar also records com.apple.provenance as a pax header on every
        # entry, and GNU tar prints an "unknown extended header keyword" line for each one,
        # burying the run's own output.
        git -C "$SCRIPT_DIR" ls-files -z --cached --others --exclude-standard \
            | COPYFILE_DISABLE=1 tar --create --file "$STAGE/source.tar" \
                --no-xattrs --null --files-from - -C "$SCRIPT_DIR"
        ;;
esac

BENCH_ARTIFACTS="/tmp/linux-container-benchmarks"
case "$MODE" in
    gate) CONTAINER_COMMAND=(./run_ci_local.sh) ;;
    msrv)
        CONTAINER_COMMAND=(
            bash -c 'cargo test -p ai-session-search --lib --locked --no-run
                     cargo check -p ai-session-search-api-consumer --lib --locked'
        )
        ;;
    bench-tui)
        CONTAINER_COMMAND=(
            bash -c "cargo build --release -p ai-session-search --bin aise
                     uv run python scripts/benchmark_release.py \
                         --tier subsystem \
                         --case tui-startup-list \
                         --case tui-typeahead-latency \
                         --fixture generated \
                         --artifact-dir $BENCH_ARTIFACTS"
        )
        ;;
    shell) CONTAINER_COMMAND=(bash) ;;
    run)
        if [ $# -eq 0 ]; then
            printf 'run needs a command after --\n' >&2
            exit 2
        fi
        CONTAINER_COMMAND=("$@")
        ;;
esac

INTERACTIVE=()
if [ "$MODE" = "shell" ] && [ -t 0 ]; then
    INTERACTIVE=(--interactive --tty)
fi

# Run rather than exec: the EXIT trap owns the staged snapshot, and exec would replace this
# shell before it could fire.
printf 'Running %s in %s (source: %s, jobs: %s)\n' "$MODE" "$IMAGE" "$SOURCE_MODE" "$CONTAINER_JOBS"
set +e
"$ENGINE" run --rm ${INTERACTIVE[@]+"${INTERACTIVE[@]}"} ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
    --mount "type=bind,source=$STAGE,target=/stage,readonly" \
    --mount "type=volume,source=$TARGET_VOLUME,target=/linux-target" \
    --mount "type=volume,source=$VENV_VOLUME,target=/linux-venv" \
    --mount "type=volume,source=$UV_CACHE_VOLUME,target=/linux-uv-cache" \
    --mount "type=volume,source=$CARGO_HOME_VOLUME,target=/linux-cargo" \
    --env "CARGO_BUILD_JOBS=$CONTAINER_JOBS" \
    --workdir /work \
    "$IMAGE" \
    bash -c '
        set -e
        tar --extract --file /stage/source.tar --directory /work
        # Commit the snapshot: 61 assertions in the contract suite shell out to git — status,
        # check-ignore, ls-files — and a tree with no repository failed eighteen of them for a
        # reason that has nothing to do with Linux. One commit gives the same clean checkout
        # actions/checkout gives a hosted run.
        if [ ! -e /work/.git ]; then
            git init -q -b main /work
            git -C /work -c user.email=gate@localhost -c user.name="container gate" add -A
            git -C /work -c user.email=gate@localhost -c user.name="container gate" \
                commit -q -m "container snapshot"
        fi
        exec "$@"' \
    -- "${CONTAINER_COMMAND[@]}"
STATUS=$?
set -e
exit "$STATUS"
