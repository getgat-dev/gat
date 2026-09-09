//! Application-level mapping from each subsystem's typed error into a
//! [`crate::error::Failure`]. One file per subsystem, each providing that
//! subsystem's `impl From<SubsystemError> for Failure`.
//!
//! This is the *only* place (besides `crate::error` itself) permitted to
//! construct a `Diagnostic`/`Failure` -- `Diagnostic::new`,
//! `Failure::expected`, `Failure::infrastructure`, and `Failure::internal`
//! are all `pub(in crate::error)`, so lower layers cannot construct
//! application diagnostics; that boundary is a compiler error, not just a
//! lint finding.

#[cfg(test)]
mod adversarial_injection_tests;
pub(crate) mod app;
mod commands;
mod config;
mod desired_revision;
mod globs;
mod history;
mod interpolate;
mod lexical_path;
mod lock;
mod mount;
mod oid;
mod output;
mod path_policy;
pub mod problem;
mod remote;
mod remote_catalog;
mod remote_session;
mod repository;
mod repository_mutation;
pub mod runtime_bootstrap;
#[cfg(test)]
mod snapshot;
mod sync;
#[cfg(test)]
mod ui_layout_tests;
mod worktree_path;

// Classify the typed OS category only; raw messages and source chains are private.
pub(super) fn io_code(source: &std::io::Error) -> super::ErrorCode {
    match source.kind() {
        std::io::ErrorKind::PermissionDenied => super::ErrorCode::PermissionDenied,
        std::io::ErrorKind::StorageFull => super::ErrorCode::StorageExhausted,
        _ => super::ErrorCode::FilesystemUnavailable,
    }
}

use super::ErrorCode;
use gat_engine::{FilesystemFailureKind, LockFailureKind, StateFailureKind};

pub(super) const fn filesystem_code(kind: FilesystemFailureKind) -> ErrorCode {
    match kind {
        FilesystemFailureKind::PermissionDenied => ErrorCode::PermissionDenied,
        FilesystemFailureKind::StorageExhausted => ErrorCode::StorageExhausted,
        FilesystemFailureKind::Unavailable => ErrorCode::FilesystemUnavailable,
    }
}

pub(super) const fn state_code(kind: StateFailureKind) -> ErrorCode {
    match kind {
        StateFailureKind::Busy => ErrorCode::StateBusy,
        StateFailureKind::Corrupt => ErrorCode::StateCorrupt,
        StateFailureKind::Incompatible => ErrorCode::StateIncompatible,
        StateFailureKind::PermissionDenied => ErrorCode::PermissionDenied,
        StateFailureKind::StorageExhausted => ErrorCode::StorageExhausted,
        StateFailureKind::Unavailable => ErrorCode::StateUnavailable,
        StateFailureKind::InvalidObjectId => ErrorCode::Internal,
    }
}

pub(super) const fn lock_code(kind: LockFailureKind) -> ErrorCode {
    match kind {
        LockFailureKind::Corrupt => ErrorCode::StateCorrupt,
        LockFailureKind::Incompatible => ErrorCode::StateIncompatible,
        LockFailureKind::InvalidPath => ErrorCode::InvalidPath,
        LockFailureKind::UnsupportedFileType => ErrorCode::UnsupportedFileType,
        LockFailureKind::PermissionDenied => ErrorCode::PermissionDenied,
        LockFailureKind::StorageExhausted => ErrorCode::StorageExhausted,
        LockFailureKind::Unavailable => ErrorCode::StateUnavailable,
        LockFailureKind::RepairRequired => ErrorCode::RepairRequired,
        LockFailureKind::RepositoryLocked => ErrorCode::RepositoryLocked,
        LockFailureKind::InvalidArgument => ErrorCode::InvalidArgumentValue,
    }
}

pub(super) const fn repository_access_code(
    kind: gat_engine::RepositoryAccessFailureKind,
) -> ErrorCode {
    match kind {
        gat_engine::RepositoryAccessFailureKind::Filesystem(kind) => filesystem_code(kind),
        gat_engine::RepositoryAccessFailureKind::Lock(kind) => lock_code(kind),
        gat_engine::RepositoryAccessFailureKind::RepositoryLocked => ErrorCode::RepositoryLocked,
    }
}
