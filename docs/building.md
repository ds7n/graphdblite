# Building graphdblite

## Common targets

```bash
cargo build --release                                 # Rust library + CLI
cargo test --tests                                    # unit + integration tests
cargo test --test tck --features tck-support          # openCypher TCK conformance
cargo test --test e2e_query_tests                     # end-to-end queries
cargo test --test record_columns_golden --features tck-support  # column-header golden (BLESS=1 to update)
cargo test --test alloc_regression --features dhat-heap --release  # alloc-count gate
cargo bench --manifest-path benches-crate/Cargo.toml --target-dir target  # criterion baselines
maturin develop --release --manifest-path bindings/python/Cargo.toml  # Python wheel (dev)
```

The `tck-support` Cargo feature gates the Gherkin step definitions used by the
TCK harness so they don't bloat normal builds. `BLESS=1` regenerates golden
files instead of asserting against them.

## Reproducible release builds

`Cargo.lock` is committed and the workspace pins an MSRV
(`rust-version = "1.82"`), so a checkout at a given commit resolves to the
same dependency versions on any machine that has the pinned toolchain.

For a deterministic release binary:

```bash
# 1. Pin the toolchain (matches the workspace MSRV).
rustup toolchain install 1.82.0
rustup override set 1.82.0

# 2. Build with --locked so Cargo refuses to update Cargo.lock,
#    and --frozen so it also refuses any network access.
cargo build --release --locked --frozen --bin graphdblite
```

Notes:

- `--locked` is the important flag for reproducibility: without it Cargo may
  silently pick newer compatible versions if a registry has them.
- The build is reproducible in terms of *dependency versions and source
  inputs*. Bit-for-bit byte-identical binaries also require pinning the build
  environment (compiler version, sysroot, source paths). Use
  `RUSTFLAGS="--remap-path-prefix=$PWD=."` if you need to strip absolute paths
  from debug info.

## Audit / supply chain

```bash
cargo deny check     # advisories, bans, licenses, sources
```

Config: [`deny.toml`](../deny.toml). Wired into `scripts/check.sh` and the
`audit.yml` workflow.

## Benchmarks

Criterion benches live in a standalone workspace under `benches-crate/` so
they don't pollute the main lockfile.

```bash
cargo bench --manifest-path benches-crate/Cargo.toml --target-dir target
scripts/bench-compare.sh    # regression check vs saved baseline (needs `critcmp`)
```

## Fuzzing

Parser and parse+plan fuzz targets — requires nightly + `cargo install cargo-fuzz`:

```bash
cargo +nightly fuzz run parse
cargo +nightly fuzz run parse_and_plan
```
