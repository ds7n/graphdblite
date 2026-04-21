#!/usr/bin/env bash
# Upload locally-built artifacts to a Forgejo or GitHub release.
#
# Usage:
#   scripts/publish-release.sh v0.1.0        # create tagged release on Forgejo
#   scripts/publish-release.sh --dev         # rolling dev-latest prerelease on Forgejo
#   scripts/publish-release.sh --github v0.1.0  # publish to GitHub instead
#   scripts/publish-release.sh --dev --dry-run
#
# Forgejo (default): reads FORGEJO_TOKEN from .env at repo root.
# GitHub (--github): requires gh CLI authenticated with repo write access.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
DRY_RUN=false
DEV=false
TAG=""
TARGET="forgejo"  # forgejo | github

# Forgejo settings
FORGEJO_URL="http://10.10.10.13:3000"
FORGEJO_OWNER="gitadmin"
FORGEJO_REPO="graphdblite"

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
      --github)     TARGET="github"; shift ;;
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
load_forgejo_token() {
  if [[ -n "${FORGEJO_TOKEN:-}" ]]; then
    return
  fi
  local envfile="$REPO_ROOT/.env"
  if [[ -f "$envfile" ]]; then
    # shellcheck disable=SC1090
    source "$envfile"
  fi
  if [[ -z "${FORGEJO_TOKEN:-}" ]]; then
    err "FORGEJO_TOKEN not set — add it to .env or export it"
    exit 1
  fi
}

check_prereqs() {
  if [[ "$TARGET" == "github" ]]; then
    if ! command -v gh &>/dev/null; then
      err "gh CLI not found — install from https://cli.github.com"
      exit 1
    fi
    if ! gh auth status &>/dev/null 2>&1; then
      err "gh CLI not authenticated — run: gh auth login"
      exit 1
    fi
  else
    load_forgejo_token
    # Quick connectivity check.
    local status
    status=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
      -H "Authorization: token $FORGEJO_TOKEN" \
      "$FORGEJO_URL/api/v1/repos/$FORGEJO_OWNER/$FORGEJO_REPO")
    if [[ "$status" != "200" ]]; then
      err "Forgejo API returned HTTP $status — check FORGEJO_URL and token"
      exit 1
    fi
    ok "Forgejo API reachable"
  fi
}

# ── Checksums ────────────────────────────────────────────────────────────────
generate_checksums() {
  log "Generating sha256sums.txt"
  local checksum_file="$DIST_DIR/sha256sums.txt"
  rm -f "$checksum_file"

  # Collect all release artifacts (not .sha256 sidecars).
  local artifacts=()
  for f in "$DIST_DIR"/cli/graphdblite-*.tar.gz "$DIST_DIR"/cli/graphdblite-*.zip \
           "$DIST_DIR"/ffi/graphdblite-ffi-*.tar.gz "$DIST_DIR"/ffi/graphdblite-ffi-*.zip \
           "$DIST_DIR"/node/*.node \
           "$DIST_DIR"/wheels/*.whl; do
    [[ -f "$f" ]] && artifacts+=("$f")
  done

  for f in "${artifacts[@]}"; do
    (cd "$(dirname "$f")" && sha256sum "$(basename "$f")") >> "$checksum_file"
  done

  ok "sha256sums.txt (${#artifacts[@]} entries)"
}

# ── Collect artifacts ────────────────────────────────────────────────────────
collect_artifacts() {
  local files=()

  # CLI (archives + sidecar checksums)
  for f in "$DIST_DIR"/cli/graphdblite-*.tar.gz "$DIST_DIR"/cli/graphdblite-*.zip \
           "$DIST_DIR"/cli/*.sha256; do
    [[ -f "$f" ]] && files+=("$f")
  done

  # FFI (archives + sidecar checksums)
  for f in "$DIST_DIR"/ffi/graphdblite-ffi-*.tar.gz "$DIST_DIR"/ffi/graphdblite-ffi-*.zip \
           "$DIST_DIR"/ffi/*.sha256; do
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

  # Combined checksums
  [[ -f "$DIST_DIR/sha256sums.txt" ]] && files+=("$DIST_DIR/sha256sums.txt")

  if [[ ${#files[@]} -eq 0 ]]; then
    err "No artifacts found in dist/ — run 'just build' first"
    exit 1
  fi

  ARTIFACTS=("${files[@]}")
  ok "Found ${#ARTIFACTS[@]} artifacts"
}

# ── Forgejo API helpers ──────────────────────────────────────────────────────
forgejo_api() {
  local method="$1" endpoint="$2"
  shift 2
  curl -s -X "$method" \
    -H "Authorization: token $FORGEJO_TOKEN" \
    -H "Content-Type: application/json" \
    "$FORGEJO_URL/api/v1/repos/$FORGEJO_OWNER/$FORGEJO_REPO$endpoint" \
    "$@"
}

forgejo_delete_release() {
  local tag="$1"
  # Get release ID by tag.
  local release_id
  release_id=$(forgejo_api GET "/releases/tags/$tag" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null || true)
  if [[ -n "$release_id" ]]; then
    forgejo_api DELETE "/releases/$release_id" > /dev/null
    ok "Deleted existing release $tag"
  fi
  # Delete the tag too.
  curl -s -X DELETE \
    -H "Authorization: token $FORGEJO_TOKEN" \
    "$FORGEJO_URL/api/v1/repos/$FORGEJO_OWNER/$FORGEJO_REPO/tags/$tag" > /dev/null 2>&1 || true
}

forgejo_create_release() {
  local tag="$1" name="$2" body="$3" prerelease="${4:-false}" target="${5:-}"
  local payload
  payload=$(python3 -c "
import json, sys
d = {'tag_name': sys.argv[1], 'name': sys.argv[2], 'body': sys.argv[3], 'prerelease': sys.argv[4] == 'true'}
if sys.argv[5]:
    d['target_commitish'] = sys.argv[5]
print(json.dumps(d))
" "$tag" "$name" "$body" "$prerelease" "$target")

  local response
  response=$(forgejo_api POST "/releases" -d "$payload")
  local release_id
  release_id=$(echo "$response" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)

  if [[ -z "$release_id" ]]; then
    err "Failed to create release: $response"
    exit 1
  fi
  echo "$release_id"
}

forgejo_upload_asset() {
  local release_id="$1" filepath="$2"
  local filename
  filename=$(basename "$filepath")

  curl -s -X POST \
    -H "Authorization: token $FORGEJO_TOKEN" \
    -F "attachment=@$filepath" \
    "$FORGEJO_URL/api/v1/repos/$FORGEJO_OWNER/$FORGEJO_REPO/releases/$release_id/assets?name=$filename" \
    > /dev/null

  ok "Uploaded $filename"
}

# ── Publish: Forgejo ─────────────────────────────────────────────────────────
forgejo_publish_dev() {
  log "Publishing dev-latest prerelease to Forgejo"

  log "  Deleting existing dev-latest (if any)"
  run forgejo_delete_release "dev-latest"

  local sha date
  sha=$(git -C "$REPO_ROOT" rev-parse HEAD)
  date=$(date -u +%Y-%m-%d)

  local body="Rolling development build from main branch.
Commit: $sha
Date: $date

## Artifacts
- **CLI binaries**: Linux (x86_64, arm64), Windows (x86_64)
- **Python wheels**: glibc + musl Linux, Windows — abi3 (Python 3.9+)
- **C FFI libraries**: Linux (x86_64, arm64), Windows (x86_64)
- **Node.js addons**: Linux (x86_64, arm64)"

  log "  Creating dev-latest release"
  local release_id
  if ! $DRY_RUN; then
    release_id=$(forgejo_create_release "dev-latest" "Dev Build (latest main)" "$body" "true" "$sha")
    ok "Created release (id: $release_id)"

    log "  Uploading artifacts"
    for f in "${ARTIFACTS[@]}"; do
      forgejo_upload_asset "$release_id" "$f"
    done
  else
    echo -e "  ${YELLOW}[dry-run]${NC} forgejo_create_release dev-latest"
    for f in "${ARTIFACTS[@]}"; do
      echo -e "  ${YELLOW}[dry-run]${NC} upload $(basename "$f")"
    done
  fi

  ok "Published dev-latest to Forgejo"
}

forgejo_publish_tagged() {
  log "Publishing release $TAG to Forgejo"

  local sha
  sha=$(git -C "$REPO_ROOT" rev-parse HEAD)

  # Create the git tag locally if it doesn't exist.
  if ! git -C "$REPO_ROOT" rev-parse "$TAG" &>/dev/null; then
    log "  Creating tag $TAG"
    run git -C "$REPO_ROOT" tag "$TAG"
  fi

  log "  Creating release $TAG"
  local release_id
  if ! $DRY_RUN; then
    release_id=$(forgejo_create_release "$TAG" "$TAG" "" "false" "$sha")
    ok "Created release (id: $release_id)"

    log "  Uploading artifacts"
    for f in "${ARTIFACTS[@]}"; do
      forgejo_upload_asset "$release_id" "$f"
    done
  else
    echo -e "  ${YELLOW}[dry-run]${NC} forgejo_create_release $TAG"
    for f in "${ARTIFACTS[@]}"; do
      echo -e "  ${YELLOW}[dry-run]${NC} upload $(basename "$f")"
    done
  fi

  ok "Published $TAG to Forgejo"
  echo ""
  echo -e "${BOLD}Next steps:${NC}"
  echo "  git push              # pushes tag to forgejo"
  echo "  git push github $TAG  # optional: also publish to GitHub with --github"
}

# ── Publish: GitHub ──────────────────────────────────────────────────────────
github_publish_dev() {
  log "Publishing dev-latest prerelease to GitHub"

  log "  Deleting existing dev-latest (if any)"
  run gh release delete dev-latest --yes --cleanup-tag --repo ds7n/graphdblite 2>/dev/null || true

  local sha date
  sha=$(git -C "$REPO_ROOT" rev-parse HEAD)
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
    --repo ds7n/graphdblite \
    "${ARTIFACTS[@]}"

  ok "Published dev-latest to GitHub"
}

github_publish_tagged() {
  log "Publishing release $TAG to GitHub"

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
    --repo ds7n/graphdblite \
    "${ARTIFACTS[@]}"

  ok "Published $TAG to GitHub"
}

# ── Main ─────────────────────────────────────────────────────────────────────
main() {
  parse_args "$@"
  check_prereqs
  generate_checksums
  collect_artifacts

  if [[ "$TARGET" == "github" ]]; then
    if $DEV; then github_publish_dev; else github_publish_tagged; fi
  else
    if $DEV; then forgejo_publish_dev; else forgejo_publish_tagged; fi
  fi
}

main "$@"
