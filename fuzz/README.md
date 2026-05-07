# Fuzz targets

`cargo-fuzz` targets for the Cypher pipeline. Goal: catch panics in
parse / plan that escape unit tests.

## Targets

- `parse` — pest grammar + AST builder only. Fastest iteration; best
  coverage per CPU-second on grammar paths.
- `parse_and_plan` — full parse → plan against a fresh in-memory
  database. Catches planner panics on parseable-but-degenerate ASTs
  (type validation walks, name-resolution unwraps, invariant asserts).

Errors are expected and ignored. Only a panic (or libFuzzer-detected
issue like OOM, timeout, leak) fails the run.

## Running

Requires nightly Rust and `cargo-fuzz`:

```bash
rustup toolchain install nightly
cargo install cargo-fuzz

# from the repo root
cargo +nightly fuzz run parse
cargo +nightly fuzz run parse_and_plan

# time-bound runs (e.g. 5 minutes)
cargo +nightly fuzz run parse -- -max_total_time=300
```

Crashes land in `fuzz/artifacts/<target>/`. Reproduce with:

```bash
cargo +nightly fuzz run parse fuzz/artifacts/parse/crash-<hash>
```

## Hidden API

The targets call `graphdblite::__fuzz::{parse, parse_and_plan}`, which
is gated behind the `fuzzing` Cargo feature in the parent crate. This
keeps internal grammar/planner entry points out of the public API
while letting fuzz targets exercise them directly.
