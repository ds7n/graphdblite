# Image used by scripts/cross-link-check.sh --docker.
#
# Carries the cross compilers needed to verify the Go binding's per-platform
# libgraphdblite_ffi.a files link cleanly without installing toolchains on
# the host. Stays small by basing on the official golang slim image.
#
# darwin/arm64 is intentionally omitted — osxcross is license-encumbered and
# non-trivial to build; that target is checked by macOS GitHub Actions
# runners instead.
FROM golang:1.24-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        gcc-mingw-w64-x86-64 \
        gcc-aarch64-linux-gnu \
        ca-certificates \
        file \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /repo
