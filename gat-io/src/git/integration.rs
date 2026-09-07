use std::io::Write;
use std::path::{Path, PathBuf};

use super::BoxedSource;
use crate::RepositoryLayout;

const ATTR_BEGIN: &str = "# >>> gat >>>";
const ATTR_END: &str = "# <<< gat <<<";
const ATTR_BODY: &str = "/gat.lock merge=gat-lock\n/gat.lock/** merge=gat-lock\n";
const MERGE_DRIVER_HUMAN_NAME: &str = "gat.lock semantic merge driver";
const MERGE_DRIVER_COMMAND: &str = "gat merge-driver %O %A %B";

/// Whether an idempotent Git integration mutation changed its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitIntegrationStatus {
    Changed,
    Unchanged,
}

/// Semantic category for a Git integration failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitIntegrationErrorKind {
    OpenRepository,
    Read,
    NonUtf8,
    Write,
    Config,
    ConfigLocked,
}

/// Failure to access repository-local Git integration files.
#[derive(Debug)]
pub struct GitIntegrationError {
    kind: GitIntegrationErrorKind,
    path: PathBuf,
    operation: String,
    source: Option<BoxedSource>,
    io_kind: Option<std::io::ErrorKind>,
}

impl GitIntegrationError {
    #[must_use]
    pub const fn kind(&self) -> GitIntegrationErrorKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    fn new(
        kind: GitIntegrationErrorKind,
        path: &Path,
        operation: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            path: path.to_path_buf(),
            operation: operation.into(),
            source: Some(Box::new(source)),
            io_kind: None,
        }
    }

    #[must_use]
    pub const fn io_kind(&self) -> Option<std::io::ErrorKind> {
        self.io_kind
    }

    fn io(
        kind: GitIntegrationErrorKind,
        path: &Path,
        operation: impl Into<String>,
        source: std::io::Error,
    ) -> Self {
        let io_kind = Some(source.kind());
        let mut error = Self::new(kind, path, operation, source);
        error.io_kind = io_kind;
        error
    }

    fn non_utf8(path: &Path) -> Self {
        Self {
            kind: GitIntegrationErrorKind::NonUtf8,
            path: path.to_path_buf(),
            operation: format!("reading {}", path.display()),
            source: None,
            io_kind: None,
        }
    }
}

impl std::fmt::Display for GitIntegrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.kind == GitIntegrationErrorKind::NonUtf8 {
            write!(
                f,
                "hook script `{}` is not valid UTF-8",
                self.path.display()
            )
        } else {
            write!(f, "{} failed", self.operation)
        }
    }
}

impl std::error::Error for GitIntegrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// Physical access to files in a repository's common Git directory.
#[derive(Debug)]
pub struct GitIntegration {
    common_dir: PathBuf,
}

impl GitIntegration {
    pub fn open(layout: &RepositoryLayout) -> Result<Self, GitIntegrationError> {
        let root = layout.root_path();
        let common_dir = super::common_dir_at(root).map_err(|source| GitIntegrationError {
            kind: GitIntegrationErrorKind::OpenRepository,
            io_kind: source.io_kind(),
            path: root.to_path_buf(),
            operation: format!("opening git repository at {}", root.display()),
            source: Some(Box::new(source)),
        })?;
        Ok(Self { common_dir })
    }

    pub fn read_hook(&self, name: &str) -> Result<Option<String>, GitIntegrationError> {
        read_optional_text(&self.common_dir.join("hooks").join(name), true)
    }

    #[allow(
        clippy::missing_panics_doc,
        reason = "Repository hook paths are constructed with a parent directory"
    )]
    pub fn write_hook(&self, name: &str, contents: &str) -> Result<(), GitIntegrationError> {
        let path = self.common_dir.join("hooks").join(name);
        let dir = path.parent().expect("hook path has a parent");
        std::fs::create_dir_all(dir).map_err(|source| {
            GitIntegrationError::io(
                GitIntegrationErrorKind::Write,
                dir,
                format!("creating {}", dir.display()),
                source,
            )
        })?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(dir)
            .map_err(|source| {
                GitIntegrationError::io(
                    GitIntegrationErrorKind::Write,
                    dir,
                    format!("creating temp file in {}", dir.display()),
                    source,
                )
            })?;
        tmp.write_all(contents.as_bytes()).map_err(|source| {
            GitIntegrationError::io(
                GitIntegrationErrorKind::Write,
                &path,
                format!("writing {}", path.display()),
                source,
            )
        })?;
        tmp.as_file().sync_all().map_err(|source| {
            GitIntegrationError::io(
                GitIntegrationErrorKind::Write,
                &path,
                format!("syncing {}", path.display()),
                source,
            )
        })?;
        make_executable(tmp.path())?;
        crate::atomic::persist_with_retry(tmp, &path).map_err(|source| {
            GitIntegrationError::io(
                GitIntegrationErrorKind::Write,
                &path,
                format!("publishing {}", path.display()),
                source.error,
            )
        })?;
        Ok(())
    }

    pub fn remove_hook(&self, name: &str) -> Result<(), GitIntegrationError> {
        remove_file(&self.common_dir.join("hooks").join(name))
    }

    pub fn install_merge_driver(&self) -> Result<GitIntegrationStatus, GitIntegrationError> {
        let path = self.common_dir.join("config");
        let mut file = load_config(&path)?;
        let mut section = file
            .section_mut_or_create_new("merge", "gat-lock")
            .map_err(|source| {
                GitIntegrationError::new(
                    GitIntegrationErrorKind::Config,
                    &path,
                    "editing merge.gat-lock section",
                    source,
                )
            })?;
        if value_is(&section, "name", MERGE_DRIVER_HUMAN_NAME)
            && value_is(&section, "driver", MERGE_DRIVER_COMMAND)
        {
            return Ok(GitIntegrationStatus::Unchanged);
        }
        section
            .set("name", MERGE_DRIVER_HUMAN_NAME)
            .map_err(|source| {
                GitIntegrationError::new(
                    GitIntegrationErrorKind::Config,
                    &path,
                    "setting merge.gat-lock.name",
                    source,
                )
            })?;
        section
            .set("driver", MERGE_DRIVER_COMMAND)
            .map_err(|source| {
                GitIntegrationError::new(
                    GitIntegrationErrorKind::Config,
                    &path,
                    "setting merge.gat-lock.driver",
                    source,
                )
            })?;
        drop(section);
        persist_config(&path, &file)?;
        Ok(GitIntegrationStatus::Changed)
    }

    pub fn uninstall_merge_driver(&self) -> Result<GitIntegrationStatus, GitIntegrationError> {
        let path = self.common_dir.join("config");
        let mut file = load_config(&path)?;
        if file
            .remove_section("merge", Some("gat-lock".into()))
            .is_none()
        {
            return Ok(GitIntegrationStatus::Unchanged);
        }
        persist_config(&path, &file)?;
        Ok(GitIntegrationStatus::Changed)
    }

    pub fn install_merge_attributes(&self) -> Result<GitIntegrationStatus, GitIntegrationError> {
        let path = self.common_dir.join("info").join("attributes");
        let existing = read_optional_text(&path, false)?.unwrap_or_default();
        let updated = gat_core::managed_block::upsert(&existing, ATTR_BEGIN, ATTR_END, ATTR_BODY);
        if updated == existing {
            return Ok(GitIntegrationStatus::Unchanged);
        }
        write_text(&path, &updated)?;
        Ok(GitIntegrationStatus::Changed)
    }

    pub fn uninstall_merge_attributes(&self) -> Result<GitIntegrationStatus, GitIntegrationError> {
        let path = self.common_dir.join("info").join("attributes");
        let Some(existing) = read_optional_text(&path, false)? else {
            return Ok(GitIntegrationStatus::Unchanged);
        };
        if !existing.contains(ATTR_BEGIN) {
            return Ok(GitIntegrationStatus::Unchanged);
        }
        let updated = gat_core::managed_block::remove(&existing, ATTR_BEGIN, ATTR_END);
        if updated.trim().is_empty() {
            remove_file(&path)?;
        } else {
            write_text(&path, &updated)?;
        }
        Ok(GitIntegrationStatus::Changed)
    }
}

fn read_optional_text(
    path: &Path,
    non_utf8_is_hook: bool,
) -> Result<Option<String>, GitIntegrationError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) if non_utf8_is_hook && source.kind() == std::io::ErrorKind::InvalidData => {
            Err(GitIntegrationError::non_utf8(path))
        }
        Err(source) => Err(GitIntegrationError::io(
            GitIntegrationErrorKind::Read,
            path,
            format!("reading {}", path.display()),
            source,
        )),
    }
}

fn write_text(path: &Path, contents: &str) -> Result<(), GitIntegrationError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| {
            GitIntegrationError::io(
                GitIntegrationErrorKind::Write,
                parent,
                format!("creating {}", parent.display()),
                source,
            )
        })?;
    }
    crate::atomic::write_atomic(path, contents).map_err(|source| {
        let io_kind = source.io_kind();
        let mut error = GitIntegrationError::new(
            GitIntegrationErrorKind::Write,
            path,
            format!("writing {}", path.display()),
            source,
        );
        error.io_kind = io_kind;
        error
    })
}

fn remove_file(path: &Path) -> Result<(), GitIntegrationError> {
    std::fs::remove_file(path).map_err(|source| {
        GitIntegrationError::io(
            GitIntegrationErrorKind::Write,
            path,
            format!("removing {}", path.display()),
            source,
        )
    })
}

fn value_is(section: &gix_config::file::SectionMut<'_>, key: &str, expected: &str) -> bool {
    section
        .value(key)
        .is_some_and(|value| value.to_vec() == expected.as_bytes())
}

fn load_config(path: &Path) -> Result<gix_config::File, GitIntegrationError> {
    let existing = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(GitIntegrationError::io(
                GitIntegrationErrorKind::Config,
                path,
                format!("reading {}", path.display()),
                source,
            ));
        }
    };
    let meta = gix_config::file::Metadata::try_from_path(path, gix_config::Source::Local)
        .unwrap_or_else(|_| gix_config::Source::Local.into());
    gix_config::File::from_bytes_no_includes(&existing, meta, Default::default()).map_err(
        |source| {
            GitIntegrationError::new(
                GitIntegrationErrorKind::Config,
                path,
                format!("parsing {}", path.display()),
                source,
            )
        },
    )
}

fn persist_config(path: &Path, file: &gix_config::File) -> Result<(), GitIntegrationError> {
    let mut lock = gix_lock::File::acquire_to_update_resource(
        path,
        gix_lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|source| match source {
        gix_lock::acquire::Error::Io(source) => GitIntegrationError::io(
            GitIntegrationErrorKind::Config,
            path,
            "locking Git configuration",
            source,
        ),
        source @ gix_lock::acquire::Error::PermanentlyLocked { .. } => GitIntegrationError::new(
            GitIntegrationErrorKind::ConfigLocked,
            path,
            "locking Git configuration",
            source,
        ),
    })?;
    file.write_to(&mut lock).map_err(|source| {
        GitIntegrationError::io(
            GitIntegrationErrorKind::Config,
            path,
            format!("writing {}", path.display()),
            source,
        )
    })?;
    lock.commit().map_err(|source| {
        GitIntegrationError::io(
            GitIntegrationErrorKind::Config,
            path,
            format!("publishing {}", path.display()),
            source.error,
        )
    })?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), GitIntegrationError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|source| {
        GitIntegrationError::io(
            GitIntegrationErrorKind::Write,
            path,
            format!("marking {} executable", path.display()),
            source,
        )
    })
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), GitIntegrationError> {
    Ok(())
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn io_constructors_preserve_os_kinds_and_keep_messages_private() {
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::StorageFull,
        ] {
            let error = GitIntegrationError::io(
                GitIntegrationErrorKind::Write,
                Path::new("hook"),
                "writing hook",
                std::io::Error::new(kind, "SENTINEL"),
            );
            assert_eq!(error.io_kind(), Some(kind));
            assert!(!error.to_string().contains("SENTINEL"));
        }
    }
}
