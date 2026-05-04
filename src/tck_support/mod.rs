//! Shared support modules for the openCypher TCK harness (`tests/tck.rs`).
//!
//! Compiled only when the `tck-support` feature is enabled. Lives inside the
//! crate so `GraphCounts::snapshot` retains crate-internal access to the
//! storage backend (the public API does not expose the SQLite connection).
//!
//! Not part of the stable public API.

pub mod compare;
pub mod errors;
pub mod graphs;
pub mod steps;
pub mod world;
