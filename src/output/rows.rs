//! Presentation row/message models: `Message`/`MessageKind`/`ListRow`/`ListStatus` are
//! how `output::render` composes a command's typed outcome into
//! terminal-shaped rows. A command outcome must never carry rendered text
//! or presentation-shaped rows
//! itself -- only `output` constructs these, from already-typed domain
//! facts.

use crate::error::UserProblem;
use crate::presentation::UserLine;
use std::error::Error as StdError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    Action,
    Success,
    Caution,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    kind: MessageKind,
    text: UserLine,
}

impl Message {
    /// The general dynamic-composition constructor: `text` must already be
    /// an approved [`UserLine`] (static authored prose, or explicit
    /// fragment composition) -- never a bare `String`/`impl Display`, so
    /// `Message::new(kind, err.to_string())` is a compile error.
    pub(crate) const fn new(kind: MessageKind, text: UserLine) -> Self {
        Self { kind, text }
    }

    /// Ergonomic static-text shortcut for the overwhelmingly common case
    /// of a `&'static str` literal, equivalent to
    /// `Message::new(kind, UserLine::authored(text))`.
    pub(crate) fn authored(kind: MessageKind, text: &'static str) -> Self {
        Self::new(kind, UserLine::authored(text))
    }

    pub(crate) const fn kind(&self) -> MessageKind {
        self.kind
    }

    pub(crate) const fn line(&self) -> &UserLine {
        &self.text
    }
}

/// Semantic status for one row in a rendered list: each state
/// maps to one short symbol and one semantic color, so meaning survives
/// with color stripped -- the symbols alone must remain unambiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Keep the full shared status vocabulary available.
pub enum ListStatus {
    /// → cyan: about to happen / in progress.
    Pending,
    /// ✓ green: a healthy/complete state.
    Success,
    /// ! yellow: needs attention but is not a hard error.
    Warning,
    /// ✗ red: a hard error or invalid state.
    Error,
    /// A green: a newly added item.
    Added,
    /// M yellow: a changed item.
    Modified,
    /// D red: a removed item.
    Deleted,
    /// R cyan: a renamed item.
    Renamed,
    /// C cyan: a copied item.
    Copied,
    /// ? yellow: not tracked/known.
    Untracked,
    /// i dim: excluded/ignored on purpose.
    Ignored,
    /// ! red: needs attention before proceeding.
    Conflict,
    /// . dim: intentionally not processed.
    Skipped,
}

/// One rendered row/line's detail text: either plain authored/composed
/// text, or a non-fatal [`UserProblem`] -- a finding that may carry a
/// private technical source. Constructing a `RowDetail` from a
/// `UserProblem` (via `From`/[`Self::composed`]) keeps the `UserProblem`
/// itself intact instead of immediately flattening it into a `String`
/// via `Display`, so its technical source survives into the final output
/// model (`SystemOutcome`, `ListRow`, ...) for tests -- rendering itself
/// always sees only the safe, pre-rendered
/// summary text, exactly as a plain `UserLine` detail would. It lives in
/// `output` because it is
/// purely an output-row concern, not part of the fatal-error contract
/// `error` owns.
#[derive(Clone, Debug)]
pub struct RowDetail {
    rendered: UserLine,
    /// Kept only for its private technical source in tests; rendering always
    /// uses `rendered` instead, so this
    /// field is never read for its `Display`/`summary()` text after
    /// construction. It has the same internal-inspection role as
    /// [`UserProblem::technical_source`].
    #[cfg_attr(not(test), allow(dead_code))]
    problem: Option<UserProblem>,
}

impl RowDetail {
    /// The safe, single-line text to render. Only exercised by this
    /// module's own tests (`line()` is the production renderer's path) and
    /// is kept for test assertions.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn as_str(&self) -> &str {
        self.rendered.as_str()
    }

    /// The approved [`UserLine`] itself, for renderers that must carry
    /// the type guarantee all the way to the terminal instead of
    /// unwrapping to `&str` early.
    pub(crate) const fn line(&self) -> &UserLine {
        &self.rendered
    }

    /// Approved static row metadata: a `&'static str` literal compiled
    /// into the binary, never a runtime `String`/`Display` result.
    pub(crate) fn authored(text: &'static str) -> Self {
        Self {
            rendered: UserLine::authored(text),
            problem: None,
        }
    }

    /// Approved dynamic row metadata composed through
    /// [`UserLine::compose`]'s closed fragment API.
    pub(crate) const fn message(message: UserLine) -> Self {
        Self {
            rendered: message,
            problem: None,
        }
    }

    /// The retained technical source, if this detail was built from a
    /// source-bearing `UserProblem` -- `pub(crate)` only, for tests, mirroring
    /// [`UserProblem::technical_source`]'s own visibility and never used
    /// to build user-facing text.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn technical_source(&self) -> Option<&(dyn StdError + Send + Sync + 'static)> {
        self.problem
            .as_ref()
            .and_then(UserProblem::technical_source)
    }

    /// Composes a `prefix`/`suffix` around `problem`'s summary (e.g.
    /// `RowDetail::composed("disabled (", problem, ")")` renders as
    /// `"disabled (<problem summary>)"`) without ever discarding
    /// `problem`'s own retained technical source. `prefix` takes
    /// `impl Into<UserLine>`, so a static authored literal and an
    /// already-composed dynamic [`UserLine`] both work naturally.
    /// `suffix` stays a `&'static str` literal since every call site's
    /// suffix is static authored punctuation, not a value that ever
    /// needs its own dynamic composition.
    pub(crate) fn composed(
        prefix: impl Into<UserLine>,
        problem: UserProblem,
        suffix: &'static str,
    ) -> Self {
        let prefix: UserLine = prefix.into();
        Self {
            rendered: UserLine::compose([
                prefix,
                UserLine::identifier(problem.summary()),
                UserLine::authored(suffix),
            ]),
            problem: Some(problem),
        }
    }
}

impl PartialEq for RowDetail {
    fn eq(&self, other: &Self) -> bool {
        self.rendered == other.rendered
    }
}

impl Eq for RowDetail {}

impl From<UserProblem> for RowDetail {
    fn from(problem: UserProblem) -> Self {
        Self {
            rendered: UserLine::identifier(problem.summary()),
            problem: Some(problem),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListRow {
    status: ListStatus,
    path: UserLine,
    metadata: Option<RowDetail>,
}

impl ListRow {
    /// `path` is domain identity (a repo-relative path), not authored
    /// prose -- it takes the exact [`UserLine`] identifier type rather
    /// than a generic `impl Into<UserLine>`, so a caller must explicitly
    /// sanitize a dynamic value (e.g. `UserLine::identifier(path)`)
    /// instead of relying on an implicit blanket conversion.
    pub(crate) const fn new(status: ListStatus, path: UserLine) -> Self {
        Self {
            status,
            path,
            metadata: None,
        }
    }

    /// `metadata` is the row's optional detail text, taking the exact
    /// [`RowDetail`] type rather than a generic
    /// `impl Into<RowDetail>` bound.
    pub(crate) const fn with_metadata(
        status: ListStatus,
        path: UserLine,
        metadata: RowDetail,
    ) -> Self {
        Self {
            status,
            path,
            metadata: Some(metadata),
        }
    }

    pub(crate) const fn status(&self) -> ListStatus {
        self.status
    }

    pub(crate) const fn path(&self) -> &UserLine {
        &self.path
    }

    pub(crate) fn metadata(&self) -> Option<&UserLine> {
        self.metadata.as_ref().map(RowDetail::line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the narrowed shape of `Message`/`ListRow`'s presentation
    /// constructors: `Message::authored` accepts a
    /// bare `&'static str` literal, `Message::new` requires an already
    /// composed `UserLine`, and `ListRow::new`/`with_metadata` require
    /// the exact `UserLine`/`RowDetail` identifier types (reachable only
    /// through an explicit constructor call, never implicitly through a
    /// generic `impl Into<..>` bound on the constructor itself).
    #[test]
    fn message_and_list_row_constructors_require_approved_presentation_types() {
        let _ = Message::authored(MessageKind::Action, "static text");
        let _ = Message::new(MessageKind::Action, UserLine::authored("static text"));
        let _ = ListRow::new(ListStatus::Success, UserLine::identifier("path"));
        let _ = ListRow::with_metadata(
            ListStatus::Success,
            UserLine::identifier("path"),
            RowDetail::authored("detail"),
        );
    }

    #[derive(Debug)]
    struct DistinctiveTechnicalError(&'static str);

    impl std::fmt::Display for DistinctiveTechnicalError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl StdError for DistinctiveTechnicalError {}

    const SENTINEL: &str = "sqlite: disk I/O error at offset 0x4f2c (os error 13)";

    #[test]
    fn row_detail_from_a_source_bearing_user_problem_retains_the_source_after_cloning() {
        let problem = crate::error::map::problem::with_source_for_test(
            "disk unreadable",
            DistinctiveTechnicalError(SENTINEL),
        );
        let detail = RowDetail::composed("desired (", problem, ")");
        // Move/clone the detail the way it travels through a row/outcome.
        let moved = detail;
        let source = moved
            .technical_source()
            .expect("RowDetail::composed must retain the UserProblem's technical source");
        assert_eq!(source.to_string(), SENTINEL);
    }

    #[test]
    fn row_detail_rendering_never_exposes_the_retained_technical_source() {
        let problem = crate::error::map::problem::with_source_for_test(
            "disk unreadable",
            DistinctiveTechnicalError(SENTINEL),
        );
        let detail = RowDetail::composed("desired (", problem, ")");
        assert!(!detail.as_str().contains(SENTINEL));
        assert_eq!(detail.as_str(), "desired (disk unreadable)");
    }

    #[test]
    fn row_detail_composed_with_a_dynamic_prefix_retains_the_source_and_sanitizes_the_prefix() {
        let problem = crate::error::map::problem::with_source_for_test(
            "object corrupt",
            DistinctiveTechnicalError(SENTINEL),
        );
        let prefix = UserLine::compose([
            UserLine::authored("repair of object "),
            UserLine::identifier("abc123 failed: \x1b[31minjected\x1b[0m: "),
        ]);
        let detail = RowDetail::composed(prefix, problem, "");
        assert!(!detail.as_str().contains('\x1b'));
        assert!(!detail.as_str().contains(SENTINEL));
        let source = detail
            .technical_source()
            .expect("RowDetail::composed must retain the UserProblem's technical source");
        assert_eq!(source.to_string(), SENTINEL);
    }

    #[test]
    fn row_detail_from_plain_text_has_no_technical_source() {
        let detail = RowDetail::authored("already valid");
        assert!(detail.technical_source().is_none());
        assert_eq!(detail.as_str(), "already valid");
    }
}
