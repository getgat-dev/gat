//! Repository initialization capabilities for `gat init`.

use std::path::{Path, PathBuf};

use gat_core::managed_block;

use crate::repository::Repository;

const BEGIN: &str = "# >>> gat >>>";
const END: &str = "# <<< gat <<<";

/// A Git hook managed by Gat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedHook {
    PostCheckout,
    PostMerge,
    PostRewrite,
}

impl ManagedHook {
    pub const ALL: [Self; 3] = [Self::PostCheckout, Self::PostMerge, Self::PostRewrite];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PostCheckout => "post-checkout",
            Self::PostMerge => "post-merge",
            Self::PostRewrite => "post-rewrite",
        }
    }
}

/// Whether an idempotent initialization step changed its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrationStatus {
    Changed,
    Unchanged,
}

/// Hooks changed by an install or uninstall operation.
#[derive(Debug, PartialEq, Eq)]
pub struct HookChanges {
    pub changed: Vec<ManagedHook>,
}

/// A resolved cache location retained as a typed engine result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCacheLocation(PathBuf);

impl ResolvedCacheLocation {
    pub(crate) const fn new(path: PathBuf) -> Self {
        Self(path)
    }

    /// The resolved host path for user-facing command output.
    ///
    /// Operational cache access remains inside the engine and I/O layers;
    /// this accessor exists only because `gat init` and `gat config get
    /// cache.location` deliberately display the resolved location.
    #[must_use]
    pub fn display_path(&self) -> &Path {
        &self.0
    }
}

/// Semantic initialization failure with its required diagnostic context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitializationErrorKind<'a> {
    OpenRepository { path: &'a Path },
    Read,
    NonUtf8Hook { path: &'a Path },
    Write,
    GitConfig,
    GitConfigLocked,
    ConfigScaffold,
}

/// Failure to converge repository initialization state. Its semantic view is
/// derived from the retained typed failure, so kind, context, and cause cannot
/// diverge.
#[derive(Debug)]
pub struct InitializationError(InitializationFailure);

#[derive(Debug)]
enum InitializationFailure {
    Git(gat_io::GitIntegrationError),
    ConfigScaffold(gat_io::ConfigWriteError),
}

impl InitializationError {
    #[must_use]
    pub fn kind(&self) -> InitializationErrorKind<'_> {
        use gat_io::GitIntegrationErrorKind;
        match &self.0 {
            InitializationFailure::Git(source) => match source.kind() {
                GitIntegrationErrorKind::OpenRepository => {
                    InitializationErrorKind::OpenRepository {
                        path: source.path(),
                    }
                }
                GitIntegrationErrorKind::Read => InitializationErrorKind::Read,
                GitIntegrationErrorKind::NonUtf8 => InitializationErrorKind::NonUtf8Hook {
                    path: source.path(),
                },
                GitIntegrationErrorKind::Write => InitializationErrorKind::Write,
                GitIntegrationErrorKind::Config => InitializationErrorKind::GitConfig,
                GitIntegrationErrorKind::ConfigLocked => InitializationErrorKind::GitConfigLocked,
            },
            InitializationFailure::ConfigScaffold(_) => InitializationErrorKind::ConfigScaffold,
        }
    }

    #[must_use]
    pub fn filesystem_failure(&self) -> Option<crate::FilesystemFailureKind> {
        let kind = match &self.0 {
            InitializationFailure::Git(source) => source.io_kind(),
            InitializationFailure::ConfigScaffold(source) => match source {
                gat_io::ConfigWriteError::Write(source) => source.io_kind(),
                gat_io::ConfigWriteError::Serialize { .. } => None,
            },
        };
        kind.map(crate::repository_access::classify_io_kind)
    }
}

impl From<gat_io::GitIntegrationError> for InitializationError {
    fn from(source: gat_io::GitIntegrationError) -> Self {
        Self(InitializationFailure::Git(source))
    }
}

impl From<gat_io::ConfigWriteError> for InitializationError {
    fn from(source: gat_io::ConfigWriteError) -> Self {
        Self(InitializationFailure::ConfigScaffold(source))
    }
}

impl std::fmt::Display for InitializationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            InitializationFailure::Git(source) => std::fmt::Display::fmt(source, f),
            InitializationFailure::ConfigScaffold(_) => {
                f.write_str("creating the project gat.yaml failed")
            }
        }
    }
}

impl std::error::Error for InitializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            InitializationFailure::Git(source)
                if source.kind() == gat_io::GitIntegrationErrorKind::NonUtf8 =>
            {
                None
            }
            InitializationFailure::Git(source) => Some(source),
            InitializationFailure::ConfigScaffold(source) => Some(source),
        }
    }
}

/// One invocation-scoped handle for repository Git integration and init I/O.
pub struct InitializationService<'a> {
    repo: &'a Repository,
    git: gat_io::GitIntegration,
}

impl Repository {
    pub fn initialization(&self) -> Result<InitializationService<'_>, InitializationError> {
        let git = gat_io::GitIntegration::open(self.layout()).map_err(InitializationError::from)?;
        Ok(InitializationService { repo: self, git })
    }
}

impl InitializationService<'_> {
    pub fn install_merge_driver(&self) -> Result<IntegrationStatus, InitializationError> {
        self.git
            .install_merge_driver()
            .map(map_status)
            .map_err(InitializationError::from)
    }

    pub fn uninstall_merge_driver(&self) -> Result<IntegrationStatus, InitializationError> {
        self.git
            .uninstall_merge_driver()
            .map(map_status)
            .map_err(InitializationError::from)
    }

    pub fn install_merge_attributes(&self) -> Result<IntegrationStatus, InitializationError> {
        self.git
            .install_merge_attributes()
            .map(map_status)
            .map_err(InitializationError::from)
    }

    pub fn uninstall_merge_attributes(&self) -> Result<IntegrationStatus, InitializationError> {
        self.git
            .uninstall_merge_attributes()
            .map(map_status)
            .map_err(InitializationError::from)
    }

    pub fn install_hooks(&self) -> Result<HookChanges, InitializationError> {
        let mut changed = Vec::new();
        for hook in ManagedHook::ALL {
            let existing = self
                .git
                .read_hook(hook.as_str())
                .map_err(InitializationError::from)?
                .unwrap_or_default();
            let body = format!("gat hook {} \"$@\" || exit $?\n", hook.as_str());
            let updated = managed_block::upsert(&existing, BEGIN, END, &body);
            let updated = if existing.is_empty() {
                format!("#!/bin/sh\n{updated}")
            } else {
                updated
            };
            if updated == existing {
                continue;
            }
            self.git
                .write_hook(hook.as_str(), &updated)
                .map_err(InitializationError::from)?;
            changed.push(hook);
        }
        Ok(HookChanges { changed })
    }

    pub fn uninstall_hooks(&self) -> Result<HookChanges, InitializationError> {
        let mut changed = Vec::new();
        for hook in ManagedHook::ALL {
            let Some(existing) = self
                .git
                .read_hook(hook.as_str())
                .map_err(InitializationError::from)?
            else {
                continue;
            };
            let Some(updated) = managed_block::remove(&existing, BEGIN, END) else {
                continue;
            };
            let rest_is_empty = updated
                .lines()
                .all(|line| line.trim().is_empty() || line.trim() == "#!/bin/sh");
            if rest_is_empty {
                self.git
                    .remove_hook(hook.as_str())
                    .map_err(InitializationError::from)?;
            } else {
                self.git
                    .write_hook(hook.as_str(), &updated)
                    .map_err(InitializationError::from)?;
            }
            changed.push(hook);
        }
        Ok(HookChanges { changed })
    }

    pub fn create_project_config_if_absent(
        &self,
        config: &gat_core::config::Config,
    ) -> Result<bool, InitializationError> {
        gat_io::ConfigStore::create_project_if_absent(self.repo.layout(), config)
            .map_err(InitializationError::from)
    }

    pub fn initialize_cache(&self) -> Result<ResolvedCacheLocation, crate::RepositoryError> {
        let root = self.repo.resolved_cache_root()?;
        Ok(ResolvedCacheLocation::new(
            root.display_path().to_path_buf(),
        ))
    }
}

const fn map_status(status: gat_io::GitIntegrationStatus) -> IntegrationStatus {
    match status {
        gat_io::GitIntegrationStatus::Changed => IntegrationStatus::Changed,
        gat_io::GitIntegrationStatus::Unchanged => IntegrationStatus::Unchanged,
    }
}
