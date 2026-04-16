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

# Build cross-compiled release artifacts
build *args:
    just _run build "scripts/build-release.sh {{args}}"

# Publish artifacts to GitHub release
publish *args:
    just _run publish "scripts/publish-release.sh {{args}}"

# Build all via Docker (reproducible)
build-docker:
    just _run build-docker "docker compose -f docker/docker-compose.yml up --build"

# Build a specific Docker target (native, zig-bins, zig-wheels, xwin)
build-docker-target target:
    just _run build-docker-target "docker compose -f docker/docker-compose.yml run --build --rm {{target}}"

# Remove build artifacts
clean:
    rm -rf dist/ target/
    just _run clean "cargo clean"
