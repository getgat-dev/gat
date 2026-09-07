//! Repository initialization capabilities for `gat init`.

#[cfg(any(test, feature = "test-support"))]
use std::ffi::OsStr;
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

/// Semantic category for an initialization infrastructure failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitializationErrorKind {
    OpenRepository,
    Read,
    NonUtf8Hook,
    Write,
    GitConfig,
    GitConfigLocked,
    ConfigScaffold,
}

/// Failure to converge repository initialization state.
#[derive(Debug)]
pub struct InitializationError {
    kind: InitializationErrorKind,
    filesystem_failure: Option<crate::FilesystemFailureKind>,
    path: Option<PathBuf>,
    operation: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl InitializationError {
    #[must_use]
    pub const fn kind(&self) -> InitializationErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn filesystem_failure(&self) -> Option<crate::FilesystemFailureKind> {
        self.filesystem_failure
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }
}

impl From<gat_io::GitIntegrationError> for InitializationError {
    fn from(source: gat_io::GitIntegrationError) -> Self {
        use gat_io::GitIntegrationErrorKind;

        let kind = match source.kind() {
            GitIntegrationErrorKind::OpenRepository => InitializationErrorKind::OpenRepository,
            GitIntegrationErrorKind::Read => InitializationErrorKind::Read,
            GitIntegrationErrorKind::NonUtf8 => InitializationErrorKind::NonUtf8Hook,
            GitIntegrationErrorKind::Write => InitializationErrorKind::Write,
            GitIntegrationErrorKind::Config => InitializationErrorKind::GitConfig,
            GitIntegrationErrorKind::ConfigLocked => InitializationErrorKind::GitConfigLocked,
        };
        Self {
            kind,
            filesystem_failure: source
                .io_kind()
                .map(crate::repository_access::classify_io_kind),
            path: Some(source.path().to_path_buf()),
            operation: source.operation().to_string(),
            source: (kind != InitializationErrorKind::NonUtf8Hook)
                .then(|| Box::new(source) as Box<dyn std::error::Error + Send + Sync>),
        }
    }
}

impl From<gat_io::ConfigWriteError> for InitializationError {
    fn from(source: gat_io::ConfigWriteError) -> Self {
        Self {
            kind: InitializationErrorKind::ConfigScaffold,
            filesystem_failure: match &source {
                gat_io::ConfigWriteError::Write(source) => source
                    .io_kind()
                    .map(crate::repository_access::classify_io_kind),
                gat_io::ConfigWriteError::Serialize { .. } => None,
            },
            operation: "creating the project gat.yaml".to_string(),
            path: None,
            source: Some(Box::new(source)),
        }
    }
}

impl std::fmt::Display for InitializationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            InitializationErrorKind::NonUtf8Hook => {
                write!(
                    f,
                    "hook `{}` is not valid UTF-8",
                    self.path
                        .as_deref()
                        .expect("git integration errors always carry a path")
                        .display()
                )
            }
            _ => write!(f, "{} failed", self.operation),
        }
    }
}

impl std::error::Error for InitializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
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
            if !existing.contains(BEGIN) {
                continue;
            }
            let updated = managed_block::remove(&existing, BEGIN, END);
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

    #[must_use]
    pub fn initialize_cache(&self) -> ResolvedCacheLocation {
        let root = self.repo.resolved_cache_root();
        ResolvedCacheLocation::new(root.display_path().to_path_buf())
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    #[must_use]
    pub fn initialize_cache_with(
        &self,
        cache_dir_override: Option<&OsStr>,
        global_config_dir: Option<PathBuf>,
    ) -> ResolvedCacheLocation {
        let root = self
            .repo
            .resolved_cache_root_with(cache_dir_override, global_config_dir);
        ResolvedCacheLocation::new(root.display_path().to_path_buf())
    }
}

const fn map_status(status: gat_io::GitIntegrationStatus) -> IntegrationStatus {
    match status {
        gat_io::GitIntegrationStatus::Changed => IntegrationStatus::Changed,
        gat_io::GitIntegrationStatus::Unchanged => IntegrationStatus::Unchanged,
    }
}
