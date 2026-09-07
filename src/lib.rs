//! Single module root for the `gat` crate: the `gat` binary is a thin
//! wrapper that pulls in this library rather than declaring its own,
//! separately-compiled module tree, so the binary and library share one
//! compilation identity -- types, tests, and lints never diverge between
//! `cargo build`/`cargo test` (through `app::run`) and `cargo bench`/the
//! helper tools.
//!
//! The production library surface is the application/presentation shell.
//! Semantic values are imported from `gat-core` directly.
//!
//! Semantic values are provided by `gat-core`; this crate exposes the
//! application and presentation layers.

pub mod app;
pub mod cli;
pub mod error;
pub mod lifecycle;
pub mod output;
pub mod presentation;
pub mod process_resources;
pub mod progress;
pub(crate) mod redaction;
