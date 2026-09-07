//! Application-level failure/diagnostic contract: every error
//! that reaches the CLI boundary is represented as a [`Failure`] carrying a
//! user-safe [`Diagnostic`], independent of whatever technical error caused
//! it. Rendering (`output::error`) only ever sees a `&Diagnostic` -- there
//! is no API that hands a renderer the technical source or its `Display`,
//! so third-party/OS/parser error text cannot leak into normal CLI output
//! by construction (the primary invariant of this boundary).
//!
//! Diagnostic text is constructed *directly*, at the point a typed
//! subsystem error (`RepositoryError`, `ConfigError`, `RemoteError`, ...)
//! is converted into a `Failure` -- not centrally derived from
//! [`ErrorCode`] alone. `ErrorCode` stays attached for coarse,
//! product-oriented classification (useful for tests and integrations), but
//! two failures with the same code
//! can and do carry different summaries/details/hints: the code classifies
//! *what kind* of failure this is, the [`Diagnostic`] text says what to
//! tell the user about *this* occurrence of it.
//!
//! ## Typed-error conventions
//!
//! Subsystems define their own `thiserror`-derived error enums beside the
//! code they describe in their owning crate -- this module is the shared `Failure`/
//! `Diagnostic` contract every subsystem error eventually converts into,
//! not a dumping ground for every subsystem's error variants. Within those
//! subsystem errors:
//!
//! - Use `#[source]` to retain technical causality (e.g. the underlying
//!   `rusqlite::Error`/`opendal::Error`) where it's useful to keep for
//!   internal inspection, exactly as `Failure::infrastructure` does with
//!   its own boxed source.
//! - A typed error's `Display` impl is a developer/abstraction
//!   description (what you'd want in a debug log or test failure
//!   message), not necessarily the final CLI wording -- final wording is
//!   authored where the error is converted to a `Diagnostic`.
//! - Never blindly interpolate a third-party error into `Display` via
//!   `{source}`/`{0}` if that `Display` text might later be reused as
//!   user-facing text; the whole point of this architecture is that a
//!   third-party `Display` is only ever a hidden technical source, never
//!   directly shown.
//! - Store sensitive raw inputs (remote URLs, interpolated env values)
//!   only when necessary, and render them only through
//!   `redaction`'s existing safe-display helpers.

// Some typed inspection accessors exist only for tests;
// keep the allowance local to this application-level error contract.
#![allow(dead_code)]

use crate::presentation::UserLine;
use std::error::Error as StdError;
use std::fmt;

/// Per-subsystem `From<SubsystemError> for Failure` mapping impls. This is
/// the *only* place outside `crate::error` itself that constructs a
/// `Diagnostic`/`Failure` -- see the module doc there for why. Kept as a
/// submodule (rather than a sibling top-level module) so `pub(in
/// crate::error)` constructors below are reachable from it while staying
/// unreachable from `model`/`storage`/`worktree`/`git`/`atomic`/`commands`.
pub mod map;

/// Stable, coarse-grained, product-oriented classification of an
/// application failure. Intentionally does not mirror every
/// implementation-level failure mode (no `SqlitePrepareFailed`,
/// `OpendalExistsFailed`, `IoError13`, ...). Adding a new
/// subsystem-specific implementation detail is not on its own a reason to
/// add a new variant; add one only when a genuinely new *product-level*
/// failure category emerges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The current directory is not inside a Gat-managed Git repository.
    NotRepository,
    /// A repository was found but could not be opened/used (e.g. its
    /// `.git` metadata is unreadable).
    RepositoryUnavailable,
    /// `gat.yaml` (or another config source) failed to parse or contains
    /// semantically invalid values.
    InvalidConfig,
    /// A config file declares a schema version Gat does not support.
    UnsupportedConfigVersion,
    /// A CLI argument's *value* is semantically invalid in a way Clap
    /// itself cannot express (e.g. a malformed glob) -- not a syntax
    /// error, which Clap owns directly.
    InvalidArgumentValue,
    /// A path is invalid Gat-lexical syntax, independent of whether it
    /// exists.
    InvalidPath,
    /// A path resolves outside the repository it was given relative to.
    PathOutsideRepository,
    /// A path refers to a file type Gat does not support tracking
    /// (symlink, device file, socket, ...).
    UnsupportedFileType,
    /// An operation was denied by filesystem/OS permissions.
    PermissionDenied,
    /// A referenced content-addressed object does not exist where
    /// expected (local cache or remote).
    ObjectMissing,
    /// A referenced object exists but fails an integrity/content check.
    ObjectCorrupt,
    /// Gat's local object cache (`objects_dir`, its `blake3/` namespace,
    /// or its `cache.sqlite3` proof database) could not be
    /// prepared/accessed for a reason not covered by a more specific code
    /// above (e.g. `ObjectMissing`/`PermissionDenied`).
    CacheUnavailable,
    /// An operation needs a remote, but none is configured.
    RemoteNotConfigured,
    /// A named remote was requested but is not defined.
    RemoteNotFound,
    /// A remote's configuration (URL, scheme, ...) is invalid.
    RemoteInvalid,
    /// A remote could not be reached (network/DNS/timeout).
    RemoteUnavailable,
    /// A remote rejected the request due to insufficient authorization.
    RemotePermissionDenied,
    /// A remote operation failed for a reason not covered by a more
    /// specific remote code above.
    RemoteOperationFailed,
    /// One of Gat's local SQLite-backed state stores (the materialized/
    /// desired-state store, or the shared cache proof index) is
    /// locked/busy (e.g. another `gat` process holds it).
    StateBusy,
    /// One of Gat's local SQLite-backed state stores (the materialized/
    /// desired-state store, or the shared cache proof index) is corrupt.
    StateCorrupt,
    /// One of Gat's local SQLite-backed state stores (the materialized/
    /// desired-state store, or the shared cache proof index) uses a
    /// schema this build does not support.
    StateIncompatible,
    /// One of Gat's local SQLite-backed state stores (the materialized/
    /// desired-state store, or the shared cache proof index) could not
    /// be opened/used for a reason not covered by a more specific state
    /// code above.
    StateUnavailable,
    /// An underlying Git operation (via `gix`) failed.
    GitOperationFailed,
    /// The requested operation conflicts with existing tracked/worktree
    /// state and needs user resolution.
    Conflict,
    /// A path falls beneath a configured mount's target: only `gat
    /// mount` commands may create, change, or remove it.
    MountOwnedPath,
    /// Gat detected a recoverable inconsistency that requires running a
    /// repair command before the requested operation can proceed.
    RepairRequired,
    /// A local filesystem write Gat depends on (creating a temp file,
    /// writing/syncing it, or renaming it into place) failed for a reason
    /// not covered by a more specific code above (e.g.
    /// `PermissionDenied`/`StorageExhausted`).
    FilesystemUnavailable,
    /// A local filesystem write failed because the underlying storage
    /// device/volume is full (or an equivalent quota/capacity limit was
    /// hit).
    StorageExhausted,
    /// Another `gat` process holds the repository's advisory lock
    /// (`.gat/state/sync.lock`) and it could not be acquired before
    /// timing out.
    RepositoryLocked,
    /// An unexpected/unclassified internal failure; the generic safe
    /// fallback (see `Failure::internal`).
    Internal,
}

impl ErrorCode {
    /// Every `ErrorCode` variant, compiler-maintained via an exhaustive
    /// `match`: adding a new variant without adding it to this list is a
    /// compile error, not a silently stale test or list.
    #[must_use]
    pub fn all() -> &'static [Self] {
        // Touching every variant here (through the match below) forces a
        // compile error on a new/renamed variant; the returned slice is
        // this const array, kept in sync with the match by construction.
        const ALL: [ErrorCode; 30] = [
            ErrorCode::NotRepository,
            ErrorCode::RepositoryUnavailable,
            ErrorCode::InvalidConfig,
            ErrorCode::UnsupportedConfigVersion,
            ErrorCode::InvalidArgumentValue,
            ErrorCode::InvalidPath,
            ErrorCode::PathOutsideRepository,
            ErrorCode::UnsupportedFileType,
            ErrorCode::PermissionDenied,
            ErrorCode::ObjectMissing,
            ErrorCode::ObjectCorrupt,
            ErrorCode::CacheUnavailable,
            ErrorCode::RemoteNotConfigured,
            ErrorCode::RemoteNotFound,
            ErrorCode::RemoteInvalid,
            ErrorCode::RemoteUnavailable,
            ErrorCode::RemotePermissionDenied,
            ErrorCode::RemoteOperationFailed,
            ErrorCode::StateBusy,
            ErrorCode::StateCorrupt,
            ErrorCode::StateIncompatible,
            ErrorCode::StateUnavailable,
            ErrorCode::GitOperationFailed,
            ErrorCode::Conflict,
            ErrorCode::MountOwnedPath,
            ErrorCode::RepairRequired,
            ErrorCode::FilesystemUnavailable,
            ErrorCode::StorageExhausted,
            ErrorCode::RepositoryLocked,
            ErrorCode::Internal,
        ];
        // Exhaustiveness guard: fails to compile if a variant is added
        // without also adding it to `ALL` above (or vice versa, if a
        // variant is removed from the enum but left dangling here).
        const fn assert_exhaustive(code: ErrorCode) {
            match code {
                ErrorCode::NotRepository
                | ErrorCode::RepositoryUnavailable
                | ErrorCode::InvalidConfig
                | ErrorCode::UnsupportedConfigVersion
                | ErrorCode::InvalidArgumentValue
                | ErrorCode::InvalidPath
                | ErrorCode::PathOutsideRepository
                | ErrorCode::UnsupportedFileType
                | ErrorCode::PermissionDenied
                | ErrorCode::ObjectMissing
                | ErrorCode::ObjectCorrupt
                | ErrorCode::CacheUnavailable
                | ErrorCode::RemoteNotConfigured
                | ErrorCode::RemoteNotFound
                | ErrorCode::RemoteInvalid
                | ErrorCode::RemoteUnavailable
                | ErrorCode::RemotePermissionDenied
                | ErrorCode::RemoteOperationFailed
                | ErrorCode::StateBusy
                | ErrorCode::StateCorrupt
                | ErrorCode::StateIncompatible
                | ErrorCode::StateUnavailable
                | ErrorCode::GitOperationFailed
                | ErrorCode::Conflict
                | ErrorCode::MountOwnedPath
                | ErrorCode::RepairRequired
                | ErrorCode::FilesystemUnavailable
                | ErrorCode::StorageExhausted
                | ErrorCode::RepositoryLocked
                | ErrorCode::Internal => {}
            }
        }
        for code in ALL {
            assert_exhaustive(code);
        }
        &ALL
    }

    /// Stable human-facing category identifier. Not currently surfaced on
    /// the CLI, but kept stable so integrations can key off of it -- renaming a
    /// variant here (not just its Rust identifier) is a compatibility
    /// break.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NotRepository => "not_repository",
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::InvalidConfig => "invalid_config",
            Self::UnsupportedConfigVersion => "unsupported_config_version",
            Self::InvalidArgumentValue => "invalid_argument_value",
            Self::InvalidPath => "invalid_path",
            Self::PathOutsideRepository => "path_outside_repository",
            Self::UnsupportedFileType => "unsupported_file_type",
            Self::PermissionDenied => "permission_denied",
            Self::ObjectMissing => "object_missing",
            Self::ObjectCorrupt => "object_corrupt",
            Self::CacheUnavailable => "cache_unavailable",
            Self::RemoteNotConfigured => "remote_not_configured",
            Self::RemoteNotFound => "remote_not_found",
            Self::RemoteInvalid => "remote_invalid",
            Self::RemoteUnavailable => "remote_unavailable",
            Self::RemotePermissionDenied => "remote_permission_denied",
            Self::RemoteOperationFailed => "remote_operation_failed",
            Self::StateBusy => "state_busy",
            Self::StateCorrupt => "state_corrupt",
            Self::StateIncompatible => "state_incompatible",
            Self::StateUnavailable => "state_unavailable",
            Self::GitOperationFailed => "git_operation_failed",
            Self::Conflict => "conflict",
            Self::MountOwnedPath => "mount_owned_path",
            Self::RepairRequired => "repair_required",
            Self::FilesystemUnavailable => "filesystem_unavailable",
            Self::StorageExhausted => "storage_exhausted",
            Self::RepositoryLocked => "repository_locked",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// User-safe presentation data for a single failure. Contains only text
/// that has already been through any necessary redaction/authoring at
/// the point it was constructed -- `output::error` renders exactly this,
/// and nothing else, for a `Failure`. Every field is
/// [`UserLine`] (approved, terminal-safe presentation text), so raw
/// control characters/newlines from a corrupted or adversarial
/// path/remote-name/config-value can never reach the renderer, and
/// arbitrary `String`/`Display` text can never enter these fields at
/// all -- both invariants are enforced once, at construction, by
/// `UserLine`'s own closed conversion graph, rather than by every call
/// site remembering to sanitize/approve its own text.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    code: ErrorCode,
    summary: UserLine,
    details: Vec<UserLine>,
    hints: Vec<UserLine>,
    /// An identifying string rendered undecorated (never prose-wrapped) --
    /// a repo-relative path, remote name, or config key that names *what*
    /// the failure is about. Doubles as both the "path/location" and
    /// "subject/context" representation the error architecture calls for:
    /// both are, in practice, the same kind of value (an identifier that
    /// must survive rendering unmodified).
    subject: Option<UserLine>,
}

impl Diagnostic {
    /// Starts a new diagnostic with its required summary line -- the
    /// first thing a user reads on failure. Keep this concise; put
    /// explanation in `Diagnostic::with_detail` and next steps in
    /// `Diagnostic::with_hint`. Takes `impl Into<UserLine>`: an
    /// authored `&'static str` literal (the common case, a fixed product
    /// sentence per `ErrorCode`) or an already-approved dynamic
    /// [`UserLine`] (built via [`UserLine::compose`] or one of its
    /// domain-identity constructors) both work naturally. This remains
    /// safe -- not a hidden reintroduction of `impl Into<String>` --
    /// only because `String` does not implement `Into<UserLine>`, so
    /// `Diagnostic::new(code, err.to_string())` is still a compile error.
    pub(in crate::error) fn new(code: ErrorCode, summary: impl Into<UserLine>) -> Self {
        Self {
            code,
            summary: summary.into(),
            details: Vec::new(),
            hints: Vec::new(),
            subject: None,
        }
    }

    /// Attaches (or appends to) a detail line shown after the summary.
    /// Takes `impl Into<UserLine>`: an authored `&'static str` literal or
    /// an already-composed dynamic [`UserLine`] both work naturally.
    /// Callable more than once to accumulate multiple notes without
    /// overwriting prior context.
    #[must_use]
    pub(in crate::error) fn with_detail(mut self, detail: impl Into<UserLine>) -> Self {
        self.details.push(detail.into());
        self
    }

    /// Appends a `hint:` line (actionable next step the user can take).
    /// Callable more than once; hints render in the order added. Takes
    /// `impl Into<UserLine>`, exactly like [`Diagnostic::new`].
    #[must_use]
    pub(in crate::error) fn with_hint(mut self, hint: impl Into<UserLine>) -> Self {
        self.hints.push(hint.into());
        self
    }

    /// Attaches the path/remote-name/config-key this diagnostic is about.
    /// Renderers must not word-wrap this value. Takes `impl Into<UserLine>`
    /// -- a static subject works through `UserLine`'s
    /// `From<&'static str>`, a dynamic one must go through one of
    /// `UserLine`'s narrow, domain-specific constructors (`path`,
    /// `path_text`, `identifier`, `config_key`, `oid`, `redacted_url`),
    /// each of which documents exactly what identity it approves. This
    /// closes the escape hatch a bare terminal-safety-only wrapper would
    /// leave open: terminal safety alone does not prove a value was ever
    /// meant to be shown, so `diagnostic.with_subject(err.to_string())`
    /// remains a compile error.
    #[must_use]
    pub(in crate::error) fn with_subject(mut self, subject: impl Into<UserLine>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    #[must_use]
    pub const fn summary(&self) -> &str {
        self.summary.as_str()
    }

    /// Approved logical detail lines, retaining identity boundaries for wrapping.
    pub(crate) fn detail_lines(&self) -> &[UserLine] {
        &self.details
    }

    /// Approved hints, retaining identity boundaries for wrapping.
    pub(crate) fn hint_lines(&self) -> &[UserLine] {
        &self.hints
    }

    /// The accumulated detail lines, joined with `\n` (see
    /// `Diagnostic::with_detail`). `None` if no detail was ever
    /// attached.
    pub fn detail(&self) -> Option<String> {
        if self.details.is_empty() {
            None
        } else {
            Some(
                self.details
                    .iter()
                    .map(UserLine::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    }

    /// The accumulated hint lines, in the order they were attached (see
    /// `Diagnostic::with_hint`).
    pub fn hints(&self) -> Vec<String> {
        self.hints.iter().map(UserLine::to_string).collect()
    }

    pub fn subject(&self) -> Option<&str> {
        self.subject.as_ref().map(UserLine::as_str)
    }

    /// Test-only constructor for modules outside `crate::error` (the
    /// renderer's own smoke tests, `app`'s policy tests) that need a
    /// throwaway `Diagnostic` to exercise rendering/dispatch, not to
    /// author a real user-facing mapping -- production code must go
    /// through `crate::error::map` instead. Deliberately permissive
    /// (`impl Into<String>`, unlike production `Diagnostic::new`): test
    /// fixtures are not subject to the same provenance requirement.
    #[cfg(test)]
    pub(crate) fn new_for_test(code: ErrorCode, summary: impl Into<String>) -> Self {
        Self {
            code,
            summary: UserLine::identifier_for_test(summary),
            details: Vec::new(),
            hints: Vec::new(),
            subject: None,
        }
    }

    /// Test-only equivalent of `Diagnostic::with_detail` for modules
    /// outside `crate::error`; see [`Diagnostic::new_for_test`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_detail_for_test(mut self, detail: impl Into<String>) -> Self {
        self.details.push(UserLine::identifier_for_test(detail));
        self
    }

    /// Test-only equivalent of `Diagnostic::with_hint` for modules
    /// outside `crate::error`; see [`Diagnostic::new_for_test`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_hint_for_test(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(UserLine::identifier_for_test(hint));
        self
    }

    /// Test-only equivalent of [`Diagnostic::with_subject`] for modules
    /// outside `crate::error`; see [`Diagnostic::new_for_test`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_subject_for_test(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(UserLine::identifier_for_test(subject));
        self
    }
}

/// A boxed technical error, retained only for internal use (tests and
/// debugging) -- never rendered to a user. See
/// [`Failure::technical_source`].
type TechnicalSource = Box<dyn StdError + Send + Sync + 'static>;

/// [`UserProblem`]'s own technical-source storage: unlike [`Failure`]
/// (which is never cloned), `UserProblem` values are collected into
/// `Vec`-shaped outcome/report structures that commonly need to be
/// cloned wholesale -- so the retained source is reference-counted
/// (`Arc`, not `Box`) precisely so [`Clone for UserProblem`] can share
/// it instead of silently dropping it.
type SharedTechnicalSource = std::sync::Arc<dyn StdError + Send + Sync + 'static>;

/// An application-level failure: a user-safe [`Diagnostic`] plus an
/// optional hidden technical source. `Failure` intentionally exposes no
/// `Display`/`Error` impl that would make it easy to accidentally print
/// the whole thing (and thus the technical source) -- callers render via
/// [`Failure::diagnostic`] and nothing else.
#[derive(Debug)]
pub struct Failure {
    // Keep Result errors small without boxing every presentation fragment.
    diagnostic: Box<Diagnostic>,
    source: Option<TechnicalSource>,
}

impl Failure {
    /// A known, expected, user-caused failure with no technical source
    /// worth retaining (e.g. "not a repository", "unknown remote").
    pub(in crate::error) fn expected(diagnostic: Diagnostic) -> Self {
        Self {
            diagnostic: Box::new(diagnostic),
            source: None,
        }
    }

    /// A known, expected, user-caused failure that nevertheless has a
    /// concrete technical source worth retaining for internal inspection
    /// (e.g. a fail-closed refusal whose typed error carries the exact
    /// repositories/rows that made it uninspectable). Semantically this
    /// is still "expected" -- the product classification is unaffected
    /// by whether a source happens to be available -- so callers must
    /// not reach for [`Failure::infrastructure`] merely to retain a
    /// source when the failure is not actually an infrastructure fault.
    pub(in crate::error) fn expected_with_source(
        diagnostic: Diagnostic,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            diagnostic: Box::new(diagnostic),
            source: Some(Box::new(source)),
        }
    }

    /// An infrastructure failure (filesystem, remote, database, ...):
    /// carries a user-authored `diagnostic` plus the technical error that
    /// caused it, retained only for internal inspection.
    pub(in crate::error) fn infrastructure(
        diagnostic: Diagnostic,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            diagnostic: Box::new(diagnostic),
            source: Some(Box::new(source)),
        }
    }

    /// The safe fallback constructor: wraps an arbitrary technical error
    /// (any `anyhow`/third-party/internal failure not yet classified) but
    /// exposes only a generic `Internal` diagnostic. This is what makes
    /// the primary invariant hold even for errors nobody has
    /// explicitly classified yet -- it can never format `source` into the
    /// user-facing text, because it never looks at `source` for that
    /// purpose at all.
    pub(in crate::error) fn internal(source: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            diagnostic: Box::new(Diagnostic::new(
                ErrorCode::Internal,
                "Gat hit an unexpected internal error",
            )),
            source: Some(Box::new(source)),
        }
    }

    /// Test-only constructors mirroring [`Failure::expected`]/
    /// [`Failure::infrastructure`] for modules outside `crate::error`
    /// (the renderer's own smoke tests) that need a throwaway `Failure`
    /// to exercise rendering, not to author a real user-facing mapping --
    /// production code must go through `crate::error::map` instead.
    #[cfg(test)]
    pub(crate) fn expected_for_test(diagnostic: Diagnostic) -> Self {
        Self::expected(diagnostic)
    }

    #[cfg(test)]
    pub(crate) fn infrastructure_for_test(
        diagnostic: Diagnostic,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::infrastructure(diagnostic, source)
    }

    /// The user-safe diagnostic to render. This is the *only* way normal
    /// CLI rendering should ever inspect a `Failure`.
    #[must_use]
    pub const fn diagnostic(&self) -> &Diagnostic {
        &self.diagnostic
    }

    /// Internal-only accessor for the hidden technical source, for tests
    /// and debugging. Deliberately
    /// `pub(crate)`, not part of the public rendering API: nothing outside
    /// this crate (and nothing in `output::error`) should ever call this
    /// to build user-facing text.
    pub(crate) fn technical_source(&self) -> Option<&(dyn StdError + Send + Sync + 'static)> {
        self.source.as_deref()
    }

    /// The process exit code this failure maps to. Every application-level
    /// failure (as opposed to a Clap usage error,
    /// which exits `2` before `Failure` is ever constructed) exits `1`;
    /// there is currently no finer-grained mapping.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        1
    }
}

/// A safe, authored presentation of one non-fatal problem observed while a
/// command otherwise continued (repair's per-object failures, `gat system
/// inspect`'s per-domain findings, ...) -- the non-fatal counterpart of
/// [`Failure`]. Like `Failure`, it exposes only text authored at
/// construction time; any technical error retained for internal
/// inspection stays private and is never reached by rendering. Unlike
/// `Failure` it carries no [`ErrorCode`] (non-fatal findings are not
/// classified for exit-code/support-tooling purposes) and no full
/// `Diagnostic` (its `summary` is normally a single short authored
/// clause meant to be embedded into a caller's own row/line, not rendered
/// standalone with its own detail/hint layout).
///
/// Construction is restricted to `crate::error` (`pub(in crate::error)`,
/// mirroring `Diagnostic`/`Failure`): subsystem/command code builds these
/// through the dedicated [`map::problem`] constructors instead of calling
/// `UserProblem::new`/`with_source` directly, so it is a compile error --
/// not just a convention -- for a lower layer to hand raw error text
/// straight to a non-fatal finding.
#[derive(Debug)]
pub struct UserProblem {
    summary: UserLine,
    source: Option<SharedTechnicalSource>,
}

impl UserProblem {
    /// An authored non-fatal finding, with no technical source worth
    /// retaining (e.g. "missing", "shape mismatch"). Restricted to
    /// `crate::error` (including [`map`]) -- see the module-level
    /// dependency-direction rule; subsystem/command code builds these
    /// through a dedicated `crate::error::map::problem` constructor
    /// instead of calling this directly. Takes `impl Into<UserLine>`: an
    /// authored `&'static str` literal (the common case) or an
    /// already-approved dynamic [`UserLine`] both work naturally -- this
    /// remains safe (not a hidden reintroduction of `impl Into<String>`)
    /// only because `String` does not implement `Into<UserLine>`.
    pub(in crate::error) fn new(summary: impl Into<UserLine>) -> Self {
        Self {
            summary: summary.into(),
            source: None,
        }
    }

    /// An authored non-fatal finding that was caused by a concrete
    /// technical error, retained privately for internal inspection (tests
    /// and debugging) but never rendered -- exactly
    /// the same contract [`Failure::infrastructure`] gives fatal errors.
    /// `summary` takes `impl Into<UserLine>`, exactly like [`Self::new`].
    /// Restricted to `crate::error` for the same reason as [`Self::new`].
    pub(in crate::error) fn with_source(
        summary: impl Into<UserLine>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            summary: summary.into(),
            source: Some(std::sync::Arc::new(source)),
        }
    }

    /// The safe, authored text to render/embed -- a [`UserLine`], so it
    /// can never smuggle a raw newline or control sequence into whatever
    /// row/line the caller embeds it in.
    #[must_use]
    pub const fn summary(&self) -> &str {
        self.summary.as_str()
    }

    /// Internal-only accessor for the hidden technical source, for tests
    /// and debugging; it never contributes to user-facing text (mirrors
    /// [`Failure::technical_source`]).
    /// `pub(crate)`, not `pub`: only this crate's own tests/tooling may
    /// inspect it.
    pub(crate) fn technical_source(&self) -> Option<&(dyn StdError + Send + Sync + 'static)> {
        self.source.as_deref()
    }
}

impl std::fmt::Display for UserProblem {
    /// Renders only the authored summary -- never the technical source,
    /// same invariant as `Failure`'s deliberate lack of a `Display`/`Error`
    /// impl that would make it easy to leak the hidden cause. Unlike
    /// `Failure`, `UserProblem` does implement `Display`: it is routine
    /// data meant to be interpolated into a row/line by its caller (e.g.
    /// `format!("repair of object {oid} failed: {problem}")`), not a type
    /// callers must be prevented from printing directly.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.summary.as_str())
    }
}

impl Clone for UserProblem {
    /// Clones both the safe summary and the retained technical source:
    /// the source is reference-counted (see `SharedTechnicalSource`),
    /// so cloning a `UserProblem` for e.g. an outcome/report `Vec` no
    /// longer silently discards the cause it was built with.
    fn clone(&self) -> Self {
        Self {
            summary: self.summary.clone(),
            source: self.source.clone(),
        }
    }
}

impl PartialEq for UserProblem {
    /// Compares only the safe summary text -- the same field `Display`
    /// exposes and the same field tests actually assert against; a
    /// retained technical source has no meaningful equality of its own.
    fn eq(&self, other: &Self) -> bool {
        self.summary == other.summary
    }
}

impl Eq for UserProblem {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct DistinctiveTechnicalError(&'static str);

    impl fmt::Display for DistinctiveTechnicalError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl StdError for DistinctiveTechnicalError {}

    const SENTINEL: &str = "sqlite: disk I/O error at offset 0x4f2c (os error 13)";

    #[test]
    fn internal_fallback_diagnostic_never_contains_source_display() {
        let failure = Failure::internal(DistinctiveTechnicalError(SENTINEL));
        let diagnostic = failure.diagnostic();
        assert!(!diagnostic.summary().contains(SENTINEL));
        assert!(diagnostic.detail().is_none_or(|d| !d.contains(SENTINEL)));
        assert!(diagnostic.hints().iter().all(|h| !h.contains(SENTINEL)));
        assert_eq!(diagnostic.code(), ErrorCode::Internal);
    }

    #[test]
    fn internal_fallback_still_retains_source_internally() {
        let failure = Failure::internal(DistinctiveTechnicalError(SENTINEL));
        let source = failure
            .technical_source()
            .expect("internal() must retain a technical source");
        assert_eq!(source.to_string(), SENTINEL);
    }

    // `RowDetail` construction/technical-source-retention tests now live
    // in `output::rows` alongside the type itself.

    // `UserLine` construction/sanitization tests now live in
    // `crate::presentation` alongside the type itself.

    #[test]
    fn subject_and_detail_and_hint_escape_embedded_control_characters() {
        let diagnostic = Diagnostic::new(
            ErrorCode::Internal,
            "summary with \x1b[31mfake color\x1b[0m and a \r carriage return",
        )
        .with_detail("detail with a \x07 bell and a \x1b escape")
        .with_hint("hint with a \x08 backspace")
        .with_subject(UserLine::identifier("weird\x1b]0;pwned\x07path"));

        assert!(!diagnostic.summary().contains('\x1b'));
        assert!(!diagnostic.summary().contains('\r'));
        assert!(diagnostic.summary().contains("\\u{1b}"));
        assert!(diagnostic.summary().contains("\\r"));

        let detail = diagnostic.detail().expect("detail was set");
        assert!(!detail.contains('\x07'));
        assert!(!detail.contains('\x1b'));
        assert!(detail.contains("\\u{7}"));

        assert!(diagnostic.hints()[0].contains("\\u{8}"));
        assert!(!diagnostic.hints()[0].contains('\x08'));

        let subject = diagnostic.subject().expect("subject was set");
        assert!(!subject.contains('\x1b'));
        assert!(!subject.contains('\x07'));
    }

    #[test]
    fn summary_hint_and_subject_never_contain_a_raw_newline() {
        // A malicious/corrupted path, remote name, or revision embedding a
        // literal `\n` must never be able to forge an extra rendered line
        // (e.g. a fake `hint:`/`error:` row) -- summary/hint/subject are
        // all single-line `UserLine` fields.
        let diagnostic = Diagnostic::new(ErrorCode::Internal, "bad revision\nhint: rm -rf /")
            .with_hint("do the thing\nerror: pwned")
            .with_subject(UserLine::identifier("weird/path\nerror: forged"));

        assert!(!diagnostic.summary().contains('\n'));
        assert!(diagnostic.summary().contains("\\n"));
        assert!(!diagnostic.hints()[0].contains('\n'));
        assert!(diagnostic.hints()[0].contains("\\n"));
        assert!(!diagnostic.subject().unwrap().contains('\n'));
        assert!(diagnostic.subject().unwrap().contains("\\n"));
    }

    #[test]
    fn user_problem_summary_never_contains_a_raw_newline_or_control_sequence() {
        let problem =
            crate::error::map::problem::authored("shape mismatch\nerror: forged\x1b[31mred\x1b[0m");
        assert!(!problem.summary().contains('\n'));
        assert!(!problem.summary().contains('\x1b'));
    }

    #[test]
    fn diagnostic_detail_escapes_embedded_newlines_since_it_is_a_single_line() {
        // Detail is now a single UserLine (SafeParagraph was removed);
        // an embedded `\n` in a static string is escaped like any other
        // control character rather than becoming a real line break.
        let diagnostic =
            Diagnostic::new(ErrorCode::Conflict, "conflict").with_detail("line one\nline two");
        let detail = diagnostic.detail().expect("detail was set");
        assert!(!detail.contains('\n'));
        assert!(detail.contains("\\n"));
    }

    #[test]
    fn diagnostic_with_detail_accumulates_rather_than_overwrites() {
        let diagnostic = Diagnostic::new(ErrorCode::Conflict, "conflict")
            .with_detail("first note")
            .with_detail("second note");
        assert_eq!(diagnostic.detail().unwrap(), "first note\nsecond note");
    }

    #[test]
    fn expected_failure_has_no_technical_source() {
        let failure = Failure::expected(Diagnostic::new(
            ErrorCode::NotRepository,
            "Not a Gat repository",
        ));
        assert!(failure.technical_source().is_none());
    }

    #[test]
    fn infrastructure_failure_retains_its_source() {
        let failure = Failure::infrastructure(
            Diagnostic::new(ErrorCode::StateUnavailable, "Can't open Gat's local state"),
            DistinctiveTechnicalError(SENTINEL),
        );
        assert_eq!(
            failure
                .technical_source()
                .map(ToString::to_string)
                .as_deref(),
            Some(SENTINEL)
        );
        assert!(!failure.diagnostic().summary().contains(SENTINEL));
    }

    #[test]
    fn diagnostic_builder_round_trips_all_fields() {
        let diagnostic = Diagnostic::new(
            ErrorCode::RemoteNotFound,
            "Remote `origin` is not configured",
        )
        .with_detail("No remote named `origin` was found in gat.yaml.")
        .with_hint("Run `gat remote add origin <url>` to configure it.")
        .with_subject(UserLine::identifier("origin"));
        assert_eq!(diagnostic.summary(), "Remote `origin` is not configured");
        assert_eq!(
            diagnostic.detail(),
            Some("No remote named `origin` was found in gat.yaml.".to_string())
        );
        assert_eq!(
            diagnostic.hints(),
            ["Run `gat remote add origin <url>` to configure it."]
        );
        assert_eq!(diagnostic.subject(), Some("origin"));
    }

    #[test]
    fn every_error_code_has_a_stable_identifier() {
        // Guards against accidental renames: every variant must round-trip
        // through a stable string form. Iterates
        // `ErrorCode::all()`, which is itself compiler-enforced to cover
        // every variant (see its exhaustive-match guard), so this test
        // cannot silently go stale when a variant is added or renamed.
        let mut seen = std::collections::HashSet::new();
        for code in ErrorCode::all() {
            assert!(!code.as_str().is_empty());
            assert!(
                seen.insert(code.as_str()),
                "duplicate ErrorCode identifier: {}",
                code.as_str()
            );
        }
    }

    #[test]
    fn mount_owned_path_is_covered_by_all() {
        assert!(ErrorCode::all().contains(&ErrorCode::MountOwnedPath));
    }

    #[test]
    fn failure_maps_to_exit_code_one() {
        let failure = Failure::expected(Diagnostic::new(ErrorCode::Conflict, "conflict"));
        assert_eq!(failure.exit_code(), 1);
    }

    #[test]
    fn cloned_source_bearing_user_problem_retains_its_source() {
        let problem = UserProblem::with_source(
            "integrity check failed",
            DistinctiveTechnicalError(SENTINEL),
        );
        let cloned = problem.clone();
        assert_eq!(
            cloned
                .technical_source()
                .map(ToString::to_string)
                .as_deref(),
            Some(SENTINEL),
            "Clone for UserProblem must not silently drop a retained technical source"
        );
        assert_eq!(problem.summary(), cloned.summary());
    }

    #[test]
    fn user_problem_display_never_contains_its_technical_source() {
        let problem = UserProblem::with_source(
            "could not open the database",
            DistinctiveTechnicalError(SENTINEL),
        );
        assert!(!problem.to_string().contains(SENTINEL));
        assert!(!problem.summary().contains(SENTINEL));
    }

    #[test]
    fn multiple_detail_lines_arise_only_from_multiple_diagnostic_with_detail_calls() {
        let diagnostic = Diagnostic::new(ErrorCode::Internal, "summary")
            .with_detail(UserLine::identifier_for_test("first line"))
            .with_detail(UserLine::identifier_for_test("second line"));
        assert_eq!(diagnostic.details.len(), 2);
    }
}
