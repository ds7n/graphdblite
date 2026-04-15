#!/usr/bin/env bash
set -euo pipefail

echo "==> Copying source"
cp -a /src/. /build/
cd /build

mkdir -p /dist/cli /dist/ffi /dist/node /dist/wheels

for TARGET in aarch64-apple-darwin x86_64-apple-darwin; do
    echo "==> Building for $TARGET"

    # Determine osxcross compiler prefix
    case "$TARGET" in
        aarch64-apple-darwin) CROSS_PREFIX="aarch64-apple-darwin24" ;;
        x86_64-apple-darwin)  CROSS_PREFIX="x86_64-apple-darwin24" ;;
    esac

    export CC="${CROSS_PREFIX}-clang"
    export CXX="${CROSS_PREFIX}-clang++"
    export CARGO_TARGET_$(echo "$TARGET" | tr '[:lower:]-' '[:upper:]_')_LINKER="$CC"

    # CLI
    echo "  ==> CLI ($TARGET)"
    cargo build --release --bin graphdblite --target "$TARGET"
    tar czf "/dist/cli/graphdblite-${TARGET}.tar.gz" \
        -C "target/${TARGET}/release" graphdblite
    sha256sum "/dist/cli/graphdblite-${TARGET}.tar.gz" \
        > "/dist/cli/graphdblite-${TARGET}.tar.gz.sha256"

    # FFI (cbindgen auto-skips when cross-compiling)
    echo "  ==> FFI ($TARGET)"
    cargo build --release -p graphdblite-ffi --target "$TARGET"
    staging=$(mktemp -d)
    cp crates/ffi/graphdblite.h "$staging/"
    cp "target/${TARGET}/release"/libgraphdblite_ffi.{a,dylib} "$staging/" 2>/dev/null || true
    tar czf "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz" -C "$staging" .
    sha256sum "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz" \
        > "/dist/ffi/graphdblite-ffi-${TARGET}.tar.gz.sha256"
    rm -rf "$staging"

    # Node.js
    echo "  ==> Node.js ($TARGET)"
    (cd crates/node && npm install && npx napi build --platform --release --target "$TARGET")
    cp crates/node/*.node /dist/node/ 2>/dev/null || true

    # Python wheel
    echo "  ==> Wheel ($TARGET)"
    maturin build --release --out /dist/wheels \
        --manifest-path crates/python/Cargo.toml \
        --target "$TARGET"
done

echo "==> Done (macos)"
