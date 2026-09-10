//! `gat-core`: pure domain value types shared by every
//! other `gat` crate -- lexical paths/glob patterns, path scopes,
//! semantic remote/route/mount names, content OIDs, Gix-independent Git
//! revision/commit identity, and Gix-independent history-selection
//! requests.
//!
//! Nothing in this crate touches the filesystem, `SQLite`, Gix, `OpenDAL`, or
//! any other I/O boundary, and it has zero dependencies on any other
//! workspace crate. Persistence and Git integration live in
//! `gat-io`, stateful workflows in `gat-engine`, use-case orchestration in
//! `gat-command`, and the root `gat` crate owns only application and
//! presentation concerns.
//!
//! Consumers import these semantic modules directly from `gat-core`; the
//! root application crate does not mirror them through a compatibility API.
//!
//! Extraction-only implementation helpers are not public contracts:
//!
//! ```compile_fail
//! use gat_core::lexical_path::{
//!     LexicalPath, NormalizedGlobPattern, normalize_glob_pattern,
//!     normalize_glob_pattern_with_meta, normalize_relative_path,
//!     validate_canonical_str,
//! };
//! ```
//!
//! ```compile_fail
//! use gat_core::globs::match_options;
//! ```
//!
//! ```compile_fail
//! use gat_core::lock::codec::parse_row;
//! ```
//!
//! ```compile_fail
//! use gat_core::progress::NoopBackend;
//! ```
//!
//! ```compile_fail
//! use gat_core::lexical_path::GatPath;
//! let path = GatPath::from_validated_canonical("data/a.bin".to_string());
//! ```
//!
//! Raw path/glob dispatch is owned by command selection, not the lock codec:
//!
//! ```compile_fail
//! fn remove(lock: &mut gat_core::lock::Lock) {
//!     let _ = lock.remove_matching("data/*.bin");
//! }
//! ```
//!
//! Raw selection construction belongs to the application boundary. Domain
//! callers provide normalized scopes and compiled patterns:
//!
//! ```compile_fail
//! let _ = gat_core::selection::Selection::resolve(None, &[], &[]);
//! ```
//!
//! ```compile_fail
//! let _ = gat_core::selection::Selection::resolve_patterns(None, &[], &[]);
//! ```
//!
//! ```compile_fail
//! let _ = gat_core::selection::Selection::from_scope(
//!     gat_core::path_scope::PathScope::Root,
//!     &[],
//!     &[],
//! );
//! ```
//!
//! ```compile_fail
//! use gat_core::selection::SelectionError;
//! ```
//!
//! ```compile_fail
//! use gat_core::globs::GlobFilter;
//! ```

pub mod cache_location;
pub mod config;
pub mod config_keys;
pub mod endpoint;
#[cfg(any(test, feature = "test-support"))]
pub mod fault;
pub mod git;
pub mod git_ignore;
pub mod git_location;
pub mod globs;
pub mod history;
pub mod lexical_path;
pub mod lifecycle;
pub mod lock;
pub mod managed_block;
pub mod name;
pub mod newline;
pub mod oid;
pub mod path_scope;
pub mod progress;
pub mod selection;
pub mod settings;
