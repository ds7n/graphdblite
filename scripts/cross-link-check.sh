#!/usr/bin/env bash
# Cross-link the Go binding against each per-platform libgraphdblite_ffi.a
# from this Linux host, to verify cgo can resolve all FFI symbols for every
# target without needing a real Windows/macOS/arm64 machine.
#
# For each target with both (a) a populated bindings/go/lib/<os_arch>/
# libgraphdblite_ffi.a and (b) the matching cross C compiler installed, the
# script `go build`s a trivial program that imports the binding. A successful
# build means cgo found every symbol referenced by graphdblite.go's #cgo
# LDFLAGS against the platform's .a — i.e. the .a is wired up correctly.
#
# A real *runtime* check still needs the target OS (or an emulator). Use
# GitHub Actions hosted runners for that — this script catches everything
# locally that doesn't require executing the binary.
#
# Required cross toolchains (install on Debian/Ubuntu):
#   linux/arm64    apt install gcc-aarch64-linux-gnu
#   windows/amd64  apt install gcc-mingw-w64-x86_64
#   darwin/arm64   osxcross (not packaged; skipped by default if absent)
#
# Targets without a populated .a or a cross compiler are reported as SKIP,
# not failures — populate libs with scripts/populate-go-libs.sh first.
#
# Building the .a files locally (instead of pulling from a release):
#   - Use `cross rustc --release --target <triple> -p graphdblite-ffi \
#         --crate-type=staticlib --target-dir target-cross`
#   - The `--crate-type=staticlib` matters for x86_64-pc-windows-gnu: the
#     default build also emits a cdylib, which fails to link from a Linux
#     host (mingw can't resolve GetHostNameW the way MSVC auto-resolves
#     it from the platform SDK). Our Go binding only consumes the .a.
#   - A separate --target-dir avoids glibc mismatches between host cargo
#     (new glibc) and the cross container (older glibc) sharing target/.
#
# Usage:
#   scripts/cross-link-check.sh                    # check all platforms (host toolchains)
#   scripts/cross-link-check.sh linux/amd64 windows/amd64
#   scripts/cross-link-check.sh --docker           # run inside a sealed image
#                                                  # with all cross compilers preinstalled
#                                                  # (docker/cross-link/Dockerfile)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINDING_DIR="$REPO_ROOT/bindings/go"
LIB_DIR="$BINDING_DIR/lib"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'
log()  { echo -e "${CYAN}==> ${NC}${BOLD}$*${NC}"; }
ok()   { echo -e "  ${GREEN}OK${NC}   $*"; }
skip() { echo -e "  ${YELLOW}SKIP${NC} $*"; }
fail() { echo -e "  ${RED}FAIL${NC} $*" >&2; }

# target -> os_arch dir / GOOS / GOARCH / CC
# bindings/go/go.mod pins go 1.23 — fall back to a newer installed toolchain
# (e.g. /usr/lib/go-1.24/bin/go) if the default `go` is older.
pick_go() {
  local candidates=(go)
  for d in /usr/lib/go-*/bin /usr/local/go/bin; do
    [[ -x "$d/go" ]] && candidates+=("$d/go")
  done
  for g in "${candidates[@]}"; do
    local v maj min
    v=$("$g" env GOVERSION 2>/dev/null) || continue
    v="${v#go}"
    maj=$(echo "$v" | cut -d. -f1)
    min=$(echo "$v" | cut -d. -f2)
    if [[ "$maj" -gt 1 ]] || { [[ "$maj" -eq 1 ]] && [[ "$min" -ge 23 ]]; }; then
      echo "$g"
      return 0
    fi
  done
  return 1
}
declare -A OS_ARCH=(
  [linux/amd64]="linux_amd64"
  [linux/arm64]="linux_arm64"
  [darwin/arm64]="darwin_arm64"
  [windows/amd64]="windows_amd64"
)
declare -A CC_FOR=(
  [linux/amd64]="cc"
  [linux/arm64]="aarch64-linux-gnu-gcc"
  [darwin/arm64]="oa64-clang"
  [windows/amd64]="x86_64-w64-mingw32-gcc"
)

USE_DOCKER=false
TARGETS=()
for arg in "$@"; do
  case "$arg" in
    --docker) USE_DOCKER=true ;;
    -h|--help)
      sed -n '2,/^set -/{ /^#/s/^# \?//p }' "$0"
      exit 0
      ;;
    *) TARGETS+=("$arg") ;;
  esac
done
if [[ ${#TARGETS[@]} -eq 0 ]]; then
  TARGETS=(linux/amd64 linux/arm64 darwin/arm64 windows/amd64)
fi

if $USE_DOCKER; then
  IMAGE="graphdblite-cross-link:latest"
  log "Building $IMAGE (cached after first run)"
  docker build -q -t "$IMAGE" "$REPO_ROOT/docker/cross-link" >/dev/null
  log "Re-executing inside $IMAGE"
  exec docker run --rm \
    -v "$REPO_ROOT:/repo" \
    -w /repo \
    -e HOME=/tmp \
    "$IMAGE" \
    scripts/cross-link-check.sh "${TARGETS[@]}"
fi

GO_BIN=$(pick_go) || { fail "no Go >= 1.23 found (binding requires it)"; exit 1; }

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

# A trivial Go program that links the binding. We don't run it — we just
# need `go build` to succeed, which forces cgo to resolve every external
# symbol in libgraphdblite_ffi.a against the binding's #cgo LDFLAGS.
cat > "$WORK_DIR/main.go" <<'GO'
package main

import (
	"fmt"

	"github.com/ds7n/graphdblite/bindings/go"
)

func main() {
	// Reference exported symbols so the linker can't dead-strip them.
	var _ = graphdblite.Open
	var _ = (*graphdblite.Database)(nil)
	fmt.Println("link ok")
}
GO

cat > "$WORK_DIR/go.mod" <<GO
module crosslinkcheck

go 1.23

require github.com/ds7n/graphdblite/bindings/go v0.0.0-00010101000000-000000000000

replace github.com/ds7n/graphdblite/bindings/go => $BINDING_DIR
GO

: > "$WORK_DIR/go.sum"

log "Cross-link check across ${#TARGETS[@]} target(s)"

failures=0
ran=0
for target in "${TARGETS[@]}"; do
  os_arch="${OS_ARCH[$target]:-}"
  cc="${CC_FOR[$target]:-}"
  if [[ -z "$os_arch" ]]; then
    fail "$target: unknown target"
    failures=$((failures + 1))
    continue
  fi

  lib="$LIB_DIR/$os_arch/libgraphdblite_ffi.a"
  if [[ ! -f "$lib" ]]; then
    skip "$target: $lib not populated (run scripts/populate-go-libs.sh)"
    continue
  fi

  if ! command -v "$cc" >/dev/null 2>&1; then
    skip "$target: cross compiler '$cc' not installed"
    continue
  fi

  goos="${target%/*}"
  goarch="${target#*/}"
  out="$WORK_DIR/bin-$goos-$goarch"
  log "$target  (CC=$cc, lib=$(du -h "$lib" | cut -f1))"
  if ( cd "$WORK_DIR" && \
       CGO_ENABLED=1 GOOS="$goos" GOARCH="$goarch" CC="$cc" \
       "$GO_BIN" build -o "$out" . ) 2> "$WORK_DIR/err-$goos-$goarch.log"; then
    ok "$target  -> $(file -b "$out" 2>/dev/null | head -c 80)"
    ran=$((ran + 1))
  else
    fail "$target  build failed:"
    sed 's/^/      /' "$WORK_DIR/err-$goos-$goarch.log" >&2
    failures=$((failures + 1))
  fi
done

echo
if [[ $failures -gt 0 ]]; then
  fail "$failures target(s) failed to link"
  exit 1
fi
if [[ $ran -eq 0 ]]; then
  skip "No targets ran — populate libs and/or install cross toolchains"
  exit 0
fi
ok "All checked targets linked successfully ($ran)"
