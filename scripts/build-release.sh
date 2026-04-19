#!/usr/bin/env bash
# Build cross-compiled release artifacts locally.
# Replaces the CI build matrix (dev-build.yml) to save GitHub Actions minutes.
#
# Usage: scripts/build-release.sh [OPTIONS] [TARGETS...]
#
# TARGETS (default: all enabled):
#   cli        Build CLI binaries
#   ffi        Build FFI libraries
#   node       Build Node.js addons
#   wheels     Build Python wheels
#   go-test    Run Go binding tests
#
# OPTIONS:
#   --enable-macos   Include macOS targets (requires osxcross)
#   --dry-run        Print commands without executing
#   --clean          Remove dist/ before building
#   -h, --help       Show this help
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
BUILD_DIR="$REPO_ROOT/target"
ENABLE_MACOS=false
DRY_RUN=false
TARGETS=()

# ── Colours ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

log()  { echo -e "${CYAN}==> ${NC}${BOLD}$*${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $*"; }
warn() { echo -e "  ${YELLOW}⚠${NC} $*"; }
err()  { echo -e "  ${RED}✗${NC} $*" >&2; }
skip() { echo -e "  ${YELLOW}skip${NC} $*"; }

# ── Helpers ──────────────────────────────────────────────────────────────────
run() {
  if $DRY_RUN; then
    echo -e "  ${YELLOW}[dry-run]${NC} $*"
  else
    "$@"
  fi
}

sha256() {
  sha256sum "$1" > "$1.sha256"
}

ensure_dir() {
  $DRY_RUN || mkdir -p "$1"
}

# Wipe the ephemeral build directory for a clean slate.
fresh_build() {
  run rm -rf "$BUILD_DIR"
}

# Run a command with CARGO_TARGET_DIR set so that cargo-xwin never
# touches the default target/ directory. Without this, xwin poisons
# target/.rustc_info.json with lld-link flags that break cbindgen's
# cargo metadata calls in subsequent native builds.
run_xwin() {
  if $DRY_RUN; then
    echo -e "  ${YELLOW}[dry-run]${NC} CARGO_TARGET_DIR=$BUILD_DIR $*"
  else
    CARGO_TARGET_DIR="$BUILD_DIR" "$@"
  fi
}

# ── Argument parsing ─────────────────────────────────────────────────────────
print_help() {
  sed -n '2,/^set -/{ /^#/s/^# \?//p }' "$0"
  exit 0
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -h|--help)       print_help ;;
      --enable-macos)  ENABLE_MACOS=true; shift ;;
      --dry-run)       DRY_RUN=true; shift ;;
      --clean)
        log "Cleaning dist/"
        rm -rf "$DIST_DIR"
        shift ;;
      cli|ffi|node|wheels|go-test)
        TARGETS+=("$1"); shift ;;
      *)
        err "Unknown argument: $1"; print_help ;;
    esac
  done
  # Default: all targets.
  if [[ ${#TARGETS[@]} -eq 0 ]]; then
    TARGETS=(cli ffi node wheels go-test)
  fi
}

# ── Prerequisites ────────────────────────────────────────────────────────────
check_prereqs() {
  log "Checking prerequisites"
  local missing=()

  for cmd in cargo rustup; do
    command -v "$cmd" &>/dev/null && ok "$cmd" || { err "$cmd not found"; missing+=("$cmd"); }
  done

  # Only check tools needed for requested targets.
  local need_cross=false need_xwin=false need_zig=false need_maturin=false
  local need_node=false need_go=false need_docker=false

  for t in "${TARGETS[@]}"; do
    case "$t" in
      cli|ffi)   need_cross=true; need_xwin=true ;;
      node)      need_zig=true; need_node=true; need_xwin=true ;;
      wheels)    need_maturin=true; need_docker=true; need_zig=true ;;
      go-test)   need_go=true ;;
    esac
  done

  $need_cross   && { command -v cross    &>/dev/null && ok "cross"     || { err "cross not found (cargo install cross --locked)"; missing+=(cross); }; }
  $need_xwin    && { command -v cargo-xwin &>/dev/null && ok "cargo-xwin" || { err "cargo-xwin not found (cargo install cargo-xwin)"; missing+=(cargo-xwin); }; }
  $need_zig     && { command -v zig      &>/dev/null && ok "zig"       || { err "zig not found"; missing+=(zig); }; }
  $need_maturin && { command -v maturin  &>/dev/null && ok "maturin"   || { err "maturin not found (pip install maturin)"; missing+=(maturin); }; }
  $need_node    && { command -v node     &>/dev/null && ok "node"      || { err "node not found"; missing+=(node); }; }
  $need_go      && { command -v go       &>/dev/null && ok "go"        || { err "go not found"; missing+=(go); }; }
  $need_docker  && { command -v docker   &>/dev/null && ok "docker"    || { warn "docker not found — manylinux wheels may fail"; }; }

  if [[ ${#missing[@]} -gt 0 ]]; then
    err "Missing required tools: ${missing[*]}"
    exit 1
  fi
}

# ── CLI binary ───────────────────────────────────────────────────────────────
build_cli() {
  log "Building CLI binaries"
  ensure_dir "$DIST_DIR/cli"

  # Linux x86_64 (native)
  fresh_build
  log "  CLI: x86_64-unknown-linux-gnu"
  run cargo build --release --bin graphdblite --target x86_64-unknown-linux-gnu --target-dir "$BUILD_DIR"
  if ! $DRY_RUN; then
    tar czf "$DIST_DIR/cli/graphdblite-x86_64-unknown-linux-gnu.tar.gz" \
      -C "$BUILD_DIR/x86_64-unknown-linux-gnu/release" graphdblite
    sha256 "$DIST_DIR/cli/graphdblite-x86_64-unknown-linux-gnu.tar.gz"
  fi
  ok "graphdblite-x86_64-unknown-linux-gnu.tar.gz"

  # Linux aarch64 (cross via Docker)
  fresh_build
  log "  CLI: aarch64-unknown-linux-gnu"
  run cross build --release --bin graphdblite --target aarch64-unknown-linux-gnu --target-dir "$BUILD_DIR"
  if ! $DRY_RUN; then
    tar czf "$DIST_DIR/cli/graphdblite-aarch64-unknown-linux-gnu.tar.gz" \
      -C "$BUILD_DIR/aarch64-unknown-linux-gnu/release" graphdblite
    sha256 "$DIST_DIR/cli/graphdblite-aarch64-unknown-linux-gnu.tar.gz"
  fi
  ok "graphdblite-aarch64-unknown-linux-gnu.tar.gz"

  # Windows x86_64 (cargo-xwin)
  fresh_build
  log "  CLI: x86_64-pc-windows-msvc"
  run_xwin cargo xwin build --release --bin graphdblite --target x86_64-pc-windows-msvc
  if ! $DRY_RUN; then
    (cd "$BUILD_DIR/x86_64-pc-windows-msvc/release" && \
      zip -q "$DIST_DIR/cli/graphdblite-x86_64-pc-windows-msvc.zip" graphdblite.exe)
    sha256 "$DIST_DIR/cli/graphdblite-x86_64-pc-windows-msvc.zip"
  fi
  ok "graphdblite-x86_64-pc-windows-msvc.zip"

  # macOS aarch64 (osxcross — disabled by default)
  if $ENABLE_MACOS; then
    fresh_build
    log "  CLI: aarch64-apple-darwin"
    run cargo build --release --bin graphdblite --target aarch64-apple-darwin --target-dir "$BUILD_DIR"
    if ! $DRY_RUN; then
      tar czf "$DIST_DIR/cli/graphdblite-aarch64-apple-darwin.tar.gz" \
        -C "$BUILD_DIR/aarch64-apple-darwin/release" graphdblite
      sha256 "$DIST_DIR/cli/graphdblite-aarch64-apple-darwin.tar.gz"
    fi
    ok "graphdblite-aarch64-apple-darwin.tar.gz"
  else
    skip "CLI: macOS targets (use --enable-macos)"
  fi
}

# ── FFI library ──────────────────────────────────────────────────────────────
build_ffi() {
  log "Building FFI libraries"
  ensure_dir "$DIST_DIR/ffi"

  local header="$REPO_ROOT/bindings/ffi/graphdblite.h"

  # Linux x86_64 (native)
  fresh_build
  log "  FFI: x86_64-unknown-linux-gnu"
  run cargo build --release -p graphdblite-ffi --target x86_64-unknown-linux-gnu --target-dir "$BUILD_DIR"
  if ! $DRY_RUN; then
    local staging
    staging=$(mktemp -d)
    cp "$header" "$staging/"
    cp "$BUILD_DIR/x86_64-unknown-linux-gnu/release"/libgraphdblite_ffi.{a,so} "$staging/" 2>/dev/null || true
    tar czf "$DIST_DIR/ffi/graphdblite-ffi-x86_64-unknown-linux-gnu.tar.gz" -C "$staging" .
    rm -rf "$staging"
    sha256 "$DIST_DIR/ffi/graphdblite-ffi-x86_64-unknown-linux-gnu.tar.gz"
  fi
  ok "graphdblite-ffi-x86_64-unknown-linux-gnu.tar.gz"

  # Linux aarch64 (cross)
  fresh_build
  log "  FFI: aarch64-unknown-linux-gnu"
  run cross build --release -p graphdblite-ffi --target aarch64-unknown-linux-gnu --target-dir "$BUILD_DIR"
  if ! $DRY_RUN; then
    local staging
    staging=$(mktemp -d)
    cp "$header" "$staging/"
    cp "$BUILD_DIR/aarch64-unknown-linux-gnu/release"/libgraphdblite_ffi.{a,so} "$staging/" 2>/dev/null || true
    tar czf "$DIST_DIR/ffi/graphdblite-ffi-aarch64-unknown-linux-gnu.tar.gz" -C "$staging" .
    rm -rf "$staging"
    sha256 "$DIST_DIR/ffi/graphdblite-ffi-aarch64-unknown-linux-gnu.tar.gz"
  fi
  ok "graphdblite-ffi-aarch64-unknown-linux-gnu.tar.gz"

  # Windows x86_64 (cargo-xwin)
  fresh_build
  log "  FFI: x86_64-pc-windows-msvc"
  run_xwin cargo xwin build --release -p graphdblite-ffi --target x86_64-pc-windows-msvc
  if ! $DRY_RUN; then
    local staging
    staging=$(mktemp -d)
    cp "$header" "$staging/"
    cp "$BUILD_DIR/x86_64-pc-windows-msvc/release"/graphdblite_ffi.{dll,dll.lib,lib} "$staging/" 2>/dev/null || true
    (cd "$staging" && zip -q "$DIST_DIR/ffi/graphdblite-ffi-x86_64-pc-windows-msvc.zip" ./*)
    rm -rf "$staging"
    sha256 "$DIST_DIR/ffi/graphdblite-ffi-x86_64-pc-windows-msvc.zip"
  fi
  ok "graphdblite-ffi-x86_64-pc-windows-msvc.zip"

  # macOS aarch64 (osxcross — disabled by default)
  if $ENABLE_MACOS; then
    fresh_build
    log "  FFI: aarch64-apple-darwin"
    run cargo build --release -p graphdblite-ffi --target aarch64-apple-darwin --target-dir "$BUILD_DIR"
    if ! $DRY_RUN; then
      local staging
      staging=$(mktemp -d)
      cp "$header" "$staging/"
      cp "$BUILD_DIR/aarch64-apple-darwin/release"/libgraphdblite_ffi.{a,dylib} "$staging/" 2>/dev/null || true
      tar czf "$DIST_DIR/ffi/graphdblite-ffi-aarch64-apple-darwin.tar.gz" -C "$staging" .
      rm -rf "$staging"
      sha256 "$DIST_DIR/ffi/graphdblite-ffi-aarch64-apple-darwin.tar.gz"
    fi
    ok "graphdblite-ffi-aarch64-apple-darwin.tar.gz"
  else
    skip "FFI: macOS targets (use --enable-macos)"
  fi
}

# ── Node.js addon ────────────────────────────────────────────────────────────
build_node() {
  log "Building Node.js addons"
  ensure_dir "$DIST_DIR/node"

  local node_dir="$REPO_ROOT/bindings/node"

  # Install npm deps if needed.
  if [[ ! -d "$node_dir/node_modules" ]]; then
    log "  Installing npm dependencies"
    (cd "$node_dir" && run npm install)
  fi

  # Linux x86_64 (native)
  fresh_build
  log "  Node: x86_64-unknown-linux-gnu"
  (cd "$node_dir" && run npx napi build --platform --release --target x86_64-unknown-linux-gnu)
  if ! $DRY_RUN; then
    cp "$node_dir"/*.linux-x64-gnu.node "$DIST_DIR/node/" 2>/dev/null || true
  fi
  ok "linux-x64-gnu.node"

  # Linux aarch64 (zig cross-compilation)
  fresh_build
  log "  Node: aarch64-unknown-linux-gnu (zig)"
  (cd "$node_dir" && run npx napi build --platform --release --target aarch64-unknown-linux-gnu --zig)
  if ! $DRY_RUN; then
    cp "$node_dir"/*.linux-arm64-gnu.node "$DIST_DIR/node/" 2>/dev/null || true
  fi
  ok "linux-arm64-gnu.node"

  # Windows x86_64 (cargo-xwin + manual rename)
  fresh_build
  log "  Node: x86_64-pc-windows-msvc (xwin)"
  run_xwin cargo xwin build --release -p graphdblite-node --target x86_64-pc-windows-msvc
  if ! $DRY_RUN; then
    cp "$BUILD_DIR/x86_64-pc-windows-msvc/release/graphdblite_node.dll" \
      "$DIST_DIR/node/graphdblite.win32-x64-msvc.node"
  fi
  ok "win32-x64-msvc.node"

  # macOS (osxcross — disabled by default)
  if $ENABLE_MACOS; then
    fresh_build
    log "  Node: aarch64-apple-darwin"
    (cd "$node_dir" && run npx napi build --platform --release --target aarch64-apple-darwin)
    if ! $DRY_RUN; then
      cp "$node_dir"/*.darwin-arm64.node "$DIST_DIR/node/" 2>/dev/null || true
    fi
    ok "darwin-arm64.node"

    fresh_build
    log "  Node: x86_64-apple-darwin"
    (cd "$node_dir" && run npx napi build --platform --release --target x86_64-apple-darwin)
    if ! $DRY_RUN; then
      cp "$node_dir"/*.darwin-x64.node "$DIST_DIR/node/" 2>/dev/null || true
    fi
    ok "darwin-x64.node"
  else
    skip "Node: macOS targets (use --enable-macos)"
  fi
}

# ── Python wheels ────────────────────────────────────────────────────────────
build_wheels() {
  log "Building Python wheels"
  ensure_dir "$DIST_DIR/wheels"

  local manifest="$REPO_ROOT/bindings/python/Cargo.toml"

  # Linux x86_64 — manylinux (zig for glibc compat)
  fresh_build
  log "  Wheels: x86_64 manylinux_2_28"
  run maturin build --release --out "$DIST_DIR/wheels" \
    --manifest-path "$manifest" \
    --target x86_64-unknown-linux-gnu --manylinux 2_28 --zig
  ok "manylinux_2_28 x86_64"

  # Linux x86_64 — musllinux (zig for musl target)
  fresh_build
  log "  Wheels: x86_64 musllinux_1_2"
  run maturin build --release --out "$DIST_DIR/wheels" \
    --manifest-path "$manifest" \
    --target x86_64-unknown-linux-musl --manylinux musllinux_1_2 --zig
  ok "musllinux_1_2 x86_64"

  # Linux aarch64 — manylinux (zig cross)
  fresh_build
  log "  Wheels: aarch64 manylinux_2_28 (zig)"
  run maturin build --release --out "$DIST_DIR/wheels" \
    --manifest-path "$manifest" \
    --target aarch64-unknown-linux-gnu --manylinux 2_28 --zig
  ok "manylinux_2_28 aarch64"

  # Linux aarch64 — musllinux (zig cross)
  fresh_build
  log "  Wheels: aarch64 musllinux_1_2 (zig)"
  run maturin build --release --out "$DIST_DIR/wheels" \
    --manifest-path "$manifest" \
    --target aarch64-unknown-linux-musl --manylinux musllinux_1_2 --zig
  ok "musllinux_1_2 aarch64"

  # Windows x86_64 (xwin)
  fresh_build
  log "  Wheels: x86_64 windows"
  run maturin build --release --out "$DIST_DIR/wheels" \
    --manifest-path "$manifest" \
    --target x86_64-pc-windows-msvc
  ok "windows x86_64"

  # macOS (osxcross — disabled by default)
  if $ENABLE_MACOS; then
    fresh_build
    log "  Wheels: aarch64 macOS"
    run maturin build --release --out "$DIST_DIR/wheels" \
      --manifest-path "$manifest" \
      --target aarch64-apple-darwin
    ok "macOS aarch64"

    fresh_build
    log "  Wheels: x86_64 macOS"
    run maturin build --release --out "$DIST_DIR/wheels" \
      --manifest-path "$manifest" \
      --target x86_64-apple-darwin
    ok "macOS x86_64"
  else
    skip "Wheels: macOS targets (use --enable-macos)"
  fi
}

# ── Go binding tests ─────────────────────────────────────────────────────────
test_go() {
  log "Running Go binding tests"
  run make -C "$REPO_ROOT/bindings/go" test
  ok "Go tests passed"
}

# ── Checksums ────────────────────────────────────────────────────────────────
combine_checksums() {
  log "Combining checksums"
  if ! $DRY_RUN; then
    cat "$DIST_DIR"/cli/*.sha256 "$DIST_DIR"/ffi/*.sha256 > "$DIST_DIR/sha256sums.txt" 2>/dev/null || true
  fi
  ok "dist/sha256sums.txt"
}

# ── Summary ──────────────────────────────────────────────────────────────────
print_summary() {
  log "Build complete"
  if ! $DRY_RUN && [[ -d "$DIST_DIR" ]]; then
    echo ""
    find "$DIST_DIR" -type f | sort | while read -r f; do
      local size
      size=$(du -h "$f" | cut -f1)
      echo -e "  ${size}\t${f#"$REPO_ROOT"/}"
    done
    echo ""
  fi
  echo -e "${GREEN}${BOLD}Done.${NC} Publish with: scripts/publish-release.sh [--dev | vX.Y.Z]"
}

# ── Main ─────────────────────────────────────────────────────────────────────
main() {
  parse_args "$@"
  check_prereqs

  for target in "${TARGETS[@]}"; do
    case "$target" in
      cli)      build_cli ;;
      ffi)      build_ffi ;;
      node)     build_node ;;
      wheels)   build_wheels ;;
      go-test)  test_go ;;
    esac
  done

  combine_checksums

  # Final cleanup — artifacts are in dist/, build dir is disposable.
  fresh_build

  print_summary
}

main "$@"
