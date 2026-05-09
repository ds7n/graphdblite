#!/usr/bin/env bash
# Run the same checks as CI (dev-build.yml test job).
# Use as a pre-push hook: cp scripts/check.sh .git/hooks/pre-push
set -e

# Bump rustc's per-thread stack from the 8MB default. Cold-cache test
# builds (`cargo test --workspace` after `cargo clean`) have hit
# SIGSEGV during typeck at the default size; rustc's own panic message
# recommends this exact value. Harmless when the stack isn't needed.
# (The criterion bench crate lives in its own workspace under
# `benches-crate/` and isn't typechecked by `--workspace`.)
export RUST_MIN_STACK=16777216

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

# Supply-chain / license / advisory gates (deny.toml)
if command -v cargo-deny &>/dev/null; then
  echo "  cargo deny check"
  cargo deny check
else
  echo "  [skip] cargo-deny not found — install with: cargo install cargo-deny --locked"
fi

# Lint GitHub Actions workflows (integrates shellcheck automatically)
if command -v actionlint &>/dev/null; then
  echo "  actionlint (+ shellcheck)"
  actionlint
else
  echo "  [skip] actionlint not found — install with: go install github.com/rhysd/actionlint/cmd/actionlint@latest"
fi

echo "==> All checks passed."
