#!/usr/bin/env bash
set -euo pipefail

echo "==> Copying source"
cp -a /src/. /build/
cd /build

# Install cargo-zigbuild for CLI/FFI cross-compilation
cargo install cargo-zigbuild --locked

TARGET="aarch64-unknown-linux-gnu"
mkdir -p /dist/cli /dist/ffi /dist/node

# CLI
echo "==> Building CLI ($TARGET)"
cargo zigbuild --release --bin graphdblite --target "${TARGET}.2.28"
tar czf "/dist/cli/graphdblite-${TARGET}.tar.gz" \
    -C "target/${TARGET}/release" graphdblite
sha256sum "/dist/cli/graphdblite-${TARGET}.tar.gz" \
    > "/dist/cli/graphdblite-${TARGET}.tar.gz.sha256"

# FFI
echo "==> Building FFI ($TARGET)"
cargo zigbuild --release -p graphdblite-ffi --target "${TARGET}.2.28"
staging=$(mktemp -d)
cp crates/ffi/graphdblite.h "$staging/"
cp "target/${TARGET}/release"/libgraphdblite_ffi.{a,so} "$staging/" 2>/dev/null || true
tar czf "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz" -C "$staging" .
sha256sum "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz" \
    > "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz.sha256"
rm -rf "$staging"

# Node.js
echo "==> Building Node.js addon ($TARGET)"
cd crates/node
npm install
npx napi build --platform --release --target "$TARGET" --zig
cp *.linux-arm64-gnu.node /dist/node/ 2>/dev/null || true
cd /build

echo "==> Done (zig-bins)"
