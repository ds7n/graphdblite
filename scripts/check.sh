#!/usr/bin/env bash
# Run the same checks as CI (dev-build.yml test job).
# Use as a pre-push hook: cp scripts/check.sh .git/hooks/pre-push
set -e

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "==> cargo clippy --all-targets --features python -- -D warnings"
cargo clippy --all-targets --features python -- -D warnings

echo "==> cargo test"
cargo test

echo "==> All checks passed."
