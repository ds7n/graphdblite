#!/usr/bin/env bash
set -euo pipefail

echo "==> Copying source"
cp -a /src/. /build/
cd /build

MANIFEST="bindings/python/Cargo.toml"
mkdir -p /dist/wheels

# Wrapper that strips --target= flags cc-rs injects (zig rejects Rust triples).
ZIG_CC_WRAPPER="/build/docker/zig-wheels/zig-cc-wrapper.sh"

# x86_64 manylinux
echo "==> Building wheel: x86_64 manylinux_2_28"
maturin build --release --out /dist/wheels \
    --manifest-path "$MANIFEST" \
    --target x86_64-unknown-linux-gnu --manylinux 2_28 --zig

# x86_64 musllinux
echo "==> Building wheel: x86_64 musllinux_1_2"
CC_x86_64_unknown_linux_musl="$ZIG_CC_WRAPPER -target x86_64-linux-musl" \
maturin build --release --out /dist/wheels \
    --manifest-path "$MANIFEST" \
    --target x86_64-unknown-linux-musl --manylinux musllinux_1_2 --zig

# aarch64 manylinux
echo "==> Building wheel: aarch64 manylinux_2_28"
maturin build --release --out /dist/wheels \
    --manifest-path "$MANIFEST" \
    --target aarch64-unknown-linux-gnu --manylinux 2_28 --zig

# aarch64 musllinux
echo "==> Building wheel: aarch64 musllinux_1_2"
CC_aarch64_unknown_linux_musl="$ZIG_CC_WRAPPER -target aarch64-linux-musl" \
maturin build --release --out /dist/wheels \
    --manifest-path "$MANIFEST" \
    --target aarch64-unknown-linux-musl --manylinux musllinux_1_2 --zig

echo "==> Done (zig-wheels)"
