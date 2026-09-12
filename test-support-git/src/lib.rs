//! Hermetic Git subprocess fixtures shared by every workspace test layer.
//!
//! This crate deliberately depends on no Gat crate, allowing `gat-io`,
//! `gat-engine`, `gat-command`, root tests, benchmarks, and higher-level
//! fixtures to share one Git process boundary without dependency cycles.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// Empty Git configuration used in place of ambient global and system files.
///
/// # Panics
/// Panics if the isolated configuration directory or file cannot be created.
pub fn isolated_gitconfig() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "gat-test-isolated-gitconfig-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("creating isolated gitconfig directory");
        let path = dir.join("gitconfig");
        std::fs::write(&path, "").expect("writing isolated gitconfig");
        path
    })
    .as_path()
}

/// Creates an owned, empty Git repository on `main`.
///
/// # Panics
/// Panics if temporary directory creation or Git initialization fails.
#[must_use]
pub fn empty_git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("creating Git fixture directory");
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    dir
}

const AMBIENT_GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_TEMPLATE_DIR",
    "GIT_DEFAULT_HASH",
    "GIT_DEFAULT_REF_FORMAT",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_DATE",
];

/// Isolates repository discovery, configuration, and identity for a child.
/// Apply deliberate test-specific environment overrides after this function.
/// This does not prevent ancestor discovery: repository tests must initialize
/// their own repository, and discovery tests must control their directory tree.
pub fn isolated_git_env(command: &mut Command) {
    for key in AMBIENT_GIT_ENV {
        command.env_remove(key);
    }
    command
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", isolated_gitconfig())
        .env("GIT_CONFIG_SYSTEM", isolated_gitconfig());
}

/// A Git command with deterministic identity and isolated configuration.
#[must_use]
pub struct GitCommand {
    command: Command,
}

impl GitCommand {
    pub fn new(dir: &Path, args: &[&str]) -> Self {
        Self::empty(dir).args(args)
    }

    pub fn empty(dir: &Path) -> Self {
        let mut command = Command::new("git");
        command.current_dir(dir);
        isolated_git_env(&mut command);
        Self { command }
    }

    pub fn arg(mut self, arg: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.arg(arg);
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.command.args(args);
        self
    }

    pub fn env(mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.env(key, value);
        self
    }

    pub fn env_remove(mut self, key: &str) -> Self {
        self.command.env_remove(key);
        self
    }

    pub fn with_command(mut self, f: impl FnOnce(&mut Command)) -> Self {
        f(&mut self.command);
        self
    }

    #[must_use]
    pub fn into_command(self) -> Command {
        self.command
    }

    #[allow(
        clippy::must_use_candidate,
        reason = "Running a command is useful even when its output is ignored"
    )]
    ///
    /// # Panics
    /// Panics if Git cannot be started or its output cannot be collected.
    pub fn output(self) -> Output {
        let mut command = self.command;
        let program = format!("{command:?}");
        command
            .output()
            .unwrap_or_else(|error| panic!("running {program}: {error}"))
    }

    #[allow(
        clippy::must_use_candidate,
        reason = "Running a command is useful even when its output is ignored"
    )]
    ///
    /// # Panics
    /// Panics if Git cannot be run or exits unsuccessfully.
    pub fn run(self) -> Output {
        let program = format!("{:?}", self.command);
        let output = self.output();
        assert!(
            output.status.success(),
            "{program} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

#[allow(
    clippy::must_use_candidate,
    reason = "Running a command is useful even when its output is ignored"
)]
pub fn run_git(dir: &Path, args: &[&str]) -> Output {
    GitCommand::new(dir, args).run()
}

/// Stages every working-tree change in `dir`.
pub fn stage_all(dir: &Path) {
    run_git(dir, &["add", "-A"]);
}

/// Converts a filesystem path into a `file://` remote URL, handling the
/// Windows drive-letter path shape (`C:\...` -> `file:///C:/...`)
/// correctly. Shared so every test module that needs a `file://` remote
/// constructs the URL identically.
#[must_use]
pub fn file_remote_url(path: &Path) -> String {
    let mut url = format!("file://{}", path.display());
    // On Windows, `path.display()` yields `C:\...`; normalize to the
    // `file:///C:/...` shape most git/URL tooling expects.
    if cfg!(windows) {
        url = url.replace('\\', "/");
        if !url.starts_with("file:///") {
            url = url.replacen("file://", "file:///", 1);
        }
    }
    url
}

/// Stages and commits every working-tree change in `dir`.
pub fn commit_all(dir: &Path, message: &str) {
    stage_all(dir);
    run_git(dir, &["commit", "-q", "-m", message]);
}

#[must_use]
pub fn command(dir: &Path, args: &[&str]) -> Command {
    GitCommand::new(dir, args).into_command()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_removes_ambient_values_and_allows_explicit_overrides() {
        let dir = std::env::temp_dir();
        let mut command = Command::new("git");
        for key in AMBIENT_GIT_ENV {
            command.env(key, "ambient");
        }
        isolated_git_env(&mut command);
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for key in AMBIENT_GIT_ENV {
            assert_eq!(env.get(std::ffi::OsStr::new(key)), Some(&None), "{key}");
        }
        let command = GitCommand::new(&dir, &["version"])
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .into_command();
        assert!(command.get_envs().any(|(key, value)| {
            key == "GIT_AUTHOR_DATE" && value == Some(std::ffi::OsStr::new("2000-01-01T00:00:00Z"))
        }));
    }

    #[test]
    fn fixture_owns_repository_and_initial_branch() {
        let dir = empty_git_repo();
        assert!(dir.path().join(".git").is_dir());
        let output = run_git(dir.path(), &["symbolic-ref", "HEAD"]);
        assert_eq!(output.stdout, b"refs/heads/main\n");
    }

    #[test]
    fn command_uses_isolated_config_and_fixed_identity() {
        let dir = std::env::temp_dir();
        let command = GitCommand::new(&dir, &["version"]).into_command();
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for key in [
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_AUTHOR_NAME",
            "GIT_COMMITTER_EMAIL",
        ] {
            assert!(
                env.get(std::ffi::OsStr::new(key))
                    .is_some_and(Option::is_some)
            );
        }
    }
}
