//! Single mutation gate.
//!
//! [`MutationGuard`] is the one proof that a worktree/materialized-state
//! mutation is allowed to proceed: it can only be constructed by
//! [`super::operation::Operation::mutate`], which asks the engine
//! repository service to acquire mutation access and revalidate the
//! operation's captured
//! [`crate::repository_state::DesiredRevision`] against the repository's
//! current canonical desired state *before* returning one. A caller cannot
//! assemble a `MutationGuard` from an unrelated repo/snapshot/session
//! combination, nor manufacture one without actually revalidating under
//! lock first -- both fields are private and there is no public
//! constructor other than `Operation::mutate`.
//!
//! Deliberately borrows the exact `Operation` whose revision was validated
//! (`&'op mut Operation<'repo>`) rather than re-splitting it into a second,
//! independently-parameterized set of repo/snapshot/session fields: the
//! borrow checker then guarantees a `MutationGuard` can only ever refer to
//! the one operation it was produced from. The two separate lifetimes
//! (`'op` for this mutable borrow, `'repo` for the operation's underlying
//! `&Repo`) matter because they have very different extents --
//! `'repo: 'op` almost always holds with room to spare -- and collapsing
//! them into one lifetime parameter would force every borrow of the guard
//! to also (unnecessarily) restrict how long the operation's `&Repo` can be
//! considered borrowed, which produces exactly the kind of overly-strict
//! borrow-checker errors a caller has no clean way to work around later.
//! ```text
//! 'repo  ──────────────────────────────>
//! 'op                ───────>
//! ```
//!
//! # Consistency contract
//!
//! - Config is snapshot-isolated per operation and plays no role in this
//!   check: a concurrent config edit never causes `Operation::mutate` to
//!   reject.
//! - Desired revision is revalidated under `RepoLock`, after any unlocked
//!   selection/remote-I/O phase, before worktree/materialized mutation.
//! - A desired-state change landing after that unlocked phase and before
//!   this gate is reached is rejected deterministically, before any
//!   worktree/materialized mutation occurs.
//! - Canonical desired-state writes (`gat add`/`rm`/`mv`, mount publish) and
//!   any operation-owned materialized-mirror reshape both serialize through
//!   the same `RepoLock` this gate uses, so nothing can race a
//!   `MutationGuard`'s already-revalidated view of desired state.

use super::operation::Operation;
use crate::repository::RepositoryMutationAccess;

/// A short-lived proof of exclusive, revision-validated mutation access to
/// one [`Operation`]'s repository. Held only for the duration of the
/// mutating reconciliation call that requested it (`sync_from_snapshot`'s
/// mutating branch) -- never stashed somewhere longer-lived, since that
/// would hold repository mutation authority across unrelated work.
pub(crate) struct MutationGuard<'op, 'repo> {
    operation: &'op mut Operation<'repo>,
    _access: RepositoryMutationAccess,
}

impl<'op, 'repo> MutationGuard<'op, 'repo> {
    /// Constructible only from within `runtime` (`Operation::mutate` is the
    /// sole production caller): deliberately
    /// `pub(super)`, not `pub(crate)` -- visible to `runtime` and its
    /// descendant modules only, so `commands`/`worktree` code cannot import
    /// this function and manually assemble a guard from an unrelated
    /// repo/snapshot/session combination, bypassing `Operation::mutate`'s
    /// acquire-then-revalidate contract. Only `Operation::mutate`
    /// actually calls it today.
    ///
    /// Structural guarantee: ordinary command code
    /// cannot construct a `MutationGuard` -- there is no `pub`/`pub(crate)`
    /// path to this constructor from `commands` or `worktree`, so any
    /// attempt to call it from those modules is a compile error, not a
    /// runtime check.
    pub(super) const fn new(
        operation: &'op mut Operation<'repo>,
        access: RepositoryMutationAccess,
    ) -> Self {
        Self {
            operation,
            _access: access,
        }
    }

    /// Read-only access to the validated operation.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn operation(&self) -> &Operation<'repo> {
        self.operation
    }

    /// Mutable access to the validated operation -- needed by reconciliation
    /// to reach `Operation::config`/`Operation::split_for_sync`/
    /// `Operation::window_services`, all
    /// of which take `&mut self` or `&self` through the same already-
    /// validated operation.
    pub(crate) const fn operation_mut(&mut self) -> &mut Operation<'repo> {
        self.operation
    }

    /// Compares `actual` -- the [`gat_core::lock::CanonicalDesiredIdentity`]
    /// the desired-index refresh just assembled
    /// while materializing its own view of desired state into the
    /// derived mirror -- against this guard's already-lock-revalidated
    /// [`crate::repository_state::DesiredRevision`], returning
    /// [`crate::repository_state::StaleDesiredRevisionError`] on mismatch
    /// on mismatch.
    ///
    /// `Operation::mutate` only proves that `gat.lock` itself matched the
    /// operation's captured revision *at the moment the lock was
    /// acquired*; it says nothing about what `refresh()` -- called only
    /// afterward, while still holding this same guard's lock -- actually
    /// materialized. In principle those two could still diverge (e.g. a
    /// concurrent process bypassing `RepoLock` entirely, or a bug in
    /// `refresh`'s own shard-catalog assembly), and reconciliation must
    /// never proceed to mutate the worktree/materialized mirror against a
    /// desired-state view it never actually validated. Calling this right
    /// after the single `refresh()` call in `execute_mutating_sync`
    /// closes that gap: reconciliation cannot
    /// begin mutating anything until the exact identity it is about to
    /// reconcile toward has been proven to match the one already
    /// revalidated under lock.
    pub(crate) fn require_desired_identity(
        &self,
        actual: gat_core::lock::CanonicalDesiredIdentity,
    ) -> Result<(), crate::repository_state::StaleDesiredRevisionError> {
        let expected = self.operation.desired_revision().identity();
        if actual == expected {
            Ok(())
        } else {
            Err(crate::repository_state::StaleDesiredRevisionError)
        }
    }
}
