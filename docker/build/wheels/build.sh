#!/usr/bin/env bash
# Build one Linux Python wheel using maturin inside a PyPA
# manylinux/musllinux container. Driven by env vars set by the compose
# service:
#   TARGET     — Rust target triple (e.g. x86_64-unknown-linux-gnu)
#   MANYLINUX  — maturin --manylinux value (e.g. 2_28, musllinux_1_2)
#
# The wheel emerges in /dist/wheels/ on the host (via the dist bind mount).
set -euo pipefail

: "${TARGET:?env TARGET not set (e.g. x86_64-unknown-linux-gnu)}"
: "${MANYLINUX:?env MANYLINUX not set (e.g. 2_28, musllinux_1_2)}"

echo "==> wheels: target=$TARGET manylinux=$MANYLINUX"

# Read-only bind mount → working tree, copy so cargo can write target/.
# Tar pipe with excludes avoids copying multi-GB host build artifacts
# (target/, target-cross/, dist/, .git/) that `cp -a /src/.` would haul
# in — previously caused 14+ min stalls on the copy alone.
tar -C /src \
    --exclude=./target \
    --exclude=./target-cross \
    --exclude=./dist \
    --exclude=./.git \
    --exclude=./node_modules \
    -cf - . | tar -C /build -xf -
cd /build

# Make sure the rust target is installed (matches container arch in normal
# operation but explicit doesn't hurt — handles future cross-libc tweaks).
rustup target add "$TARGET"

mkdir -p /dist/wheels

PATH="/opt/python/cp311-cp311/bin:$PATH" \
maturin build \
    --release \
    --out /dist/wheels \
    --manifest-path bindings/python/Cargo.toml \
    --target "$TARGET" \
    --manylinux "$MANYLINUX"

echo "==> wheels: done ($TARGET, $MANYLINUX)"
