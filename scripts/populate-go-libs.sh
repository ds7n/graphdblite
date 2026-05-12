#!/usr/bin/env bash
# Populate bindings/go/lib/<os_arch>/libgraphdblite_ffi.a from a published release.
#
# Usage:
#   scripts/populate-go-libs.sh v0.1.0          # pull from Forgejo (default)
#   scripts/populate-go-libs.sh --github v0.1.0 # pull from GitHub release instead
#   scripts/populate-go-libs.sh --dry-run v0.1.0
#
# Fetches the four FFI archives for the given tag and extracts each
# libgraphdblite_ffi.a into bindings/go/lib/<os_arch>/. Stages the results
# with `git add -f` so they can be committed.
#
# Forgejo settings — required from env when --forgejo (the default).
# Example (typically sourced from .env at repo root):
#   FORGEJO_URL=https://forgejo.example.com
#   FORGEJO_OWNER=your-org   FORGEJO_REPO=graphdblite
# GitHub settings — defaults to ds7n/graphdblite; override with:
#   GITHUB_OWNER=your-org    GITHUB_REPO=graphdblite
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIB_DIR="$REPO_ROOT/bindings/go/lib"

# Forgejo settings — required from env when --forgejo is selected.
# Source .env if present so callers don't have to export manually.
if [[ -f "$REPO_ROOT/.env" ]]; then
  # shellcheck disable=SC1090
  source "$REPO_ROOT/.env"
fi
FORGEJO_URL="${FORGEJO_URL:-}"
FORGEJO_OWNER="${FORGEJO_OWNER:-}"
FORGEJO_REPO="${FORGEJO_REPO:-graphdblite}"

# GitHub settings — override via env.
GITHUB_OWNER="${GITHUB_OWNER:-ds7n}"
GITHUB_REPO="${GITHUB_REPO:-graphdblite}"

TARGET="forgejo"
DRY_RUN=false
TAG=""

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'
log()  { echo -e "${CYAN}==> ${NC}${BOLD}$*${NC}"; }
ok()   { echo -e "  ${GREEN}✓${NC} $*"; }
warn() { echo -e "  ${YELLOW}!${NC} $*"; }
err()  { echo -e "  ${RED}✗${NC} $*" >&2; }

print_help() {
  sed -n '2,/^set -/{ /^#/s/^# \?//p }' "$0"
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) print_help ;;
    --github)  TARGET="github" ;;
    --forgejo) TARGET="forgejo" ;;
    --dry-run) DRY_RUN=true ;;
    v*)        TAG="$1" ;;
    *)         err "Unknown arg: $1"; exit 1 ;;
  esac
  shift
done

if [[ -z "$TAG" ]]; then
  err "Missing release tag (e.g. v0.1.0)"
  exit 1
fi

if [[ "$TARGET" == "forgejo" ]]; then
  if [[ -z "$FORGEJO_URL" ]]; then
    err "FORGEJO_URL not set — add it to .env (e.g. https://forgejo.example.com)"
    exit 1
  fi
  if [[ -z "$FORGEJO_OWNER" ]]; then
    err "FORGEJO_OWNER not set — add it to .env"
    exit 1
  fi
fi

# triple -> os_arch directory under bindings/go/lib
declare -A TRIPLES=(
  [x86_64-unknown-linux-gnu]="linux_amd64"
  [aarch64-unknown-linux-gnu]="linux_arm64"
  [aarch64-apple-darwin]="darwin_arm64"
  [x86_64-pc-windows-gnu]="windows_amd64"
)

# windows-gnu is packaged as .zip by .github/workflows/dev-build.yml; the
# unix triples are .tar.gz.
archive_ext() {
  case "$1" in
    *windows-gnu) echo "zip" ;;
    *)            echo "tar.gz" ;;
  esac
}

asset_url() {
  local filename="$1"
  case "$TARGET" in
    forgejo) echo "$FORGEJO_URL/$FORGEJO_OWNER/$FORGEJO_REPO/releases/download/$TAG/$filename" ;;
    github)  echo "https://github.com/$GITHUB_OWNER/$GITHUB_REPO/releases/download/$TAG/$filename" ;;
  esac
}

run() {
  if $DRY_RUN; then
    echo -e "  ${YELLOW}[dry-run]${NC} $*"
  else
    "$@"
  fi
}

log "Populating Go FFI libs from $TARGET release $TAG"

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

populated=()
for triple in "${!TRIPLES[@]}"; do
  os_arch="${TRIPLES[$triple]}"
  ext=$(archive_ext "$triple")
  filename="graphdblite-ffi-${triple}.${ext}"
  url=$(asset_url "$filename")
  dest_dir="$LIB_DIR/$os_arch"
  dest_lib="$dest_dir/libgraphdblite_ffi.a"

  log "  $triple -> $os_arch"
  if [[ ! -d "$dest_dir" ]]; then
    err "Missing destination dir $dest_dir (per-platform layout)"
    exit 1
  fi

  archive="$WORK_DIR/$filename"
  if $DRY_RUN; then
    echo -e "    ${YELLOW}[dry-run]${NC} curl -fL $url -o $archive"
  else
    if ! curl -fL --progress-bar "$url" -o "$archive"; then
      err "Download failed: $url"
      exit 1
    fi
  fi

  extract_dir="$WORK_DIR/$triple"
  mkdir -p "$extract_dir"
  if ! $DRY_RUN; then
    case "$ext" in
      tar.gz) tar xzf "$archive" -C "$extract_dir" ;;
      zip)    unzip -q "$archive" -d "$extract_dir" ;;
    esac
    src_lib="$extract_dir/libgraphdblite_ffi.a"
    if [[ ! -f "$src_lib" ]]; then
      err "libgraphdblite_ffi.a not found in $filename"
      exit 1
    fi
    cp "$src_lib" "$dest_lib"
    ok "$(du -h "$dest_lib" | cut -f1)  $dest_lib"
  fi
  populated+=("$dest_lib")
done

log "Staging libs with git add -f"
for lib in "${populated[@]}"; do
  run git -C "$REPO_ROOT" add -f "$lib"
done

echo
log "Next steps"
echo "  git -C \"$REPO_ROOT\" status"
echo "  git -C \"$REPO_ROOT\" commit -m \"release: $TAG Go FFI libs\""
echo "  git -C \"$REPO_ROOT\" tag -a \"$TAG-go\" -m \"Go FFI libs for $TAG\""
echo "  git -C \"$REPO_ROOT\" push origin \"$TAG-go\""
