//! Pure repository path layout and discovery: the physical `.gat` layout
//! facts (repo root, config/state/lock paths) and the walk-up-to-`.git`
//! discovery algorithm, with no knowledge of config/lock domain types.
//! The type centralizes repository paths owned by the I/O layer.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gat_core::{cache_location::CacheLocation, config::ConfigScope};

/// A resolved local object-cache capability.
///
/// The physical cache layout stays inside `gat-io`; higher layers retain
/// this cheap-to-clone handle and ask it for the specific cache capability
/// they need.
#[derive(Clone, Debug)]
pub struct CacheRoot {
    inner: Arc<CacheRootInner>,
}

#[derive(Debug)]
pub(crate) struct CacheRootInner {
    pub(crate) objects_dir: PathBuf,
}

impl CacheRoot {
    #[must_use]
    pub fn open_client(&self) -> crate::CacheClient {
        crate::CacheClient::open_shared(Arc::clone(&self.inner))
    }

    #[must_use]
    pub fn writer(&self) -> crate::CacheWriter {
        crate::CacheWriter::new(Arc::clone(&self.inner))
    }

    #[must_use]
    pub fn presence(&self) -> crate::CachePresence {
        crate::CachePresence::new(Arc::clone(&self.inner))
    }

    #[must_use]
    pub fn maintenance(&self) -> crate::CacheMaintenance<'_> {
        crate::CacheMaintenance::new(&self.inner.objects_dir)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    #[must_use]
    pub fn object_path_for_test(&self, oid: &gat_core::oid::Oid) -> PathBuf {
        crate::cache::object::cache_path_oid(&self.inner.objects_dir, oid)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn make_object_writable_for_test(
        &self,
        oid: &gat_core::oid::Oid,
    ) -> crate::CacheResult<()> {
        crate::cache::object::unprotect(&self.object_path_for_test(oid))
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn break_database_for_test(&self) {
        let client = crate::cache::object::CacheClient::open(self.inner.objects_dir.clone());
        client.break_database_for_test();
    }

    /// The resolved host path for user-facing presentation only.
    #[must_use]
    pub fn display_path(&self) -> &Path {
        &self.inner.objects_dir
    }
}

/// Typed failures from repository discovery and layout-path resolution.
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    /// [`std::env::current_dir`] failed (e.g. the directory was deleted
    /// out from under the process, or is otherwise unreadable).
    #[error("could not resolve the current working directory")]
    CurrentDirectory(#[source] std::io::Error),

    /// No `.git` marker was found in the current directory or any
    /// ancestor.
    #[error("not a git repository (or any parent up to /)")]
    NotRepository,

    /// `RepositoryLayout::config_path_for_home` was asked for
    /// [`ConfigScope::Global`] but `$HOME`/`%USERPROFILE%` is not set.
    #[error(
        "cannot resolve the global gat config path: $HOME (or %USERPROFILE% on Windows) is not set"
    )]
    ConfigPathUnavailable,
}

/// Whether `path` (a candidate `<dir>/.git`) actually marks a git repo
/// root, not just some unrelated file or directory that happens to be
/// named `.git`. A real git checkout's `.git` is either a directory (the
/// common case) or, for worktrees and submodules, a file whose contents
/// start with `gitdir: ` pointing at the real git dir elsewhere. Anything
/// else (a stray empty file, a symlink to nowhere, etc.) is rejected so
/// discovery doesn't mistake it for a repo root.
fn is_git_marker(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => true,
        Ok(meta) if meta.is_file() => {
            std::fs::read_to_string(path).is_ok_and(|s| s.trim_start().starts_with("gitdir:"))
        }
        _ => false,
    }
}

/// The physical layout of a `gat` repository: its root directory and the
/// derived paths for `.gat`-owned state (config, materialized state
/// database, sync lock). Owns discovery (walking up to find `.git`) and
/// pure path derivation only -- no config/lock domain policy and no
/// object maintenance. Cache-location derivation consumes already-resolved
/// config/environment inputs without reading either source itself.
#[derive(Debug, Clone)]
pub struct RepositoryLayout {
    root: PathBuf,
    cache_root: PathBuf,
}

impl RepositoryLayout {
    /// Discover the repository containing the current working directory
    /// by walking up to find a `.git` marker.
    pub fn discover() -> Result<Self, LayoutError> {
        let dir = std::env::current_dir().map_err(LayoutError::CurrentDirectory)?;
        Self::discover_from(dir)
    }

    /// Pure variant of [`Self::discover`] that walks up from an explicit
    /// starting directory instead of reading the process's current
    /// working directory.
    pub fn discover_from(dir: PathBuf) -> Result<Self, LayoutError> {
        Self::discover_with_marker(dir, is_git_marker)
    }

    fn discover_with_marker(
        mut dir: PathBuf,
        mut has_marker: impl FnMut(&Path) -> bool,
    ) -> Result<Self, LayoutError> {
        loop {
            if has_marker(&dir.join(".git")) {
                return Ok(Self::at(dir));
            }
            if !dir.pop() {
                return Err(LayoutError::NotRepository);
            }
        }
    }

    /// Construct a layout for a known repository root, deriving
    /// `cache_root` as `root.join(".gat")`.
    #[must_use]
    pub fn at(root: PathBuf) -> Self {
        let cache_root = root.join(".gat");
        Self { root, cache_root }
    }

    pub(crate) fn root_path(&self) -> &Path {
        &self.root
    }

    /// Borrow this repository's working-tree capability without exposing
    /// raw-path construction to downstream crates.
    #[must_use]
    pub fn worktree_client(&self) -> crate::WorktreeClient<'_> {
        crate::WorktreeClient::new(self.root_path())
    }

    pub(crate) fn cache_root_path(&self) -> &Path {
        &self.cache_root
    }

    /// `<repo_root>/gat.yaml` (the project-scope config location).
    pub(crate) fn config_path(&self) -> PathBuf {
        self.root.join("gat.yaml")
    }

    /// The `<repo_root>/.gat/gat.yaml` (local) location's directory --
    /// same as `cache_root`, but named for its use here so callers reading
    /// `local_config_path` don't need to know that detail.
    pub(crate) fn local_config_dir(&self) -> &Path {
        &self.cache_root
    }

    /// The repo-local `SQLite` database backing materialized state.
    pub(crate) fn materialized_db_path(&self) -> PathBuf {
        self.cache_root.join("state").join("state.sqlite3")
    }

    pub(crate) fn sync_lock_path(&self) -> PathBuf {
        self.cache_root.join("state").join("sync.lock")
    }

    /// Resolve the repository's object-cache directory from already-read
    /// configuration and environment inputs.
    ///
    /// Keeping this physical path derivation beside the rest of the
    /// repository layout prevents higher layers from independently
    /// reconstructing the default and relative configured locations.
    #[must_use]
    pub fn resolve_cache_root(
        &self,
        cache_dir_override: Option<&OsStr>,
        configured_location: Option<&CacheLocation>,
    ) -> CacheRoot {
        let objects_dir = if let Some(dir) = cache_dir_override {
            PathBuf::from(dir)
        } else {
            match configured_location {
                Some(location) if location.as_path().is_absolute() => {
                    location.as_path().to_path_buf()
                }
                Some(location) => self.root.join(location.as_path()),
                None => self.cache_root.join("objects"),
            }
        };
        CacheRoot {
            inner: Arc::new(CacheRootInner { objects_dir }),
        }
    }

    /// Pure resolver: the global config directory from an explicit,
    /// already-resolved home directory (or `None` if there isn't one),
    /// with no environment access.
    #[must_use]
    #[allow(
        clippy::single_option_map,
        reason = "This resolver centralizes the configuration directory layout for all callers"
    )]
    pub fn global_config_dir_from(home: Option<&Path>) -> Option<PathBuf> {
        home.map(|home| home.join(".gat"))
    }

    /// The `gat.yaml` path for `scope`, given an already-resolved global
    /// config directory (see [`Self::global_config_dir_from`]). Fails only
    /// for [`ConfigScope::Global`] when `global_config_dir` is `None`.
    pub(crate) fn config_path_for_home(
        &self,
        scope: ConfigScope,
        global_config_dir: Option<&Path>,
    ) -> Result<PathBuf, LayoutError> {
        Ok(match scope {
            ConfigScope::Global => global_config_dir
                .ok_or(LayoutError::ConfigPathUnavailable)?
                .join("gat.yaml"),
            ConfigScope::Project => self.config_path(),
            ConfigScope::Local => self.local_config_dir().join("gat.yaml"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_derives_cache_root_under_dot_gat() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        assert_eq!(layout.root_path(), Path::new("/repo"));
        assert_eq!(layout.cache_root_path(), Path::new("/repo/.gat"));
    }

    #[test]
    fn config_path_is_project_scoped_at_repo_root() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        assert_eq!(layout.config_path(), PathBuf::from("/repo/gat.yaml"));
    }

    #[test]
    fn local_config_dir_is_the_cache_root() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        assert_eq!(layout.local_config_dir(), Path::new("/repo/.gat"));
    }

    #[test]
    fn materialized_db_path_is_under_cache_root_state() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        assert_eq!(
            layout.materialized_db_path(),
            PathBuf::from("/repo/.gat/state/state.sqlite3")
        );
    }

    #[test]
    fn sync_lock_path_is_under_cache_root_state() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        assert_eq!(
            layout.sync_lock_path(),
            PathBuf::from("/repo/.gat/state/sync.lock")
        );
    }

    #[test]
    fn cache_resolution_prefers_override_without_reinterpreting_it() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        let configured = CacheLocation::from_path(PathBuf::from("configured"));

        assert_eq!(
            layout
                .resolve_cache_root(Some(OsStr::new("override")), Some(&configured))
                .display_path(),
            Path::new("override")
        );
    }

    #[test]
    fn cache_resolution_joins_only_relative_configured_locations() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        let relative = CacheLocation::from_path(PathBuf::from("shared"));
        let absolute = CacheLocation::from_path(PathBuf::from("/var/cache/gat"));

        assert_eq!(
            layout
                .resolve_cache_root(None, Some(&relative))
                .display_path(),
            Path::new("/repo/shared")
        );
        assert_eq!(
            layout
                .resolve_cache_root(None, Some(&absolute))
                .display_path(),
            Path::new("/var/cache/gat")
        );
        assert_eq!(
            layout.resolve_cache_root(None, None).display_path(),
            Path::new("/repo/.gat/objects")
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_resolution_preserves_non_utf8_override_bytes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        let override_path = std::ffi::OsString::from_vec(vec![b'c', b'a', 0xff]);
        let resolved = layout.resolve_cache_root(Some(&override_path), None);

        assert_eq!(
            resolved.display_path().as_os_str().as_bytes(),
            override_path.as_bytes()
        );
    }

    #[test]
    fn global_config_dir_from_appends_dot_gat_to_an_explicit_home() {
        let home = Path::new("/home/alice");
        assert_eq!(
            RepositoryLayout::global_config_dir_from(Some(home)),
            Some(PathBuf::from("/home/alice/.gat"))
        );
    }

    #[test]
    fn global_config_dir_from_is_none_without_a_home() {
        assert_eq!(RepositoryLayout::global_config_dir_from(None), None);
    }

    #[test]
    fn config_path_for_home_resolves_project_scope() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        assert_eq!(
            layout
                .config_path_for_home(ConfigScope::Project, None)
                .unwrap(),
            PathBuf::from("/repo/gat.yaml")
        );
    }

    #[test]
    fn config_path_for_home_resolves_local_scope() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        assert_eq!(
            layout
                .config_path_for_home(ConfigScope::Local, None)
                .unwrap(),
            PathBuf::from("/repo/.gat/gat.yaml")
        );
    }

    #[test]
    fn config_path_for_home_resolves_global_scope_from_an_explicit_dir() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        let global_dir = PathBuf::from("/home/alice/.gat");
        assert_eq!(
            layout
                .config_path_for_home(ConfigScope::Global, Some(&global_dir))
                .unwrap(),
            PathBuf::from("/home/alice/.gat/gat.yaml")
        );
    }

    #[test]
    fn config_path_for_home_reports_config_path_unavailable_without_a_home() {
        let layout = RepositoryLayout::at(PathBuf::from("/repo"));
        let err = layout
            .config_path_for_home(ConfigScope::Global, None)
            .unwrap_err();
        assert!(matches!(err, LayoutError::ConfigPathUnavailable));
    }

    #[test]
    fn discover_from_finds_a_dot_git_directory_in_the_starting_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let layout = RepositoryLayout::discover_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(layout.root_path(), dir.path());
    }

    #[test]
    fn discover_from_walks_up_ancestors_to_find_a_dot_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        let layout = RepositoryLayout::discover_from(nested).unwrap();
        assert_eq!(layout.root_path(), dir.path());
    }

    #[test]
    fn discover_from_recognizes_a_gitdir_file_marker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".git"),
            "gitdir: /elsewhere/.git/worktrees/x\n",
        )
        .unwrap();
        let layout = RepositoryLayout::discover_from(dir.path().to_path_buf()).unwrap();
        assert_eq!(layout.root_path(), dir.path());
    }

    #[test]
    fn discover_from_skips_a_stray_dot_git_file_without_a_gitdir_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join(".git"), "not a gitdir marker").unwrap();
        let layout = RepositoryLayout::discover_from(nested).unwrap();
        assert_eq!(layout.root_path(), dir.path());
    }

    #[test]
    fn discovery_without_markers_checks_every_ancestor_and_reports_not_repository() {
        let dir = tempfile::tempdir().unwrap();
        // Even a real marker must not affect the injected marker-free filesystem.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let start = dir.path().join("a").join("b");
        let mut visited = Vec::new();
        let err = RepositoryLayout::discover_with_marker(start.clone(), |path| {
            visited.push(path.to_path_buf());
            false
        })
        .unwrap_err();
        assert!(matches!(err, LayoutError::NotRepository));
        let expected: Vec<_> = start.ancestors().map(|path| path.join(".git")).collect();
        assert_eq!(visited, expected);
    }
}
