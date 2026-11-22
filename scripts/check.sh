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

# Compile-check feature-gated code (tck-support, dhat-heap, fuzzing). The
# `cargo test --workspace` step below does not enable any features, so
# without this gate the tck harness and alloc-regression test can break
# silently on master.
echo "  cargo check --workspace --all-targets --all-features"
cargo check --workspace --all-targets --all-features

echo "  cargo test --workspace"
cargo test --workspace

# Advisory / supply-chain / license gates (deny.toml). Required, not
# optional: a missing tool must not let a known vulnerability through.
# cargo-deny reads the same RustSec database as cargo-audit, so it is the
# single advisory gate (mirrors .github/workflows/audit.yml).
if ! command -v cargo-deny &>/dev/null; then
  echo "  [FAIL] cargo-deny not found; install with: cargo install cargo-deny --locked" >&2
  exit 1
fi
echo "  cargo deny --all-features check"
cargo deny --all-features check

# Lint GitHub Actions workflows (integrates shellcheck automatically)
if command -v actionlint &>/dev/null; then
  echo "  actionlint (+ shellcheck)"
  actionlint
else
  echo "  [skip] actionlint not found — install with: go install github.com/rhysd/actionlint/cmd/actionlint@latest"
fi

echo "==> All checks passed."
