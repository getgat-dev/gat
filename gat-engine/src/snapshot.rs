//! Immutable repository snapshot.
//!
//! [`Snapshot`] owns the effective-config snapshot for one
//! operation plus the values derived once from it: the resolved object-
//! cache capability, the resolved link/materialization policy, and the
//! compiled [`EffectivePathPolicy`]. Construction is side-effect free (it
//! performs no I/O beyond the already-loaded `Config` it is handed) and
//! every accessor is read-only -- this type never mutates and is never
//! mutated after construction, so it can be shared freely across an
//! operation's phases without any synchronization.
//!
//! This holds only state that is fully determined by one
//! coherent repository generation's effective config, not any
//! mutable/lazy execution service (cache access, remote operators,
//! the remote executor, ...) -- those live on `Session` instead.

use super::path_policy::EffectivePathPolicy;
use crate::remote_catalog::RemoteCatalog;
use crate::repository_state::DesiredRevision;
use gat_core::config::{Config, MaterializationStrategy};

/// Everything snapshot construction can fail with: compiling the
/// [`RemoteCatalog`] out of effective config, or compiling the
/// [`EffectivePathPolicy`] out of that config and catalog.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    RemoteCatalog(#[from] crate::remote_catalog::RemoteCatalogError),
    #[error(transparent)]
    PathPolicy(#[from] crate::path_policy::PathPolicyError),
}

type Result<T> = std::result::Result<T, SnapshotError>;

pub(crate) struct SnapshotInput {
    config: Config,
    cache_root: gat_io::CacheRoot,
    materialization_strategy: MaterializationStrategy,
    desired_revision: DesiredRevision,
}

impl SnapshotInput {
    pub(crate) const fn new(
        config: Config,
        cache_root: gat_io::CacheRoot,
        materialization_strategy: MaterializationStrategy,
        desired_revision: DesiredRevision,
    ) -> Self {
        Self {
            config,
            cache_root,
            materialization_strategy,
            desired_revision,
        }
    }
}

/// One coherent, immutable snapshot of repository-level state for an
/// operation: the effective config, the resolved object-cache capability,
/// the resolved link/materialization mode, the compiled path policy, the
/// compiled [`RemoteCatalog`], and the
/// [`DesiredRevision`] identity that desired state
/// represented at the moment this snapshot was captured.
pub(crate) struct Snapshot {
    config: Config,
    cache_root: gat_io::CacheRoot,
    materialization_strategy: MaterializationStrategy,
    policy: EffectivePathPolicy,
    remotes: RemoteCatalog,
    desired_revision: DesiredRevision,
}

impl Snapshot {
    /// Builds a snapshot from repository-resolved state for one coherent
    /// generation. Side-effect free: compiles the remote catalog and path
    /// policy with no repository access, filesystem writes, remote
    /// initialization, or other observable side effect. The remote catalog
    /// is compiled before the path policy so an
    /// invalid `remotes.default` (naming a remote absent from
    /// `remotes.by_name`) fails snapshot construction itself, rather than
    /// silently producing an `EffectivePathPolicy` with no compiled
    /// default route.
    pub(crate) fn new(input: SnapshotInput) -> Result<Self> {
        let SnapshotInput {
            config,
            cache_root,
            materialization_strategy,
            desired_revision,
        } = input;
        let remotes = RemoteCatalog::from_config(&config.remotes)?;
        let policy = EffectivePathPolicy::from_config(&config, &remotes)?;
        Ok(Self {
            config,
            cache_root,
            materialization_strategy,
            policy,
            remotes,
            desired_revision,
        })
    }

    pub(crate) const fn config(&self) -> &Config {
        &self.config
    }

    /// Physical cache location retained for engine-owned services.
    ///
    /// Command orchestration must use semantic operation services instead
    /// of receiving this host path.
    pub(crate) const fn cache_root(&self) -> &gat_io::CacheRoot {
        &self.cache_root
    }

    pub(crate) const fn materialization_strategy(&self) -> &MaterializationStrategy {
        &self.materialization_strategy
    }

    pub(crate) const fn policy(&self) -> &EffectivePathPolicy {
        &self.policy
    }

    /// The operation's compiled [`RemoteCatalog`]: the sole remote
    /// config/name/id authority, used by
    /// [`super::remote_session::RemoteSession::open`]/`open_handle` to
    /// derive both the diagnostic name and endpoint URL for an opened
    /// [`super::remote_session::RemoteHandle`] from just a
    /// [`crate::remote_catalog::RemoteId`] -- `RemoteCatalog` itself stores
    /// each remote's URL alongside
    /// its name/id (see `RemoteCatalog::url`), so nothing downstream
    /// needs its own separate clone of
    /// [`gat_core::config::RemotesConfig`] to build operators.
    pub(crate) const fn remotes_catalog(&self) -> &RemoteCatalog {
        &self.remotes
    }

    /// The [`DesiredRevision`] this snapshot's desired state represented
    /// when captured -- what a later
    /// [`crate::repository::Repository::revalidate_desired_revision`]
    /// call checks a composite operation's mutating phase against.
    pub(crate) const fn desired_revision(&self) -> &DesiredRevision {
        &self.desired_revision
    }

    pub(crate) const fn replace_desired_revision(&mut self, desired_revision: DesiredRevision) {
        self.desired_revision = desired_revision;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Purely mechanical: constructing a snapshot from a loaded config must
    /// not perform resource initialization beyond resolving the values
    /// retained by the snapshot.
    #[test]
    fn construction_resolves_cache_location_and_materialization_strategy_exactly_once() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let config = repo.load_config().unwrap();

        let before_loc = crate::test_support::cache_location_resolutions();
        let before_strategy = crate::test_support::materialization_strategy_resolutions();
        let desired_revision = crate::repository_state::current_desired_revision(&repo).unwrap();
        let input = repo.snapshot_input(config, desired_revision);
        let after_input_loc = crate::test_support::cache_location_resolutions();
        let after_input_strategy = crate::test_support::materialization_strategy_resolutions();
        let snapshot = Snapshot::new(input).unwrap();
        let after_new_loc = crate::test_support::cache_location_resolutions();
        let after_new_strategy = crate::test_support::materialization_strategy_resolutions();

        assert_eq!(after_input_loc - before_loc, 1);
        assert_eq!(after_input_strategy - before_strategy, 1);
        assert_eq!(after_new_loc - after_input_loc, 0);
        assert_eq!(after_new_strategy - after_input_strategy, 0);
        // Read-only accessors work and reflect the given config.
        assert_eq!(snapshot.config().mounts.by_name.len(), 0);
        let _ = snapshot.cache_root();
        let _ = snapshot.materialization_strategy();
        let _ = snapshot.policy();
    }
}
