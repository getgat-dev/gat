//! Domain/engine-facing Git history *selection* -- the user-intent value
//! (roots + traversal + time window + parent policy + exclusions)
//! independent of `gix`. `gat-io`'s private Git implementation resolves a
//! [`HistorySelection`] against an open repository; nothing in this
//! module knows about Gix object IDs, repositories, ref traversal, or
//! commit peeling.
//!
//! Keeping this request type Gix-independent in `gat-core` lets engine and
//! command services accept a [`HistorySelection`] without depending on
//! `gix`; only the Git I/O owner resolves it.

use super::git::{GitRevisionSpec, GitTimestamp};
use std::num::NonZeroUsize;

/// One user-facing "starting point" for history selection.
///
/// `Branches` is deliberately unified: it resolves both local
/// (`refs/heads/**`) and remote-tracking (`refs/remotes/**`) branch tips,
/// deduplicated by resolved commit id, because remote-tracking refs are
/// still local (possibly stale) Git refs rather than a distinct "remote"
/// concept -- a specific remote-tracking ref remains selectable via
/// `Revision("origin/main".into())`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryRoot {
    /// The current `HEAD` commit.
    Head,
    /// An explicit revision/rev-spec, resolved strictly (`gix::Repository::rev_parse_single`).
    Revision(GitRevisionSpec),
    /// Every local branch tip (`refs/heads/**`) and remote-tracking branch
    /// tip (`refs/remotes/**`), deduplicated by resolved commit id. A
    /// branch ref that *validly* resolves to a non-commit object (e.g. one
    /// pointing at a blob or tree) is skipped, not an error; but a branch
    /// ref that *fails* to resolve/peel at all -- a dangling ref, a
    /// corrupt loose-ref file, an I/O error listing `refs/heads/**` --
    /// makes resolution fail closed inside the Git I/O implementation.
    Branches,
    /// Every tag (`refs/tags/**`), peeled to the commit it points at. A
    /// tag (lightweight or annotated) that *validly* resolves to a
    /// non-commit object -- e.g. one pointing at a blob or tree -- is
    /// skipped, not an error; but a tag ref that *fails* to resolve/peel
    /// at all makes resolution fail closed inside the Git I/O
    /// implementation.
    Tags,
    /// Every commit-bearing ref of any kind: branches, tags, and anything
    /// else (e.g. notes/CI refs). Exposed as `--all-history`; also used
    /// internally to build `gc`'s conservative no-selection default, but
    /// that reuse is incidental -- `--all-history` goes through the same
    /// general roots+traversal pipeline as every other scope, it does
    /// not borrow `gc`-only safety semantics. A ref that *validly*
    /// resolves to a non-commit object is skipped, not an error --
    /// unlike an explicit `--rev`, which stays strict on that point --
    /// but a ref that *fails* to resolve/peel at all (dangling, corrupt,
    /// unreadable) makes resolution fail closed inside the Git I/O
    /// implementation, which is what keeps `gc`'s
    /// conservative default from ever computing an incomplete keep-set
    /// silently.
    AllRefs,
}

/// How far to walk from each selected root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryTraversal {
    /// Only the root commits themselves, no ancestry walk. This is the
    /// default traversal for an explicit scope selector (`--rev`,
    /// `--branches`, `--tags`) given on its own: selecting a revision
    /// does not inherently mean selecting all of its ancestors too.
    /// `app::resolve_history_selection` switches to `Ancestors` only when
    /// the user asks for ancestry, explicitly (`--ancestors`, `--depth`,
    /// `--all-history`) or because a flag logically requires walking
    /// history rather than just inspecting roots (`--since`/`--until`,
    /// `--first-parent`, `--exclude-rev`).
    Tips,
    /// Walk ancestors of each root independently. `per_root`, when set,
    /// caps how many commits are *traversed* per root (graph-ancestry
    /// order, closest ancestors first -- not commit-timestamp order),
    /// evaluated *before* any time-window filter (`TimeWindow`): a commit
    /// outside `--since`/`--until` still consumes one unit of traversal
    /// depth. Per-root results are unioned/deduplicated only after both
    /// the depth cap and the time filter have been applied.
    Ancestors { per_root: Option<NonZeroUsize> },
}

impl Default for HistoryTraversal {
    fn default() -> Self {
        Self::Ancestors { per_root: None }
    }
}

/// Inclusive commit-time (committer time, matching `git log --since/--until`)
/// filter. `None` on either bound means unbounded on that side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimeWindow {
    pub since: Option<GitTimestamp>,
    pub until: Option<GitTimestamp>,
}

impl TimeWindow {
    #[must_use]
    pub const fn is_unbounded(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }

    #[must_use]
    pub fn contains(&self, seconds: GitTimestamp) -> bool {
        self.since.is_none_or(|since| seconds >= since)
            && self.until.is_none_or(|until| seconds <= until)
    }
}

/// Which parent edges to follow while walking ancestors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParentMode {
    /// Follow every parent (the default; matches plain `git log`).
    #[default]
    All,
    /// Follow only the first parent of each commit.
    First,
}

/// User intent: which Git commits has the user selected? Gix-independent;
/// `gat-io` resolves this selection against an open repository.
#[derive(Clone, Debug, Default)]
pub struct HistorySelection {
    pub roots: Vec<HistoryRoot>,
    pub traversal: HistoryTraversal,
    pub time: TimeWindow,
    pub parents: ParentMode,
    /// Revisions/rev-specs whose ancestry is excluded, resolved strictly
    /// like `roots`. Mirrors `git log ^rev`/hidden-tip support.
    pub excluded: Vec<GitRevisionSpec>,
}

impl HistorySelection {
    /// The conservative, `gat.lock`-agnostic default used by plain `gat
    /// gc` (no history flags at all) for every explicitly selected repository:
    /// every ref, unbounded
    /// ancestry, every parent, no exclusions. This is a *GC safety
    /// default*, not the meaning of an explicit `--rev`/`--branches`/
    /// `--tags` -- an explicit scope selector defaults to
    /// [`HistoryTraversal::Tips`] instead (see
    /// `crate::app::resolve_history_selection`).
    #[must_use]
    pub fn conservative_default() -> Self {
        Self {
            roots: vec![HistoryRoot::AllRefs],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            time: TimeWindow::default(),
            parents: ParentMode::All,
            excluded: Vec::new(),
        }
    }
}

/// Command-level history intent, resolved before entering Git history traversal.
#[derive(Clone, Debug, Default)]
pub enum HistoryRequest {
    /// Use the command's retention or current-state default.
    #[default]
    CommandDefault,
    /// Do not consult this repository's committed snapshots.
    Disabled,
    /// Consult the explicitly selected Git history.
    Selected(HistorySelection),
}
