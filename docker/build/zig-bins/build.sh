#!/usr/bin/env bash
set -euo pipefail

echo "==> Copying source"
cp -a /src/. /build/
cd /build

# Install cargo-zigbuild for CLI/FFI cross-compilation
cargo install cargo-zigbuild --locked

TARGET="aarch64-unknown-linux-gnu"
VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml)
echo "==> Version: $VERSION"
mkdir -p /dist/cli /dist/ffi /dist/node

# CLI
echo "==> Building CLI ($TARGET)"
cargo zigbuild --release --bin graphdblite --target "${TARGET}.2.28"
tar czf "/dist/cli/graphdblite-${VERSION}-${TARGET}.tar.gz" \
    -C "target/${TARGET}/release" graphdblite
sha256sum "/dist/cli/graphdblite-${VERSION}-${TARGET}.tar.gz" \
    > "/dist/cli/graphdblite-${VERSION}-${TARGET}.tar.gz.sha256"

# FFI
echo "==> Building FFI ($TARGET)"
cargo zigbuild --release -p graphdblite-ffi --target "${TARGET}.2.28"
staging=$(mktemp -d)
cp bindings/ffi/graphdblite.h "$staging/"
cp "target/${TARGET}/release"/libgraphdblite_ffi.{a,so} "$staging/" 2>/dev/null || true
tar czf "/dist/ffi/graphdblite-ffi-${VERSION}-${TARGET}.tar.gz" -C "$staging" .
sha256sum "/dist/ffi/graphdblite-ffi-${VERSION}-${TARGET}.tar.gz" \
    > "/dist/ffi/graphdblite-ffi-${VERSION}-${TARGET}.tar.gz.sha256"
rm -rf "$staging"

# Node.js
echo "==> Building Node.js addon ($TARGET)"
cd bindings/node
npm install
npx napi build --platform --release --target "$TARGET" --zig
for f in *.linux-arm64-gnu.node; do
    cp "$f" "/dist/node/graphdblite-${VERSION}.linux-arm64-gnu.node"
done
cd /build

# Windows-gnu FFI (for Go cgo — gcc-style static archive).
# We don't ship CLI/Node/Python for windows-gnu; those stay MSVC (via xwin).
WIN_TARGET="x86_64-pc-windows-gnu"
echo "==> Building FFI (${WIN_TARGET})"
cargo zigbuild --release -p graphdblite-ffi --target "$WIN_TARGET"
staging=$(mktemp -d)
cp bindings/ffi/graphdblite.h "$staging/"
cp "target/${WIN_TARGET}/release"/libgraphdblite_ffi.a "$staging/" 2>/dev/null || true
cp "target/${WIN_TARGET}/release"/graphdblite_ffi.dll "$staging/" 2>/dev/null || true
cp "target/${WIN_TARGET}/release"/libgraphdblite_ffi.dll.a "$staging/" 2>/dev/null || true
(cd "$staging" && zip -q "/dist/ffi/graphdblite-ffi-${VERSION}-${WIN_TARGET}.zip" ./*)
sha256sum "/dist/ffi/graphdblite-ffi-${VERSION}-${WIN_TARGET}.zip" \
    > "/dist/ffi/graphdblite-ffi-${VERSION}-${WIN_TARGET}.zip.sha256"
rm -rf "$staging"

echo "==> Done (zig-bins)"
