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
        command
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", isolated_gitconfig())
            .env("GIT_CONFIG_SYSTEM", isolated_gitconfig());
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
