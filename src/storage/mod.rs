//! Storage layer: low-level SQLite KV encoding plus the node/edge/index
//! primitives that build on top of it.

pub mod encoding;
pub mod kv;

pub(crate) mod edge;
pub(crate) mod index;
pub(crate) mod node;
