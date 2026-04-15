#!/usr/bin/env bash
set -euo pipefail

echo "==> Copying source"
cp -a /src/. /build/
cd /build

TARGET="x86_64-pc-windows-msvc"
mkdir -p /dist/cli /dist/ffi /dist/node /dist/wheels

# CLI
echo "==> Building CLI ($TARGET)"
cargo xwin build --release --bin graphdblite --target "$TARGET"
(cd "target/${TARGET}/release" && \
    zip -q "/dist/cli/graphdblite-${TARGET}.zip" graphdblite.exe)
sha256sum "/dist/cli/graphdblite-${TARGET}.zip" \
    > "/dist/cli/graphdblite-${TARGET}.zip.sha256"

# FFI (cbindgen auto-skips when cross-compiling)
echo "==> Building FFI ($TARGET)"
cargo xwin build --release -p graphdblite-ffi --target "$TARGET"
staging=$(mktemp -d)
cp crates/ffi/graphdblite.h "$staging/"
cp "target/${TARGET}/release"/graphdblite_ffi.{dll,dll.lib,lib} "$staging/" 2>/dev/null || true
(cd "$staging" && zip -q "/dist/ffi/graphdblite-ffi-${TARGET}.zip" ./*)
sha256sum "/dist/ffi/graphdblite-ffi-${TARGET}.zip" \
    > "/dist/ffi/graphdblite-ffi-${TARGET}.zip.sha256"
rm -rf "$staging"

# Node.js (build cdylib with xwin, rename to .node)
echo "==> Building Node.js addon ($TARGET)"
cargo xwin build --release -p graphdblite-node --target "$TARGET"
cp "target/${TARGET}/release/graphdblite_node.dll" \
    "/dist/node/graphdblite.win32-x64-msvc.node"

# Python wheel
echo "==> Building wheel ($TARGET)"
maturin build --release --out /dist/wheels \
    --manifest-path crates/python/Cargo.toml \
    --target "$TARGET"

echo "==> Done (xwin)"
