#!/usr/bin/env bash
# Run the same checks as CI (dev-build.yml test job).
# Use as a pre-push hook: cp scripts/check.sh .git/hooks/pre-push
set -e

echo "==> Running pre-push checks (mirrors CI)..."

echo "  cargo fmt --check"
cargo fmt --check --all

echo "  cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "  cargo test --workspace"
cargo test --workspace

# Security audit
if command -v cargo-audit &>/dev/null; then
  echo "  cargo audit"
  cargo audit
else
  echo "  [skip] cargo-audit not found — install with: cargo install cargo-audit"
fi

# Lint GitHub Actions workflows (integrates shellcheck automatically)
if command -v actionlint &>/dev/null; then
  echo "  actionlint (+ shellcheck)"
  actionlint
else
  echo "  [skip] actionlint not found — install with: go install github.com/rhysd/actionlint/cmd/actionlint@latest"
fi

echo "==> All checks passed."
