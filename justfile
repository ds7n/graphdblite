# graphdblite development tasks
# Run `just --list` to see all available recipes.

# Run lint + test checks (same as pre-push hook)
check:
    scripts/check.sh

# Run cargo test
test *args:
    cargo test --workspace {{args}}

# Run cargo fmt
fmt:
    cargo fmt --all

# Build cross-compiled release artifacts
build *args:
    scripts/build-release.sh {{args}}

# Publish artifacts to GitHub release
publish *args:
    scripts/publish-release.sh {{args}}

# Remove build artifacts
clean:
    rm -rf dist/
    cargo clean
