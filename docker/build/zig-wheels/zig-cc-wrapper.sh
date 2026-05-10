#!/usr/bin/env bash
# Wrapper around `zig cc` that strips --target= flags injected by cc-rs.
# cc-rs adds --target=<rust-triple> (e.g. --target=x86_64-unknown-linux-musl)
# which zig rejects because it doesn't understand Rust triples.
# cargo-zigbuild already passes the correct zig-native -target flag via the
# CC env var, so the cc-rs flag is redundant and must be removed.
args=()
for arg in "$@"; do
    [[ "$arg" == --target=* ]] && continue
    args+=("$arg")
done
exec zig cc "${args[@]}"
