#!/usr/bin/env bash
set -euo pipefail

echo "==> Copying source"
cp -a /src/. /build/
cd /build
git config --global --add safe.directory /build

TARGET="x86_64-unknown-linux-gnu"
mkdir -p /dist/cli /dist/ffi /dist/node

# CLI
echo "==> Building CLI ($TARGET)"
cargo build --release --bin graphdblite --target "$TARGET"
tar czf "/dist/cli/graphdblite-${TARGET}.tar.gz" \
    -C "target/${TARGET}/release" graphdblite
sha256sum "/dist/cli/graphdblite-${TARGET}.tar.gz" \
    > "/dist/cli/graphdblite-${TARGET}.tar.gz.sha256"

# FFI
echo "==> Building FFI ($TARGET)"
cargo build --release -p graphdblite-ffi --target "$TARGET"
staging=$(mktemp -d)
cp bindings/ffi/graphdblite.h "$staging/"
cp "target/${TARGET}/release"/libgraphdblite_ffi.{a,so} "$staging/" 2>/dev/null || true
tar czf "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz" -C "$staging" .
sha256sum "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz" \
    > "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz.sha256"
rm -rf "$staging"

# Node.js
echo "==> Building Node.js addon ($TARGET)"
cd bindings/node
npm install
npx napi build --platform --release --target "$TARGET"
cp *.linux-x64-gnu.node /dist/node/ 2>/dev/null || true
cd /build

# Go tests
echo "==> Running Go binding tests"
make -C bindings/go test

echo "==> Done (native)"
