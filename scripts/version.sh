#!/usr/bin/env bash
#
# Version management for graphdblite.
#
# Usage:
#   ./scripts/version.sh          # print current version and verify all artifacts match
#   ./scripts/version.sh 0.2.0    # set version across all artifacts
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKSPACE_TOML="$ROOT/Cargo.toml"
NODE_PKG="$ROOT/bindings/node/package.json"

# Extract current version from workspace Cargo.toml
get_workspace_version() {
    sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$WORKSPACE_TOML"
}

# Extract version from package.json
get_node_version() {
    grep -m1 '"version"' "$NODE_PKG" | sed 's/.*"\([0-9][^"]*\)".*/\1/'
}

# Verify all sources agree
verify() {
    local ws_ver node_ver
    ws_ver="$(get_workspace_version)"
    node_ver="$(get_node_version)"

    echo "Cargo workspace: $ws_ver"
    echo "Node package:    $node_ver"

    if [ "$ws_ver" != "$node_ver" ]; then
        echo "ERROR: version mismatch" >&2
        return 1
    fi

    echo "OK — all artifacts at $ws_ver"
}

# Set version everywhere
set_version() {
    local new_ver="$1"

    # Validate semver format
    if ! echo "$new_ver" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.]+)?$'; then
        echo "ERROR: '$new_ver' is not valid semver" >&2
        return 1
    fi

    local old_ver
    old_ver="$(get_workspace_version)"

    # Update workspace Cargo.toml
    sed -i "/^\[workspace\.package\]/,/^\[/{s/^version = \".*\"/version = \"$new_ver\"/}" "$WORKSPACE_TOML"

    # Update package.json
    sed -i "s/\"version\": \"$old_ver\"/\"version\": \"$new_ver\"/" "$NODE_PKG"

    echo "$old_ver -> $new_ver"
    verify
}

if [ $# -eq 0 ]; then
    verify
else
    set_version "$1"
fi
