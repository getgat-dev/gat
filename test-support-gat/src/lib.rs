//! Dev-only test fixtures shared by Gat's command/domain unit tests and
//! external integration tests.
//!
//! Kept out of the production `gat` API surface: it's only ever reached
//! through `[dev-dependencies]`, never through `gat`'s own `[dependencies]`
//! or public re-exports, so nothing here ships in the built binary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::Duration;

use test_support_git::run_git;

/// A freshly initialized Git repository fixture.
/// # Panics
/// Panics if the temporary directory cannot be created or Git initialization fails.
#[must_use]
pub fn empty_git_repo() -> TestRepo {
    TestRepo::empty_git_repo()
}

/// A Git repository fixture with one initial commit.
#[must_use]
pub fn git_repo_with_initial_commit() -> TestRepo {
    TestRepo::git_repo_with_initial_commit()
}

/// Adds paths through the authoritative command implementation.
pub fn add(
    repo: &gat_engine::Repository,
    paths: &[PathBuf],
    progress: &dyn gat_core::progress::ProgressReporter,
) -> Result<gat_command::AddOutcome, gat_command::AddError> {
    let paths = paths
        .iter()
        .map(gat_core::path_scope::normalize_path_scope)
        .collect::<Result<Vec<_>, _>>()?;
    gat_command::add(
        repo,
        gat_command::AddRequest {
            paths,
            force: false,
        },
        progress,
    )
}

/// Reconciles the whole repository through the authoritative command path.
pub fn sync(
    repo: &gat_engine::Repository,
    progress: &dyn gat_core::progress::ProgressReporter,
) -> Result<gat_command::SyncOutcome, gat_command::SyncError> {
    gat_command::sync(
        repo,
        gat_command::SyncRequest {
            selection: Some(gat_core::selection::Selection::root()),
            force: false,
            dry_run: false,
            trust_state: false,
            fetch: false,
            repair: false,
            remote: None,
            rematerialize: false,
        },
        progress,
    )
}

/// Adds a project-scoped remote through the authoritative command path.
pub fn remote_add(
    repo: &gat_engine::Repository,
    name: &str,
    url: String,
) -> Result<gat_command::RemoteOutcome, gat_command::RemoteError> {
    gat_command::remote(
        repo,
        gat_command::RemoteRequest::Add {
            name: gat_core::name::RemoteName::from_string(name.to_string()),
            url: gat_core::endpoint::RemoteUrlTemplate::from_string(url),
            scope: gat_core::config::ConfigScope::Project,
        },
    )
}

/// Adds a transport-fixture remote and explicitly chooses it if the fixture has no default.
pub fn remote_add_with_default(
    repo: &gat_engine::Repository,
    name: &str,
    url: String,
) -> Result<gat_command::RemoteOutcome, gat_command::RemoteError> {
    let outcome = remote_add(repo, name, url)?;
    if repo.load_config()?.remotes.default.is_none() {
        gat_command::remote(
            repo,
            gat_command::RemoteRequest::Default {
                action: gat_engine::DefaultAction::Set(name.into()),
                scope: gat_core::config::ConfigScope::Project,
            },
        )?;
    }
    Ok(outcome)
}

/// Adds a project-scoped route through the authoritative command path.
pub fn route_add(
    repo: &gat_engine::Repository,
    name: &str,
    remote: &str,
    path: &str,
) -> std::result::Result<gat_command::RouteOutcome, Box<dyn std::error::Error>> {
    let path = gat_core::config::normalize_route_path(path)?;
    gat_command::route(
        repo,
        gat_command::RouteRequest::Add {
            name: gat_core::name::RouteName::from_string(name.to_string()),
            remote: gat_core::name::RemoteName::from_string(remote.to_string()),
            path,
            scope: gat_core::config::ConfigScope::Project,
        },
    )
    .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
}

/// Removes paths through the authoritative command implementation.
pub fn remove(
    repo: &gat_engine::Repository,
    paths: &[PathBuf],
    cached: bool,
) -> Result<gat_command::RemoveOutcome, gat_command::RemoveError> {
    let paths = paths
        .iter()
        .map(gat_core::path_scope::normalize_path_scope)
        .collect::<Result<Vec<_>, _>>()?;
    gat_command::remove(repo, gat_command::RemoveRequest { paths, cached })
}

/// A disposable git (and, for [`TestRepo::empty_gat_repo`]/
/// [`TestRepo::gat_repo`], `gat`-initialized) repository fixture that
/// owns its [`tempfile::TempDir`] and exposes only the small set of
/// high-value primitives most tests actually need, rather than one
/// configurable mega-builder.
///
/// **Hermeticity guarantee**: every constructor's filesystem state lives
/// exclusively beneath its own owned temporary directory, and none of
/// them ever depend on: the caller's current directory; any
/// pre-existing repository state; the developer/CI machine's real
/// `$HOME`/`%USERPROFILE%`; the `GAT_CACHE_LOCATION` environment variable; a
/// real, ambient global Gat configuration (`~/.gat/gat.yaml`); or the
/// developer/CI machine's real global/system Git configuration
/// (`~/.gitconfig`/`/etc/gitconfig`). [`TestRepo::empty_gat_repo`] and
/// [`TestRepo::gat_repo`] construct an invocation from explicit empty inputs;
/// every Git subprocess invocation
/// these fixtures make goes through [`run_git`]/[`test_support_git::GitCommand`], which
/// isolate global/system Git config the same way (see
/// [`test_support_git::isolated_gitconfig`]). That same `gat init` opens the repository
/// through `gat-command`, `gat-engine`, and `gat-io::GitIntegration`;
/// the I/O capability uses isolated Gix options, so the global/system Git
/// config independence holds for Gix discovery too, not only for the
/// plain `git`/`gat` subprocess isolation above.
pub struct TestRepo {
    dir: tempfile::TempDir,
    /// Cache location returned by initialization, available through
    /// [`Self::cache_dir`] for initialized Gat fixtures.
    cache_dir: Option<String>,
}

/// Initializes a fixture using exactly the same explicit invocation path as production.
fn run_gat_init(repo: &TestRepo) -> String {
    let invocation = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap();
    let repo = invocation.repository_at(repo.path().to_path_buf());
    gat_command::init(&repo, gat_command::InitRequest::default())
        .expect("gat init in TestRepo fixture")
        .cache_location
        .display_path()
        .display()
        .to_string()
}

impl TestRepo {
    /// A freshly `git init`-ed repo with no commits yet.
    /// # Panics
    /// Panics if the temporary directory cannot be created or Git initialization fails.
    #[must_use]
    pub fn empty_git_repo() -> Self {
        let dir = tempfile::tempdir().expect("creating TestRepo tempdir");
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        Self {
            dir,
            cache_dir: None,
        }
    }

    /// [`Self::empty_git_repo`] plus one initial commit (a `README` file),
    /// so `gat`'s repo discovery (and anything that needs `HEAD` to
    /// resolve) has something to find.
    #[must_use]
    pub fn git_repo_with_initial_commit() -> Self {
        let repo = Self::empty_git_repo();
        repo.write("README", "hi");
        repo.commit_all("init");
        repo
    }

    /// [`Self::empty_git_repo`] plus `gat init` run in-process against the
    /// real production entry point ([`gat_command::init`])
    /// -- exercising the same initialization code path a spawned `gat
    /// init` would, without spawning a process. Unlike [`Self::gat_repo`],
    /// this has no commits yet, matching what a real `git init && gat
    /// init` sequence produces before any test-specific file is added.
    ///
    /// Fully hermetic: never reads (or depends on the presence/absence
    /// of) the developer/CI machine's real `$HOME`/`%USERPROFILE%`,
    /// `GAT_CACHE_LOCATION`, or global Gat configuration; see `Invocation`.
    #[must_use]
    pub fn empty_gat_repo() -> Self {
        let mut repo = Self::empty_git_repo();
        repo.cache_dir = Some(run_gat_init(&repo));
        repo
    }

    /// [`Self::git_repo_with_initial_commit`] plus `gat init` run
    /// in-process against the real production entry point
    /// ([`gat_command::init`]) -- exercising the
    /// same initialization code path a spawned `gat init` would, without
    /// spawning a process.
    ///
    /// Fully hermetic: never reads (or depends on the presence/absence
    /// of) the developer/CI machine's real `$HOME`/`%USERPROFILE%`,
    /// `GAT_CACHE_LOCATION`, or global Gat configuration; see `Invocation`.
    #[must_use]
    pub fn gat_repo() -> Self {
        let mut repo = Self::git_repo_with_initial_commit();
        repo.cache_dir = Some(run_gat_init(&repo));
        repo
    }

    /// This repo's root directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The cache directory returned by initialization. Use this resolved
    /// path rather than recomputing it from ambient configuration.
    /// # Panics
    /// Panics if this fixture has not run gat initialization.
    #[must_use]
    pub fn cache_dir(&self) -> &str {
        self.cache_dir
            .as_deref()
            .expect("TestRepo::cache_dir() called on a fixture that never ran gat init")
    }

    /// Writes `contents` to `relative_path` under this repo's root,
    /// creating any missing parent directories first.
    /// # Panics
    /// Panics if a parent directory or the file cannot be written.
    pub fn write(&self, relative_path: &str, contents: impl AsRef<[u8]>) {
        let path = self.path().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("creating parent directory for TestRepo::write");
        }
        std::fs::write(&path, contents).expect("writing TestRepo file");
    }

    /// Reads `relative_path` under this repo's root as UTF-8 text.
    /// # Panics
    /// Panics if the file cannot be read as UTF-8.
    #[must_use]
    pub fn read(&self, relative_path: &str) -> String {
        std::fs::read_to_string(self.path().join(relative_path)).expect("reading TestRepo file")
    }

    /// Runs `git` with `args` in this repo, panicking on failure (see
    /// [`run_git`]).
    #[allow(
        clippy::must_use_candidate,
        reason = "Running Git is useful even when its output is ignored"
    )]
    pub fn git(&self, args: &[&str]) -> Output {
        run_git(self.path(), args)
    }

    /// Stages every file currently in the working tree (`git add -A`),
    /// without committing.
    pub fn stage_all(&self) {
        self.git(&["add", "-A"]);
    }

    /// Stages and commits every file currently in the working tree (`git
    /// add -A; git commit -q -m msg`).
    pub fn commit_all(&self, message: &str) {
        self.stage_all();
        self.git(&["commit", "-q", "-m", message]);
    }

    /// Creates (but does not check out) a branch named `name` at `HEAD`.
    pub fn branch(&self, name: &str) {
        self.git(&["branch", name]);
    }

    /// Checks out `reference` (a branch name, tag, or commit-ish).
    pub fn checkout(&self, reference: &str) {
        self.git(&["checkout", "-q", reference]);
    }

    /// This repo's root directory as a `file://` remote URL, for tests
    /// that push/fetch/pull against a real filesystem remote instead of a
    /// network service (`file://` remotes are the default, and only,
    /// integration-test remote backend).
    #[must_use]
    pub fn file_remote_url(&self) -> String {
        file_remote_url(self.path())
    }
}

pub use test_support_git::file_remote_url;

/// Parses `args` (without the leading `argv[0]` program name, which this
/// prepends as a fixed `"gat"`) into a [`gat::cli::Cli`] the same way
/// `main.rs` does, for tests (`app_dispatch.rs`, `lifecycle_consistency.rs`)
/// that call `gat::app::run` in-process with a parsed CLI rather than
/// spawning the compiled binary. Shared here instead of duplicated in
/// each of those files (which, as separate compiled test crates, can't
/// share a private helper directly).
#[must_use]
pub fn parse_cli(args: &[&str]) -> gat::cli::Cli {
    use clap::Parser;
    let mut full = vec!["gat"];
    full.extend_from_slice(args);
    gat::cli::Cli::parse_from(full)
}

/// RAII guard around a spawned child process: kills and reaps it on
/// `Drop` (best-effort, ignoring errors from a process that already
/// exited) so a test that panics -- or an assertion that fails --
/// before explicit cleanup never leaks a lingering child process into
/// the rest of the test run. Every process-boundary test that spawns a
/// helper binary (e.g. `tests/lock_integration.rs`'s `lock-holder`)
/// should hold its child through this guard rather than a bare
/// [`std::process::Child`].
pub struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    /// Spawns `cmd`, wrapping the resulting [`Child`] in a guard.
    /// # Panics
    /// Panics if the child process cannot be started.
    #[must_use]
    pub fn spawn(mut cmd: Command) -> Self {
        let child = cmd.spawn().expect("spawning guarded child process");
        Self { child }
    }

    /// Takes the child's stdin handle (e.g. to close it, signaling the
    /// child to exit gracefully). Returns `None` if already taken or the
    /// child wasn't spawned with a piped stdin.
    pub const fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.stdin.take()
    }

    /// Takes the child's stdout handle (e.g. to read a readiness line
    /// from it via [`read_line_within`]). Returns `None` if already
    /// taken or the child wasn't spawned with a piped stdout.
    pub const fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// Sends a kill signal to the child (see [`Child::kill`]).
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }

    /// Waits for the child to exit, reaping it (see [`Child::wait`]).
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort: a process that already exited (e.g. because the
        // test itself already called `wait`) yields an error here that
        // isn't actionable during a drop, so it's intentionally ignored.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads one line from `reader` on a dedicated thread and waits up to
/// `timeout` for it, returning the line (with any trailing newline
/// stripped) or panicking if the timeout elapses or the read itself
/// fails -- a bounded replacement for an unbounded blocking
/// [`std::io::BufRead::read_line`] call when waiting on a child process's
/// readiness signal. A helper thread is necessary because std's
/// blocking `Read` has no built-in deadline; `reader` must be `'static`
/// so it can be moved onto that thread (e.g. a
/// [`std::process::ChildStdout`] taken out of a [`ChildGuard`], not
/// borrowed from it).
/// # Panics
/// Panics if the read fails, times out, or the reader thread disconnects.
pub fn read_line_within(
    mut reader: impl std::io::BufRead + Send + 'static,
    timeout: Duration,
) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = reader.read_line(&mut line).map(|_| line);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(line)) => line.trim_end().to_string(),
        Ok(Err(e)) => panic!("reading readiness line: {e}"),
        Err(error) => panic!("waiting up to {timeout:?} for a readiness line: {error}"),
    }
}
