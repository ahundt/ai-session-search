# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

# A Linux execution environment for the checks this repository already owns.
#
# It deliberately contains no test logic. `run_ci_local.sh`, the registered benchmarks, and the
# MSRV commands are the same on every platform; what a macOS or Windows maintainer cannot do is
# run them against Linux path handling, Linux SQLite builds, and Linux process behavior without
# waiting for hosted CI. This image supplies that and nothing else, so a check only ever exists
# in one place.
#
# Not installed: cargo-deny and actionlint. They inspect inputs that do not vary by host, and
# `run_ci_local.sh` reports each as a named skip with its exact pinned install command. zizmor is
# resolved by run_ci_local.sh through pinned uv tool and scans the streamed checkout offline. A
# green container run therefore proves workflow security plus the Linux build, test, packaging,
# and install pathways, but not the dependency-advisory gate.
FROM ubuntu:24.04

# `stable` matches the `rust`, `rust-portability`, and `rust-install` CI jobs. Pass 1.88.0 to
# reproduce the `msrv` job, which is the one hosted check a local macOS gate cannot approximate.
ARG RUST_TOOLCHAIN=stable
# Required, and supplied by scripts/linux_container_gate.sh from .github/workflows/ci.yml, so a
# container run resolves the same Python environment a hosted run does.
ARG UV_VERSION
ARG PYTHON_VERSION=3.12

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        pkg-config \
        procps \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:/opt/uv/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal \
        --default-toolchain "$RUST_TOOLCHAIN" --component clippy --component rustfmt \
    && rustc --version \
    && cargo --version

RUN test -n "$UV_VERSION" \
    || { echo "the UV_VERSION build argument is required; it comes from ci.yml" >&2; exit 1; } \
    && curl -LsSf "https://astral.sh/uv/${UV_VERSION}/install.sh" \
        | env UV_INSTALL_DIR=/opt/uv/bin sh \
    && uv --version

# Resolve the interpreter at build time so a gate run does not pay to download it.
ENV UV_PYTHON_INSTALL_DIR=/opt/uv-python
RUN uv python install "$PYTHON_VERSION"

# Run as a normal user, because root is not the environment the checks describe. A test that
# makes a directory mode 0o000 and asserts the discovery warning passes as any ordinary user and
# fails as root, which bypasses the permission bits: `discovery_keeps_readable_sources_and_
# reports_denied_subtrees` is exactly that test, and hosted CI runs unprivileged.
#
# `ubuntu` is the base image's existing uid 1000. Docker seeds a new named volume from the image
# path it covers, so creating these directories with the right owner here is what makes the
# volumes writable without a privileged entrypoint.
ENV CARGO_HOME=/linux-cargo
RUN mkdir -p /work /linux-target /linux-venv /linux-uv-cache /linux-cargo \
    && chown ubuntu:ubuntu /work /linux-target /linux-venv /linux-uv-cache /linux-cargo

# No host path is mounted writable. The wrapper streams a source snapshot into /work, and every
# build artifact lands on a container volume: a Linux `target/` written into the host checkout
# would collide with the host toolchain's artifacts in the same directory, and a Linux `.venv`
# would replace the host interpreter's.
#
# Debug info is reduced to line tables for the same reason the wrapper caps `CARGO_BUILD_JOBS`:
# linking this crate's test binaries at `debuginfo=2` exhausted an 8 GiB container and the OOM
# killer took `rustc` and `ld` with signal 9, which reads as three failed gate steps rather than
# as memory. It changes what a backtrace shows, not what any test asserts.
# The environment is a subdirectory of its volume, not the volume root: uv refuses a directory
# that exists and holds anything other than a valid environment, so a single stray file at the
# mount point would fail every Python step with "not a valid Python environment".
ENV CARGO_TARGET_DIR=/linux-target \
    UV_PROJECT_ENVIRONMENT=/linux-venv/env \
    UV_CACHE_DIR=/linux-uv-cache \
    UV_LINK_MODE=copy \
    CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_DEV_DEBUG=line-tables-only \
    CARGO_PROFILE_TEST_DEBUG=line-tables-only

USER ubuntu
WORKDIR /work
