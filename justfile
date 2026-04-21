# graphdblite development tasks
# Run `just --list` to see all available recipes.
# Logs are written to logs/<recipe>-<timestamp>.log

log_dir := "logs"

# helper: ensure log dir exists and build a timestamped log path
_log recipe:
    @mkdir -p {{log_dir}}
    @echo "{{log_dir}}/{{recipe}}-$(date +%Y%m%d-%H%M%S).log"

# helper: run a command, tee plain text to log, colorize terminal via tailspin
_run recipe +cmd:
    #!/usr/bin/env bash
    set -euo pipefail
    logfile=$(just _log {{recipe}})
    {{cmd}} 2>&1 | tee "$logfile" | tspin

# Run lint + test checks (same as pre-push hook)
check:
    just _run check "scripts/check.sh"

# Run cargo test
test *args:
    just _run test "cargo test --workspace {{args}}"

# Run cargo fmt
fmt:
    just _run fmt "cargo fmt --all"

# Build all release artifacts via Docker
build:
    just _run build "docker compose -f docker/docker-compose.yml up --build"

# Build a specific Docker target (native, zig-bins, zig-wheels, xwin, macos)
build-target target:
    just _run build "docker compose -f docker/docker-compose.yml run --build --rm {{target}}"


# Publish tagged release to Forgejo (e.g. just publish v0.1.0)
publish *args:
    just _run publish "scripts/publish-release.sh {{args}}"

# Publish dev-latest prerelease to Forgejo
publish-dev:
    just _run publish "scripts/publish-release.sh --dev"

# Publish to GitHub instead of Forgejo
publish-github *args:
    just _run publish "scripts/publish-release.sh --github {{args}}"

# Publish dev-latest to GitHub
publish-github-dev:
    just _run publish "scripts/publish-release.sh --github --dev"

# Dry-run publish (Forgejo)
publish-dry *args:
    just _run publish "scripts/publish-release.sh --dry-run {{args}}"

# Dry-run dev-latest publish (Forgejo)
publish-dry-dev:
    just _run publish "scripts/publish-release.sh --dry-run --dev"


# Remove build artifacts
clean:
    rm -rf dist/ target/
    just _run clean "cargo clean"
