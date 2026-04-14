#!/usr/bin/env bash
# Upload locally-built artifacts to a GitHub release.
#
# Usage:
#   scripts/publish-release.sh v0.1.0        # create tagged release
#   scripts/publish-release.sh --dev         # rolling dev-latest prerelease
#   scripts/publish-release.sh --dev --dry-run
#
# Requires: gh CLI authenticated with repo write access.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
DRY_RUN=false
DEV=false
TAG=""

# ── Colours ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

log()  { echo -e "${CYAN}==> ${NC}${BOLD}$*${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $*"; }
err()  { echo -e "  ${RED}✗${NC} $*" >&2; }

run() {
  if $DRY_RUN; then
    echo -e "  ${YELLOW}[dry-run]${NC} $*"
  else
    "$@"
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
      -h|--help)    print_help ;;
      --dry-run)    DRY_RUN=true; shift ;;
      --dev)        DEV=true; shift ;;
      v*)           TAG="$1"; shift ;;
      *)            err "Unknown argument: $1"; print_help ;;
    esac
  done

  if ! $DEV && [[ -z "$TAG" ]]; then
    err "Provide a tag (e.g. v0.1.0) or use --dev"
    exit 1
  fi
}

# ── Prerequisites ────────────────────────────────────────────────────────────
check_prereqs() {
  if ! command -v gh &>/dev/null; then
    err "gh CLI not found — install from https://cli.github.com"
    exit 1
  fi
  if ! gh auth status &>/dev/null 2>&1; then
    err "gh CLI not authenticated — run: gh auth login"
    exit 1
  fi
}

# ── Collect artifacts ────────────────────────────────────────────────────────
collect_artifacts() {
  local files=()

  # CLI
  for f in "$DIST_DIR"/cli/graphdblite-*.tar.gz "$DIST_DIR"/cli/graphdblite-*.zip; do
    [[ -f "$f" ]] && files+=("$f")
  done

  # FFI
  for f in "$DIST_DIR"/ffi/graphdblite-ffi-*.tar.gz "$DIST_DIR"/ffi/graphdblite-ffi-*.zip; do
    [[ -f "$f" ]] && files+=("$f")
  done

  # Node.js
  for f in "$DIST_DIR"/node/*.node; do
    [[ -f "$f" ]] && files+=("$f")
  done

  # Python wheels
  for f in "$DIST_DIR"/wheels/*.whl; do
    [[ -f "$f" ]] && files+=("$f")
  done

  # Checksums
  [[ -f "$DIST_DIR/sha256sums.txt" ]] && files+=("$DIST_DIR/sha256sums.txt")

  if [[ ${#files[@]} -eq 0 ]]; then
    err "No artifacts found in dist/ — run scripts/build-release.sh first"
    exit 1
  fi

  ARTIFACTS=("${files[@]}")
  ok "Found ${#ARTIFACTS[@]} artifacts"
}

# ── Publish ──────────────────────────────────────────────────────────────────
publish_dev() {
  log "Publishing dev-latest prerelease"

  log "  Deleting existing dev-latest (if any)"
  run gh release delete dev-latest --yes --cleanup-tag 2>/dev/null || true

  local sha
  sha=$(git -C "$REPO_ROOT" rev-parse HEAD)
  local date
  date=$(date -u +%Y-%m-%d)

  log "  Creating dev-latest release"
  run gh release create dev-latest \
    --title "Dev Build (latest main)" \
    --notes "Rolling development build from main branch.
Commit: $sha
Date: $date

## Artifacts
- **CLI binaries**: Linux (x86_64, arm64), Windows (x86_64)
- **Python wheels**: glibc + musl Linux, Windows — abi3 (Python 3.9+)
- **C FFI libraries**: Linux (x86_64, arm64), Windows (x86_64)
- **Node.js addons**: Linux (x86_64, arm64)" \
    --prerelease \
    --target "$sha" \
    "${ARTIFACTS[@]}"

  ok "Published dev-latest"
}

publish_tagged() {
  log "Publishing release $TAG"

  local sha
  sha=$(git -C "$REPO_ROOT" rev-parse HEAD)

  # Create the tag locally if it doesn't exist.
  if ! git -C "$REPO_ROOT" rev-parse "$TAG" &>/dev/null; then
    log "  Creating tag $TAG"
    run git -C "$REPO_ROOT" tag "$TAG"
  fi

  log "  Creating release $TAG"
  run gh release create "$TAG" \
    --title "$TAG" \
    --generate-notes \
    "${ARTIFACTS[@]}"

  ok "Published $TAG"
  echo ""
  echo -e "${BOLD}Next steps:${NC}"
  echo "  git push origin $TAG   # triggers macOS CI builds (appended to this release)"
}

# ── Main ─────────────────────────────────────────────────────────────────────
main() {
  parse_args "$@"
  check_prereqs
  collect_artifacts

  if $DEV; then
    publish_dev
  else
    publish_tagged
  fi
}

main "$@"
