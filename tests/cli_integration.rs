//! Subprocess-level integration tests that invoke the compiled `gat`
//! binary directly (via `std::process::Command`, so no extra dev-dependency
//! is needed), covering behavior that spans modules and process boundaries
//! and therefore can't be fully characterized by unit tests alone:
//! stream/exit-code behavior, running outside a Git repo, worktree `.git`
//! files, add/rm/mv partial failure, hook-mode quietness, corrupt cache
//! entries, missing remote objects, and paths with unusual characters.
//!
//! This suite documents *existing* behavior (not idealized behavior), so a
//! failing assertion here means a real, user-visible regression.
//!
//! The test bodies live in concern-specific submodules so
//! this crate stays navigable as it grows; each submodule targets one
//! coherent slice of process-boundary behavior rather than one large
//! undifferentiated file. This remains a single compiled integration-test
//! target (`cli_integration`, one binary), avoiding the extra compile/link
//! overhead of splitting it into several separate `tests/*.rs` crates.

#[path = "common/mod.rs"]
mod common;

#[path = "cli_integration/support.rs"]
mod support;

#[path = "cli_integration/blake3_layout.rs"]
mod blake3_layout;
#[path = "cli_integration/cache_corruption.rs"]
mod cache_corruption;
#[path = "cli_integration/config_errors.rs"]
mod config_errors;
#[path = "cli_integration/environment.rs"]
mod environment;
#[path = "cli_integration/error_leak_adversarial.rs"]
mod error_leak_adversarial;
#[path = "cli_integration/lifecycle_notices.rs"]
mod lifecycle_notices;
#[path = "cli_integration/mutation_failures.rs"]
mod mutation_failures;
#[path = "cli_integration/process_boundary.rs"]
mod process_boundary;
#[path = "cli_integration/release_artifact.rs"]
mod release_artifact;
#[path = "cli_integration/remote_file_backend.rs"]
mod remote_file_backend;
#[path = "cli_integration/renderer_process_contract.rs"]
mod renderer_process_contract;
#[path = "cli_integration/system_repair.rs"]
mod system_repair;
#[path = "cli_integration/transfer_errors.rs"]
mod transfer_errors;

#[path = "cli_integration/selection.rs"]
mod selection;
