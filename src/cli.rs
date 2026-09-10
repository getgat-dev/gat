use clap::{Args, Parser, Subcommand, ValueEnum};
use gat_core::config::ConfigScope;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "gat",
    version,
    about = "Simple, fast, versioned large-file storage for git.",
    max_term_width = 100,
    styles = crate::output::help_styles()
)]
pub struct Cli {
    /// Show all rows and complete details
    #[arg(short = 'o', long, global = true)]
    pub full_output: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Shared `--global`/`--project`/`--local` flags for every command that
/// writes configuration, flattened into each so the flags and their semantics
/// are defined once. Mutually exclusive;
/// `--project` is the default when none is given. The selected location
/// determines where writes go; reads combine all applicable locations.
#[derive(Args, Clone, Copy, Debug, Default)]
pub struct ConfigScopeArgs {
    /// Write to the global config (`~/.gat/gat.yaml`), applying to every
    /// repository for this user.
    #[arg(long, group = "config_scope")]
    global: bool,
    /// Write to the project config (`<repo_root>/gat.yaml`), committed to
    /// git and shared with everyone who clones the repo. The default.
    #[arg(long, group = "config_scope")]
    project: bool,
    /// Write to the local config (`<repo_root>/.gat/gat.yaml`), repo-local
    /// and never committed (`.gat/` isn't tracked).
    #[arg(long, group = "config_scope")]
    local: bool,
}

impl ConfigScopeArgs {
    /// Resolves the flags to a [`ConfigScope`], defaulting to `Project`
    /// when none was passed.
    #[allow(dead_code)] // Used by the binary build.
    #[must_use]
    pub const fn resolve(self) -> ConfigScope {
        if self.global {
            ConfigScope::Global
        } else if self.local {
            ConfigScope::Local
        } else {
            ConfigScope::Project
        }
    }
}

#[cfg(test)]
impl ConfigScopeArgs {
    /// Builds the flags that resolve to `scope`, for tests that construct
    /// scope-carrying actions directly instead of parsing a command line.
    #[allow(dead_code)] // Used by crate tests.
    pub(crate) fn for_scope(scope: ConfigScope) -> Self {
        Self {
            global: matches!(scope, ConfigScope::Global),
            project: matches!(scope, ConfigScope::Project),
            local: matches!(scope, ConfigScope::Local),
        }
    }
}

/// Reusable Git *history selection* flags: which commits does the user
/// mean by "history"? Independent of `gat.lock`/object reachability --
/// currently flattened into `gat gc`/`push`/`fetch`/`pull`/`status`
/// --remote.
/// The resolved semantics are represented by `gat_core::history` and
/// executed by the engine/I/O history capabilities; this struct only
/// holds the raw, unvalidated flag values (kept free of `gix` types so
/// `cli.rs` still compiles standalone).
///
/// Two independent questions, deliberately kept separate:
///
/// - **Scope** -- *which commit(s)* are the roots: `--rev`, `--branches`,
///   `--tags` (freely combinable with each other and with `--rev`), or
///   `--all-history` (every commit-bearing ref of any kind; mutually
///   exclusive with the other three -- it is a broader, distinct root
///   set, not shorthand for combining them).
/// - **Traversal** -- *how far* to walk from those roots: nothing (just
///   the roots themselves, "tips"), `--ancestors` (walk full, unbounded
///   ancestry), or `--depth N` (walk ancestry bounded to `N` commits per
///   root, closest ancestors first). `--ancestors` and `--depth` are
///   mutually exclusive -- `--depth N` alone already means "walk
///   ancestry, bounded to N"; combining both would be redundant/
///   ambiguous about which one wins.
///
/// A scope selector alone selects exactly those commits ("tips") and
/// nothing else -- selecting a revision does not itself mean walking that
/// revision's ancestry. For example, `--rev v1.2.3` alone selects only
/// the commit `v1.2.3` points at; `--rev v1.2.3 --depth 5` selects
/// `v1.2.3` and its 4 closest ancestors.
///
/// `--since`/`--until`/`--first-parent`/`--exclude-rev` are only
/// meaningful while walking ancestry, so each one implies `--ancestors`
/// (unbounded, unless `--depth` is also given). Any traversal-implying
/// flag given *without* an explicit scope selector defaults its root to
/// `HEAD` alone, matching plain `git log`'s behavior -- e.g. `--depth 5`
/// alone selects `HEAD` and its 4 closest ancestors, not every branch tip
/// and tag. `--all-history` is the only scope option that implicitly
/// enables ancestry on its own (unbounded unless `--depth` narrows it);
/// every other scope option (`--rev`/`--branches`/`--tags`) stays `Tips`
/// unless a traversal flag is also given.
#[derive(Args, Clone, Debug, Default)]
pub struct HistoryArgs {
    /// Explicit revision/rev-spec to select (e.g. a commit, tag, or branch
    /// name). Repeatable; each one is resolved strictly and becomes its
    /// own root. Alone (with no other history flag), selects only that
    /// revision itself -- add `--ancestors`/`--depth`/`--since`/`--until`
    /// to also walk its ancestry. Combinable with `--branches`/`--tags`;
    /// mutually exclusive with `--all-history`.
    #[arg(long = "rev", conflicts_with = "all_history")]
    pub(crate) rev: Vec<String>,
    /// Select every branch tip: local (`refs/heads/**`) and remote-tracking
    /// (`refs/remotes/**`), deduplicated by commit id. There is no
    /// separate remote-tracking-only flag -- select a specific
    /// remote-tracking ref with `--rev origin/main`. Alone, selects only
    /// those tip commits themselves, not their ancestry -- combine with
    /// `--ancestors` (or `--depth`) to also walk backwards from each tip.
    /// A branch that *validly* resolves to a non-commit object is
    /// silently skipped rather than erroring; but a branch ref that
    /// *fails* to resolve at all (dangling, corrupt, unreadable) makes
    /// resolution fail closed with an error instead. Combinable with
    /// `--rev`/`--tags`; mutually exclusive with `--all-history`.
    #[arg(long, conflicts_with = "all_history")]
    pub(crate) branches: bool,
    /// Select every tag, peeled to the commit it points at. Alone,
    /// selects only those tagged commits themselves, not their ancestry
    /// -- combine with `--ancestors` (or `--depth`) to also walk
    /// backwards from each tag. A tag that *validly* resolves to a
    /// non-commit object (e.g. a tag pointing at a blob or tree) is
    /// silently skipped rather than erroring -- use an explicit `--rev`
    /// if you need that case to fail loudly; but a tag ref that *fails*
    /// to resolve at all makes resolution fail closed with an error
    /// instead. Combinable with `--rev`/`--branches`; mutually exclusive
    /// with `--all-history`.
    #[arg(long, conflicts_with = "all_history")]
    pub(crate) tags: bool,
    /// Select every commit-bearing ref of any kind (branches, tags, and
    /// anything else, e.g. notes/CI refs) as roots, and walk each one's
    /// full, unbounded ancestry -- the only scope option that implicitly
    /// enables ancestry; every other scope option stays `Tips` unless a
    /// traversal flag is also given. A ref that *validly* resolves to a
    /// non-commit object (e.g. one pointing at a blob or tree) is
    /// silently skipped rather than erroring; but a ref that *fails* to
    /// resolve at all (dangling, corrupt, unreadable) makes resolution
    /// fail closed with an error instead. Combine with `--depth` to cap
    /// the walked depth per root instead of walking unbounded ancestry.
    /// Mutually exclusive with `--rev`/`--branches`/`--tags`: it is a
    /// distinct, broader root set, not a shorthand for combining them.
    #[arg(long = "all-history")]
    pub(crate) all_history: bool,
    /// Walk each selected root's full, unbounded ancestry (closest
    /// ancestors first, not commit-timestamp order) instead of selecting
    /// only the roots themselves. With no scope selector given (`--rev`/
    /// `--branches`/`--tags`/`--all-history`), the root defaults to
    /// `HEAD` alone. Mutually exclusive with `--depth` (which already
    /// implies walking ancestry, bounded).
    #[arg(long, conflicts_with = "depth")]
    pub(crate) ancestors: bool,
    /// Limit ancestry traversal from each selected root to at most this
    /// many *visited* commits (closest ancestors first, not
    /// commit-timestamp order), then union/deduplicate across roots. This
    /// bounds how far the walk goes, not how many commits end up in the
    /// result: combined with `--since`/`--until`, a commit outside the
    /// time window still consumes one unit of the depth budget, so
    /// `--depth 5 --since DATE` can return fewer than 5 commits per root
    /// (even zero) without walking further than 5 ancestors -- it never
    /// means "keep walking until 5 commits match". With no scope
    /// selector given (`--rev`/`--branches`/`--tags`/`--all-history`),
    /// the root defaults to `HEAD` alone -- `--depth 5` alone visits
    /// `HEAD` and its 4 closest ancestors, not 5 commits from every
    /// branch/tag. `--branches --depth 5` means up to 5 commits visited
    /// from *each* branch tip, not 5 total. `--all-history --depth 5`
    /// keeps `--all-history`'s root set (every commit-bearing ref) and
    /// only bounds that per-root walk to 5 commits each. Mutually
    /// exclusive with `--ancestors`.
    #[arg(long)]
    pub(crate) depth: Option<std::num::NonZeroUsize>,
    /// Only include commits committed at/after this date. Accepts
    /// Git-style/ISO dates (e.g. `2024-01-01`, `2 weeks ago`). Implies
    /// walking ancestry from the selected roots to find commits in the
    /// time window, even without `--ancestors`/`--depth`; with no scope
    /// selector given, the root defaults to `HEAD` alone.
    #[arg(long)]
    pub(crate) since: Option<String>,
    /// Only include commits committed at/before this date. Accepts
    /// Git-style/ISO dates (e.g. `2024-01-01`, `2 weeks ago`). Implies
    /// walking ancestry from the selected roots to find commits in the
    /// time window, even without `--ancestors`/`--depth`; with no scope
    /// selector given, the root defaults to `HEAD` alone.
    #[arg(long)]
    pub(crate) until: Option<String>,
    /// Follow only the first parent of each commit instead of every
    /// parent (excludes commits only reachable via a merged-in branch).
    /// Only meaningful while walking ancestry -- implies walking it, even
    /// without `--ancestors`/`--depth`/`--since`/`--until`; with no scope
    /// selector given, the root defaults to `HEAD` alone (not every ref).
    #[arg(long)]
    pub(crate) first_parent: bool,
    /// Exclude this revision and its ancestors from the selection.
    /// Repeatable. Only meaningful while walking ancestry -- implies
    /// walking it, even without `--ancestors`/`--depth`/`--since`/
    /// `--until`; with no scope selector given, the root defaults to
    /// `HEAD` alone (not every ref).
    #[arg(long = "exclude-rev")]
    pub(crate) exclude_rev: Vec<String>,
}

/// The one shared path-selection CLI definition: `--path`, `--include`,
/// and `--exclude`, flattened into every selection-aware command
/// (`status`, `ls-files`, `diff`, `push`, `fetch`, `pull`, `sync`, and
/// `mount add`) so the three flags -- and their semantics -- are defined
/// once instead of once per command. Resolved into the single
/// `gat_core::selection::Selection` domain type below the CLI boundary
/// (that conversion lives in `app`, kept out of this
/// file so `cli.rs` still compiles standalone).
#[derive(Args, Clone, Debug, Default)]
pub struct SelectionArgs {
    /// Select this file or subtree. Any explicit --path, --include, or
    /// --exclude replaces the complete configured selection.
    /// Use --path . for the whole repository. For mount add, omission selects
    /// the source root; repository selection defaults never apply to mounts.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Glob pattern (matched relative to `--path`) selecting which paths to
    /// include. Repeatable. Component-aware: `*.bin` matches only immediate
    /// children, while `**/*.bin` matches recursively. Empty means
    /// everything under `--path`.
    #[arg(long)]
    include: Vec<String>,
    /// Glob pattern (matched relative to `--path`) excluded from `--include`
    /// (or from everything under `--path`, if `--include` is empty).
    /// Repeatable. Component-aware like `--include`; exclude always wins
    /// over include.
    #[arg(long)]
    exclude: Vec<String>,
}

/// A repository operation uses either one named selection or inline selectors.
#[derive(Args, Clone, Debug, Default)]
pub struct RepositorySelectionArgs {
    /// Use this saved selection for this operation without changing the default.
    #[arg(long = "selection", conflicts_with_all = ["path", "include", "exclude"])]
    pub(crate) name: Option<String>,
    #[command(flatten)]
    pub(crate) inline: SelectionArgs,
}

impl std::ops::Deref for RepositorySelectionArgs {
    type Target = SelectionArgs;
    fn deref(&self) -> &Self::Target {
        &self.inline
    }
}

impl SelectionArgs {
    /// Matching base for explicit CLI selection or mount-source selection.
    /// Call `is_explicit` first when omission should use repository defaults.
    #[allow(dead_code)] // Used by the binary build.
    pub(crate) fn path_arg(&self) -> &std::path::Path {
        self.path
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."))
    }

    #[allow(dead_code)] // Used by the binary build.
    pub(crate) const fn is_explicit(&self) -> bool {
        self.path.is_some() || !self.include.is_empty() || !self.exclude.is_empty()
    }

    /// The raw `--include` globs.
    #[allow(dead_code)] // Used by the binary build.
    pub(crate) fn include_globs(&self) -> &[String] {
        &self.include
    }

    /// The raw `--exclude` globs.
    #[allow(dead_code)] // Used by the binary build.
    pub(crate) fn exclude_globs(&self) -> &[String] {
        &self.exclude
    }
}

#[cfg(test)]
impl SelectionArgs {
    /// Builds `SelectionArgs` directly, for tests that construct
    /// selection-carrying actions instead of parsing a command line.
    #[allow(dead_code)] // Used by crate tests.
    pub(crate) fn new(path: PathBuf, include: Vec<String>, exclude: Vec<String>) -> Self {
        Self {
            path: Some(path),
            include,
            exclude,
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Declaratively converge gat's repository-local Git integration to
    /// the requested state (and set up the local cache dir, unaffected by
    /// either flag): without `--no-hooks`/`--no-merge-driver`, gat's Git
    /// hooks (`post-checkout`, `post-merge`, `post-rewrite`, so `gat
    /// sync` runs automatically after checkout, merge/pull, and
    /// rebase/amend) and the `gat.lock` semantic merge driver
    /// (`merge.gat-lock.*` Git config plus an `info/attributes` entry, so
    /// Git merges/rebases resolve independent `gat.lock` changes
    /// automatically instead of reporting false conflicts) are ensured
    /// present -- installed if missing, left alone if already installed.
    /// With `--no-hooks` and/or `--no-merge-driver`, the corresponding
    /// integration is ensured absent instead -- actively removed if
    /// present, left alone if already absent. Re-running `gat init` at
    /// any time re-converges to whatever flags are passed, so toggling a
    /// flag on a later run installs or removes integration accordingly.
    /// Also scaffolds an example `gat.yaml` (every key commented out) if
    /// `--example-config` is passed and the project doesn't have one yet;
    /// an existing `gat.yaml` is always left untouched. Without
    /// `--example-config`, `gat init` never touches `gat.yaml`.
    #[command(
        about = "Initialize Gat with hooks, merge support, and a local cache.",
        long_about
    )]
    Init {
        /// Ensure gat's Git hooks are absent (removing gat's managed
        /// `post-checkout`/`post-merge`/`post-rewrite` blocks if
        /// present) instead of ensuring them installed. The merge driver
        /// and attributes (unless `--no-merge-driver` is also passed)
        /// still converge to installed, and the local cache dir is still
        /// set up.
        #[arg(long)]
        no_hooks: bool,

        /// Ensure the `gat.lock` semantic merge driver is absent
        /// (removing the `merge.gat-lock.*` Git config and gat's managed
        /// `info/attributes` block if present) instead of ensuring it
        /// installed. Hooks (unless `--no-hooks` is also passed) and the
        /// local cache dir still converge to installed/set up.
        #[arg(long)]
        no_merge_driver: bool,

        /// Also write an example `gat.yaml` (every key commented out) if
        /// the project doesn't have one yet. An already-present
        /// `gat.yaml` is always left completely untouched; this only
        /// opts into creating a fresh one when none exists.
        #[arg(long)]
        example_config: bool,
    },

    /// Start tracking a file, directory, or glob pattern with gat: hashes
    /// the content into the local cache, records a row in `gat.lock`, and
    /// excludes the real path via `.git/info/exclude`. Refuses a path
    /// already tracked by plain git, and any path matching `.gatignore`
    /// (a gat-only, `.gitignore`-syntax exclude file); directory and glob
    /// arguments report skipped `.gatignore` members instead. A new,
    /// untracked path also covered by `.gitignore` / `.git/info/exclude`
    /// / global excludes is likewise skipped (or refused, if named
    /// explicitly) unless it's already gat-tracked -- already-tracked
    /// paths stay addable even though gat's own managed excludes make
    /// them git-ignored. `--force` follows git's own style: normal
    /// argument selection determines what's in scope (an explicit path,
    /// a whole directory, or a glob's matches), and `--force` disables
    /// ignore filtering *within* that scope rather than widening it, so
    /// an explicitly-named ignored path, or an ignored file that would
    /// otherwise be skipped inside a directory or glob's scope,
    /// is included instead. `--force` never bypasses the plain-git-tracked
    /// or root `.git`, `.gat`, `gat.lock`, and `gat.yaml` infrastructure refusals, repository/mount
    /// ownership confinement, or the symlink/non-regular-file
    /// restriction.
    #[command(
        about = "Track files and directories and store them in the local cache.",
        long_about
    )]
    Add {
        /// Files, directories, or glob patterns to start tracking. `*.bin`
        /// matches only the current directory; `**/*.bin` matches
        /// recursively.
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Disable `.gatignore`/Git-ignore (`.gitignore`/`.git/info/exclude`
        /// /global excludes) filtering within the selection scope: an
        /// explicitly-named ignored path, or an ignored file that a
        /// directory or glob argument would otherwise skip, is
        /// included instead. Does not widen the selection scope itself,
        /// and never bypasses the plain-git-tracked or gat/git
        /// infrastructure-path refusals.
        #[arg(short, long)]
        force: bool,
    },

    /// Stop tracking path(s) (files, directories recursively, or glob
    /// patterns matched against `gat.lock` itself): removes the row(s)
    /// from `gat.lock`, un-excludes the real path, and deletes the
    /// working-tree file(s) — like `git rm`. Pass `--cached` to keep the
    /// file(s) on disk and only drop them from `gat.lock`. Unlike `gat
    /// add`'s glob support, matching is purely against `gat.lock`'s rows,
    /// not the working tree, so a pattern removes the rows it names
    /// whether or not the underlying files still exist on disk.
    #[command(
        about = "Stop tracking files or directories, optionally keeping local files.",
        long_about
    )]
    Rm {
        /// Tracked files, directories, or glob patterns to stop tracking.
        /// `*.bin` matches only the current directory; `**/*.bin` matches
        /// recursively.
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Keep the file(s) on disk; only remove them from `gat.lock`.
        #[arg(long)]
        cached: bool,
    },

    /// Manage named storage endpoints with `list`, `add`, `update`,
    /// `remove`, and `show`, like `gat route` and `gat mount`.
    /// Choose the optional default with `gat remote default <name>`.
    /// Adding a remote never chooses a default. Mutations write the project
    /// scope unless `--global` or `--local` is selected. URL templates
    /// may contain `${VAR}` references expanded immediately before use.
    #[command(
        about = "Configure remote storage including local file remotes.",
        long_about
    )]
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },

    /// Manage named path selections and the default selection.
    #[command(
        about = "Manage named path selections and the default selection.",
        long_about
    )]
    Selection {
        #[command(subcommand)]
        action: SelectionAction,
    },

    /// Get or set a `gat.yaml` setting, git-config style: `gat config
    /// <key>` prints the current (or default) value, `gat config <key>
    /// <value>` sets a scalar key, `gat config <key> <value> [<value>...]`
    /// sets a list-valued key (one positional argument per element -- no
    /// `,`-delimited values). Supported keys: `cache.location`,
    /// `cache.materialization_strategy`, `cache.ingest_strategy`,
    /// `sync.trust_state`, `sync.auto_fetch`,
    /// `sync.auto_repair`,
    /// `lock.shard_levels`, `git.ignore_patterns`. `gat config <key>` (no
    /// value) shows the effective value merged from every location in a
    /// human-readable report, with one value per line and its source below.
    /// List elements are never comma-joined. `gat config <key>
    /// <value>...` writes to the project `gat.yaml` by default -- pass
    /// `--global` (`~/.gat/gat.yaml`) or `--local`
    /// (`<repo_root>/.gat/gat.yaml`) to write elsewhere instead.
    /// `--clear` persists an explicit empty list for a list-valued key
    /// (invalid for a key like `cache.materialization_strategy` whose empty value is itself
    /// invalid); `--unset` removes the key from the selected scope.
    /// Omitted settings inherit from a lower-priority layer or use their built-in default.
    /// Manage resources through gat remote, gat mount, gat route, and gat selection.
    #[command(
        about = "View or update configuration settings in `gat.yaml`.",
        long_about
    )]
    Config {
        /// The `gat.yaml` key to get or set, e.g. `cache.materialization_strategy`.
        key: String,
        /// New value(s) to set: one for a scalar key, or one positional
        /// argument per element for a list-valued key. Omit (and omit
        /// `--clear`/`--unset`) to print the current (or default) value.
        values: Vec<String>,

        /// Persist an explicit empty list in the selected scope (list-valued
        /// keys only). Mutually exclusive with `--unset` and with passing values.
        #[arg(long, conflicts_with_all = ["unset", "values"])]
        clear: bool,
        /// Remove the key from the selected scope and inherit from lower layers
        /// or use its built-in default.
        /// Mutually exclusive with `--clear` and with passing values.
        #[arg(long, conflicts_with_all = ["clear", "values"])]
        unset: bool,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },

    /// Show tracked files: whether their content is fetched locally, and
    /// whether their current lock entries differ from the staged Gat lock.
    /// Payload file contents are not inspected. Run `gat add` to record edits.
    /// Path scopes are matched lexically against
    /// tracked paths: `./data` and `data/` mean the same thing;
    /// an exact file shows just that file, and a directory shows every
    /// tracked path beneath it. Selection comes from tracked state rather
    /// than disk existence, so a tracked path still matches even if its
    /// working-tree file is gone.
    ///
    /// With `--remote`, checks a Gat storage remote instead: which unique
    /// required objects (deduplicated by content) are missing from it.
    /// This is a presence check for the selected object set, not a remote
    /// bucket inventory -- it never lists/scans the remote. With no
    /// history flag, selects from the current on-disk desired state (same
    /// as `push`/`fetch`, so `gat add x; gat status --remote` sees `x`
    /// immediately). With a history flag, inspects only the committed
    /// `gat.lock` state that history selects, without unioning in the
    /// current working tree. History flags are rejected without
    /// `--remote`, since local status has no history concept.
    #[command(about = "Check file status locally or on remote storage.", long_about)]
    Status {
        #[command(flatten)]
        selection: RepositorySelectionArgs,

        /// Check remote object presence instead of local status. Bare
        /// `--remote` routes each selected path independently -- explicit
        /// override, then most-specific route, then repository default --
        /// same as `push`/`fetch`; `--remote <name>` ignores routing and
        /// checks every selected path against that one named remote.
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        remote: Option<String>,

        #[command(flatten)]
        history: HistoryArgs,
    },

    /// Show a semantic, revision-to-revision diff of gat-tracked files:
    /// which tracked paths were added, removed, or changed size/content
    /// between two `gat.lock` revisions, instead of `gat.lock`'s raw
    /// (large, hash-only) text diff. With no revisions, compares `HEAD`
    /// against the current working tree; with one revision, compares it
    /// against the current working tree; with two, compares them directly
    /// against each other. Every revision is resolved via `gix` the same
    /// way `git rev-parse` would (a branch, tag, `HEAD~2`, a commit sha,
    /// ...). Path scopes use the same lexical exact-file / directory-prefix
    /// matching as `gat status`; a scope with no tracked matches simply shows
    /// no changes.
    #[command(
        about = "Compare files across revisions or against the working tree.",
        long_about
    )]
    Diff {
        /// First revision to compare. Defaults to `HEAD`. When only this
        /// is given, compares it against the current working tree.
        rev1: Option<String>,
        /// Second revision to compare `rev1` against. Defaults to the
        /// current (unstaged) working tree when omitted.
        rev2: Option<String>,

        #[command(flatten)]
        selection: RepositorySelectionArgs,
    },

    /// List all gat-tracked files. Path scopes use the same lexical exact-file
    /// / directory-prefix matching as `gat status`, against the current
    /// on-disk `gat.lock` regardless of whether the working-tree file still
    /// exists.
    #[command(
        name = "ls-files",
        about = "List files with optional path and glob filters.",
        long_about
    )]
    LsFiles {
        #[command(flatten)]
        selection: RepositorySelectionArgs,
    },

    /// Upload objects for tracked files to the remote. With no history flag
    /// (the default), path scopes use the same lexical exact-file /
    /// directory-prefix matching as `gat status`, against the current
    /// on-disk `gat.lock` (whatever `gat add`/`gat rm` last wrote), whether
    /// or not it's been `git add`ed or committed. Never scans for
    /// arbitrary unadded files.
    ///
    /// With a history flag (see `gat gc`'s flags, reused here verbatim),
    /// uploads only the unique objects required by that committed
    /// historical `gat.lock` selection instead -- current on-disk state is
    /// never implicitly unioned into an explicit history selection. `PATH`
    /// filters historical entries before deduplicating by content, so the
    /// same content tracked at multiple paths/revisions is only checked/
    /// uploaded once either way. If this repository is a shallow clone,
    /// history-selected push proceeds on the locally knowable selection
    /// and prints a caution instead of failing.
    #[command(about = "Upload tracked objects to remote storage.", long_about)]
    Push {
        #[command(flatten)]
        selection: RepositorySelectionArgs,
        /// Remote to push to. Overrides path routing; when omitted, routes
        /// choose storage by path, with the configured default remote as fallback.
        #[arg(long)]
        remote: Option<String>,

        #[command(flatten)]
        history: HistoryArgs,
    },

    /// Download objects for tracked files from the remote into the local
    /// cache. With no history flag (the default), path scopes use the same
    /// lexical exact-file / directory-prefix matching as `gat status`,
    /// against the current on-disk `gat.lock`.
    ///
    /// With a history flag (see `gat gc`'s flags, reused here verbatim),
    /// downloads only the unique objects required by that committed
    /// historical `gat.lock` selection instead, without changing the
    /// working tree or unioning in current on-disk state. `PATH` filters
    /// historical entries before deduplicating by content. If this
    /// repository is a shallow clone, history-selected fetch proceeds on
    /// the locally knowable selection and prints a caution instead of
    /// failing.
    #[command(
        about = "Download objects from remote storage into the local cache.",
        long_about
    )]
    Fetch {
        #[command(flatten)]
        selection: RepositorySelectionArgs,
        /// Remote to fetch from. Overrides path routing; when omitted, routes
        /// choose storage by path, with the configured default remote as fallback.
        #[arg(long)]
        remote: Option<String>,

        #[command(flatten)]
        history: HistoryArgs,
    },

    /// Fetch, then materialize tracked files in the working tree. With no
    /// history flag (the default), path scopes use the same lexical
    /// exact-file / directory-prefix matching as `gat status`, but
    /// selection is still lock-based: a tracked path remains in scope even
    /// if it is currently missing on disk.
    ///
    /// With a history flag (see `gat gc`'s flags, reused here verbatim),
    /// the fetch step also prefetches the unique objects required by that
    /// committed historical `gat.lock` selection, unioned with the objects
    /// the current on-disk state needs -- so a historical selection can
    /// never make `pull` fail just because the current checked-out object
    /// was outside it. The materialize step is unaffected: it always
    /// reconciles the current on-disk desired state only, never a
    /// historical revision. `PATH` restricts both the current and
    /// historical fetch scopes, and (as always) the materialize scope. Use
    /// `gat fetch` instead if you only want a historical prefetch without
    /// touching the working tree. If this repository is a shallow clone,
    /// history-selected pull proceeds on the locally knowable selection and
    /// prints a caution instead of failing.
    #[command(
        about = "Download objects and materialize files in the working tree.",
        long_about
    )]
    Pull {
        #[command(flatten)]
        selection: RepositorySelectionArgs,
        /// Remote to pull from. Overrides path routing; when omitted, routes
        /// choose storage by path, with the configured default remote as fallback.
        #[arg(long)]
        remote: Option<String>,

        #[command(flatten)]
        history: HistoryArgs,
    },

    /// Delete objects outside the union of selected Git histories and the current
    /// working lock. Additional repositories must be supplied explicitly with
    /// --repository; shared storage users are not discovered automatically.
    /// Failed repository inspection blocks deletion unless --unsafe is supplied.
    /// Remote deletion always requires --unsafe. Start with --dry-run.
    #[command(
        about = "Remove objects from the local cache or remote storage.",
        long_about
    )]
    Gc {
        /// Keep only the current working lock and additional repositories' HEAD locks.
        /// Conflicts with every history selection or traversal flag.
        #[arg(long, conflicts_with_all = ["rev", "branches", "tags", "all_history", "ancestors", "depth", "since", "until", "first_parent", "exclude_rev"])]
        no_history: bool,

        /// Show what would be deleted without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Allow remote deletion and override incomplete repository inspection.
        /// WARNING: unlisted repositories sharing storage are not protected.
        #[arg(long = "unsafe")]
        r#unsafe: bool,
        /// Collect this remote instead of the local cache. Deletion requires --unsafe.
        #[arg(long)]
        remote: Option<String>,

        /// Additional Git repository whose selected history is protected.
        /// Repeat for multiple repositories. Local paths and Git URLs are cloned
        /// into temporary repositories; uncommitted peer changes are not included.
        #[arg(long = "repository", value_name = "LOCATION")]
        repositories: Vec<String>,

        #[command(flatten)]
        history: HistoryArgs,
    },

    /// Reconcile the working tree with the currently checked-out
    /// `gat.lock`: materialize newly tracked files, replace files whose
    /// content changed, restore tracked files that went missing, and
    /// remove files absent from desired state — validating the working tree by
    /// default with a Git-style stat cache that hashes only when metadata
    /// cannot safely prove identity. Pass `--trust-state` to explicitly
    /// trust `gat.lock` plus the materialized state without touching the
    /// working tree. Also reshapes
    /// `gat.lock` on disk to match `lock.shard_levels` if it changed
    /// since the last reshape, same as `add`/`rm`/`mv` do.
    /// Local-cache only (see `gat pull` to fetch first). Runs
    /// automatically via installed Git hooks after checkout/switch,
    /// merge/pull, and rebase/amend; run it directly after Git operations
    /// hooks don't cover (`git reset --hard`, `git restore`, `git
    /// read-tree`, ...). Path scopes use the same lexical exact-file /
    /// directory-prefix matching as `gat status`, across both the desired
    /// `gat.lock` entries and the last materialized sync state; a tracked
    /// file still matches even if it's currently missing on disk.
    /// With no path selectors, use the default named selection, or the whole repository if unset.
    /// Any explicit --path, --include, or --exclude replaces both configured
    /// lists. Use --path . to reconcile the whole repository.
    #[command(
        about = "Reconcile the working tree and materialize files.",
        long_about
    )]
    Sync {
        #[command(flatten)]
        selection: RepositorySelectionArgs,

        /// Overwrite or remove locally modified gat-managed files instead
        /// of leaving them untouched as conflicts.
        #[arg(long)]
        force: bool,

        /// Show what would be materialized, replaced, removed, or flagged
        /// as a conflict/missing object, without changing anything.
        #[arg(long)]
        dry_run: bool,

        /// Trust `gat.lock` plus recorded materialized state without
        /// touching the working tree. This opts into the fast path; by
        /// default `gat sync` validates with Git-style lazy stat-then-hash
        /// fallback.
        #[arg(long)]
        trust_state: bool,

        /// Fetch missing cache objects from the remote before reconciling,
        /// like `gat pull` does, instead of `gat sync`'s normal
        /// local-cache-only behavior. Same as `gat config sync.auto_fetch
        /// true`, for one run.
        #[arg(long)]
        fetch: bool,

        /// Re-fetch and re-materialize any cache object found corrupted
        /// during this sync (a conflict where the cache object gat would
        /// have used is itself bad, not just the working file) from a
        /// remote, instead of only reporting it.
        #[arg(long)]
        repair: bool,

        /// Remote to fetch missing/repair corrupted cache objects from.
        /// Overrides path routing; when omitted, routes choose storage by path,
        /// with the configured default remote as fallback. Only meaningful
        /// with `--fetch`/`--repair` (or `sync.auto_fetch`/
        /// `sync.auto_repair`).
        #[arg(long)]
        remote: Option<String>,

        /// Recreate already-correct managed files using the current
        /// `cache.materialization_strategy`, even though their content is
        /// already up to date. Unlike a plain `gat sync` (which leaves an
        /// already-correct file untouched no matter how
        /// `cache.materialization_strategy` changed), this may rewrite a
        /// large amount of working-tree data: every selected clean file is
        /// recreated. Forces full working-tree validation for this run
        /// (bypassing `sync.trust_state`'s fast path) so a locally
        /// modified file is reported as a conflict instead of silently
        /// overwritten; combine with `--force` to overwrite it anyway.
        /// Combinable with `--dry-run` to preview what would be
        /// recreated.
        #[arg(long)]
        rematerialize: bool,
    },

    /// Inspect, repair, or clean Gat-managed repository internals
    /// at the explicit maintenance boundary: authoritative lock state
    /// (`gat.lock`), the derived repo-local state databases under
    /// `.gat/state/`, the local cache's metadata/temporary artifacts under
    /// `.gat/objects/`, and Gat's managed block in `.git/info/exclude`.
    /// Normal Gat commands stay focused on normal operation and never
    /// opportunistically repair or clean these internals on the user's
    /// behalf. Omit the scope to target every domain (`all`).
    #[command(
        about = "Inspect, repair, or clean internals and local cache state.",
        long_about
    )]
    System {
        #[command(subcommand)]
        action: SystemAction,
    },

    /// Internal: invoked by gat's installed Git hooks
    /// (`post-checkout`/`post-merge`/`post-rewrite`) as `gat hook <name>
    /// "$@"`. Runs the same reconciliation as `gat sync`, but never exits
    /// nonzero for recoverable per-file conditions (local modifications,
    /// missing cache objects) since Git has already completed the
    /// checkout/merge/rewrite by the time the hook runs.
    #[command(hide = true)]
    Hook {
        /// Hook being invoked: `post-checkout`, `post-merge`, or
        /// `post-rewrite`.
        name: String,
        /// Raw arguments Git passed to the hook.
        args: Vec<String>,
    },

    /// Git's custom merge-driver protocol entrypoint for `gat.lock`,
    /// installed by `gat init` as `merge.gat-lock.driver` and invoked by
    /// Git as `gat merge-driver %O %A %B`. Performs a
    /// three-way semantic merge of `gat.lock` (or one `gat.lock/` shard
    /// file) keyed by canonical tracked path instead of Git's default
    /// line-oriented text merge, and writes the clean result back to
    /// `ours`. Exits nonzero, leaving `ours` conflict-marker-free but
    /// unresolved, if the same path changed incompatibly on both sides.
    #[command(hide = true)]
    MergeDriver {
        /// `%O`: the common-ancestor version of the lock file.
        ancestor: PathBuf,
        /// `%A`: our version; also where Git expects the merge result.
        ours: PathBuf,
        /// `%B`: their version.
        theirs: PathBuf,
    },

    /// Rename or move a tracked file or directory: updates the `gat.lock`
    /// row(s), moves the real file(s) on disk, and re-syncs excludes — like
    /// `git mv`, but for gat-tracked paths. Both paths must be within the
    /// repository; moving into an existing directory (`gat mv a dir/`) is
    /// not supported, name the destination file/directory explicitly.
    #[command(about = "Move or rename files and update `gat.lock`.", long_about)]
    Mv {
        /// Tracked file or directory to rename/move.
        src: PathBuf,
        /// New path for `src`. Must name the destination explicitly (not
        /// an existing directory to move into).
        dst: PathBuf,
        /// Replace an existing destination file (and, if it's Gat-tracked,
        /// its `gat.lock`/materialized-state ownership) instead of failing
        /// when `dst` already exists. Does not allow moving into an
        /// existing directory.
        #[arg(long)]
        force: bool,
    },

    /// Show or manage mounts. A mount records that a
    /// target subtree in this repository mirrors `gat.lock` rows
    /// imported from another Git repository (its `URL`),
    /// and it owns every Gat-managed path beneath the mount target.
    /// Each mount has a stable `NAME` (its `mounts:` config key), a
    /// `URL` (the upstream Git repository), and a `TARGET` (the
    /// destination path it owns).
    ///
    /// The mental model is mount = ownership/provenance, route = path
    /// storage policy, remote = storage endpoint -- all three exposed as
    /// top-level nouns (`gat mount add`, `gat route add`, `gat remote
    /// add`), with a shared management vocabulary. Where
    /// the bytes for a target actually live is decided separately by a
    /// named *route* (`gat route`, a `routes.<NAME>` entry with its own
    /// `path`/`remote`); a mount never persists a remote or a route
    /// reference of its own -- mounts and routes meet only through path
    /// resolution. Add/update automatically reuse or import the source default
    /// remote and create a target route when none exists. Use --no-setup to skip
    /// this, or --remote to choose storage explicitly. These resources remain
    /// independent of the mount; the destination default remote is unchanged.
    ///
    /// Only `gat mount` commands may change mount configuration
    /// or mount-owned `gat.lock` rows. Written to `gat.yaml`. Mutating subcommands
    /// write to the project `gat.yaml` by default; pass `--global` or
    /// `--local` to write elsewhere instead.
    #[command(
        about = "Import files from another Git repository by path.",
        long_about
    )]
    Mount {
        #[command(subcommand)]
        action: MountAction,
    },

    /// Show or manage named path-based storage routes: which
    /// named remote serves a given tracked path's object bytes,
    /// independent of who owns that path (see `gat mount`). Mirrors the
    /// same `list`/`add`/`remove` vocabulary as `gat remote`/`gat mount`: `list`
    /// shows every configured route (plus a synthetic `*` row for the
    /// repository default remote), `add <NAME> <REMOTE> <PATH>`
    /// creates a new named route, `update <NAME>` changes an existing
    /// route's `--path`/`--remote` in place, `remove <NAME>` (or `rm`)
    /// drops one, `show <NAME>` reports full detail. A route's `NAME` is
    /// its stable identity (the `routes:` config key); `path` and
    /// `remote` are properties of that route and never affect resolution
    /// precedence -- the most-specific configured `path` always wins,
    /// regardless of name. Written to `gat.yaml`. `add`/`update`/`remove` write to the
    /// project `gat.yaml` by default; pass `--global` or `--local` to
    /// write elsewhere instead.
    #[command(about = "Route files to specific storage remotes by path.", long_about)]
    Route {
        #[command(subcommand)]
        action: RouteAction,
    },
}

#[derive(Subcommand, Clone, Debug)]
pub enum SystemAction {
    /// Read-only inspection of Gat-managed maintenance state. Never writes
    /// or repairs anything; reports actionable findings only.
    #[command(long_about)]
    Inspect(SystemInspectArgs),
    /// Restore invariants deliberately: recover authoritative `gat.lock`
    /// state only via an explicit recovery choice, and rebuild derived
    /// state (`SQLite` mirrors, cache metadata, managed Git excludes) when
    /// needed. Omit the scope to target every domain.
    #[command(long_about)]
    Repair(SystemRepairArgs),
    /// Remove only state proven disposable: completed transaction scratch,
    /// abandoned temporary cache files, stale Gat-managed Git artifacts,
    /// and other cleanup that never makes an authoritative recovery
    /// choice. Omit the scope to target every domain.
    #[command(long_about)]
    Clean(SystemCleanArgs),
}

/// Which `gat system` maintenance domain to target. `all` (or omitting the
/// scope entirely) means every domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SystemScope {
    Lock,
    State,
    Cache,
    Git,
    All,
}

#[derive(Args, Clone, Debug, Default)]
pub struct SystemInspectArgs {
    /// Maintenance domain to inspect (`all` by default).
    #[arg(value_enum)]
    pub scope: Option<SystemScope>,
}

#[derive(Args, Clone, Debug, Default)]
pub struct SystemRepairArgs {
    /// Maintenance domain to repair (`all` by default).
    #[arg(value_enum)]
    pub scope: Option<SystemScope>,
    /// Explicitly restore the validated backup representation left by an
    /// interrupted `gat.lock` reshape (`.gat/lock-reshape/<txn>/backup`)
    /// instead of promoting the staged target. Only meaningful with
    /// `gat system repair lock`.
    #[arg(long, conflicts_with = "promote_staged")]
    pub restore_backup: bool,
    /// Explicitly promote the validated staged `gat.lock` target left by
    /// an interrupted reshape (`.gat/lock-reshape/<txn>/new`) instead of
    /// restoring the backup. Only meaningful with `gat system repair
    /// lock`.
    #[arg(long, conflicts_with = "restore_backup")]
    pub promote_staged: bool,
    /// Which interrupted `gat.lock` reshape transaction id to recover when
    /// more than one exists under `.gat/lock-reshape/`. Only meaningful
    /// with `gat system repair lock`.
    #[arg(long)]
    pub transaction: Option<String>,
}

#[derive(Args, Clone, Debug, Default)]
pub struct SystemCleanArgs {
    /// Maintenance domain to clean (`all` by default).
    #[arg(value_enum)]
    pub scope: Option<SystemScope>,
    /// Also remove unverified `tmp-*` cache ingest scratch files, in
    /// addition to the ordinary proven-safe cleanup `gat system clean`
    /// does by default. Gat cannot tell an abandoned temp file apart from
    /// one an in-progress `add`/`fetch` on this or another process
    /// sharing the cache is still writing, so this is an explicit,
    /// destructive opt-in: it can disrupt another process's in-progress
    /// ingest. Never implied by `all`.
    #[arg(long)]
    pub purge_temporary: bool,
    /// Also empty Gat's content-object fan-out namespace under the local
    /// cache, in addition to the ordinary disposable temporary cache
    /// state `gat system clean` removes by default. All locally cached
    /// objects are removed and may need to be fetched/rebuilt again.
    /// Never implied by `all`; must be opted into explicitly.
    #[arg(long)]
    pub purge_objects: bool,
}

#[derive(Subcommand)]
pub enum SelectionAction {
    /// List effective selections and mark the default.
    #[command(long_about)]
    List,
    /// Show a selection, its filters, and defining scope.
    #[command(long_about)]
    Show {
        /// Saved selection name.
        name: String,
    },
    /// Save a complete selection without changing the default.
    ///
    /// Supply --path, --include, or --exclude. Use --path . to explicitly
    /// select all tracked paths. Filters may match no files yet.
    #[command(
        long_about,
        group(clap::ArgGroup::new("definition")
            .args(["path", "include", "exclude"])
            .required(true)
            .multiple(true)),
        after_help = "Use --path . to save an unrestricted selection (all tracked paths)."
    )]
    Add {
        /// Saved selection name.
        name: String,
        #[command(flatten)]
        selection: SelectionArgs,
        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Update a definition in its existing scope; omitted fields are preserved.
    ///
    /// Supply at least one path, pattern, or clear option.
    #[command(
        long_about,
        group(clap::ArgGroup::new("changes")
            .args(["path", "include", "exclude", "clear_include", "clear_exclude"])
            .required(true)
            .multiple(true))
    )]
    Update {
        /// Saved selection name.
        name: String,
        /// New literal repository-root-relative path.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Replace include patterns, relative to the saved path.
        #[arg(long, conflicts_with = "clear_include")]
        include: Vec<String>,
        /// Replace exclusions, relative to the saved path.
        #[arg(long, conflicts_with = "clear_exclude")]
        exclude: Vec<String>,
        /// Remove all saved include restrictions.
        #[arg(long)]
        clear_include: bool,
        /// Remove all saved exclusions.
        #[arg(long)]
        clear_exclude: bool,
        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Remove this scope's definition; a lower definition may become effective.
    #[command(long_about, alias = "rm")]
    Remove {
        /// Saved selection name.
        name: String,
        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Show or set the default; unset restores inheritance from lower layers.
    #[command(long_about)]
    Default {
        /// Existing selection to choose; omit to show the current choice.
        #[arg(conflicts_with = "unset")]
        name: Option<String>,
        /// Remove this scope's default pointer and restore inheritance.
        #[arg(long)]
        unset: bool,
        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
}

#[derive(Subcommand)]
pub enum RemoteAction {
    /// Show or choose the default remote. Adding a remote never chooses it.
    #[command(long_about)]
    Default {
        /// Existing remote to choose; omit to show the current default.
        #[arg(conflicts_with = "unset")]
        name: Option<String>,
        /// Remove this scope's choice and inherit from lower layers.
        #[arg(long)]
        unset: bool,
        #[command(flatten)]
        scope: ConfigScopeArgs,
    },

    /// List all configured remotes.
    #[command(long_about)]
    List,
    /// Add a named remote.
    #[command(long_about)]
    Add {
        /// Name to add the remote under (`default` is reserved).
        name: String,
        /// Remote URL (`file://`, `s3://`, `azblob://`, `gcs://`, or
        /// `oss://`, subject to enabled build features); may contain
        /// `${VAR}` references, expanded from the environment every time
        /// gat runs (quote the argument so your shell doesn't expand it
        /// first), instead of writing a secret-bearing URL (e.g. one with a
        /// SAS token or embedded credentials) into `gat.yaml`.
        url: String,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Update an existing remote, preserving its stable name.
    /// Omitted flags keep their current values.
    #[command(long_about)]
    Update {
        /// Name of the remote to update.
        name: String,
        /// New URL template; may contain `${VAR}` references (see `add`).
        #[arg(long)]
        url: Option<String>,
        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Remove a named remote.
    #[command(long_about, alias = "rm")]
    Remove {
        /// Name of the remote to remove.
        name: String,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Show a remote's redacted URL, default status, and defining scope.
    /// Reads the effective configuration merged across every scope.
    /// Whole `${VAR}` query references and sanitized Azure endpoints remain
    /// visible. Userinfo, literal query secrets, and fragments are hidden.
    /// Display never reads environment variables or opens a backend.
    #[command(long_about)]
    Show {
        /// Name of the remote to inspect.
        name: String,
    },
}

impl RemoteAction {
    /// The config scope (`--global`/`--project`/`--local`) a mutating
    /// remote action writes to, defaulting to `Project`. Read-only actions
    /// (`list`, `show`) carry no scope flags and use the default.
    #[allow(dead_code)] // Used by the binary build.
    #[must_use]
    pub const fn config_scope(&self) -> ConfigScope {
        match self {
            Self::Default { scope, .. }
            | Self::Add { scope, .. }
            | Self::Remove { scope, .. }
            | Self::Update { scope, .. } => scope.resolve(),
            Self::List | Self::Show { .. } => ConfigScope::Project,
        }
    }
}

#[derive(Subcommand)]
pub enum MountAction {
    /// List all configured mounts.
    #[command(long_about)]
    List,
    /// Add a mount: imports `gat.lock` rows from another Git repository
    /// under `TARGET`, which this mount then owns. `NAME` is the mount's
    /// stable identity (its `mounts:` config key). `URL` is the upstream
    /// Git repository: a local path, or a remote URL (`http://`, `https://`,
    /// `ssh://`, `git://`, `file://`, or scp-like `[user@]host:path`) -- remote URLs
    /// are cloned into a temporary directory for the duration of the
    /// command. `TARGET` is the destination target this mount owns in this
    /// repository; if omitted, Gat derives it from `URL`'s parsed source
    /// repository path -- its repository name, with a terminal `.git`
    /// stripped where applicable. It must not be `.`, must not overlap any
    /// other mount's target, and must not already contain root-owned
    /// `gat.lock` rows. `--path`
    /// selects a subtree inside the source repository (default `.`), and
    /// `--include`/`--exclude` filter within it (relative to `--path`),
    /// before the selected rows are reparented under `TARGET`. Resolves
    /// `--rev`'s `rev_lock` (via `gix`, from the other repository's local
    /// git history). Automatically reuse or import the source default remote
    /// and create a target route unless one already exists. Use --no-setup to
    /// skip storage setup. The destination default remote is unchanged.
    #[command(long_about)]
    Add {
        /// Stable mount identity and `mounts:` config key.
        name: String,
        /// Upstream Git repository this mount pulls from: a local path, or
        /// a remote URL (`http://`, `https://`, `ssh://`, `git://`, `file://`,
        /// or scp-like `[user@]host:path`).
        url: String,
        /// Destination target this mount owns in this repository. Cannot
        /// be `.`, and cannot overlap another mount's target. If omitted,
        /// Gat derives it from `URL`'s parsed source repository path --
        /// its repository name, with a terminal `.git` stripped where
        /// applicable.
        target: Option<PathBuf>,

        #[command(flatten)]
        selection: SelectionArgs,

        /// Git ref (branch, tag, or commit) to track. Defaults to the
        /// other repository's default branch.
        #[arg(long)]
        rev: Option<String>,
        /// Route the target to this remote, resolving destination names first,
        /// then importing or reusing the source repository's matching remote.
        #[arg(long, conflicts_with = "no_setup")]
        remote: Option<String>,
        /// Skip automatic remote and route setup for this operation.
        #[arg(long)]
        no_setup: bool,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Remove a mount by name. Removes its `mounts:` entry in `gat.yaml`
    /// and deletes the `gat.lock` rows it owns (every row under its
    /// target, since mount targets never overlap). Pass `--detach-only` to
    /// keep those rows instead: they become root-owned. Never removes a
    /// `routes:` entry at the mount target -- routes are independent
    /// policy, and nested routes inside a mount target remain valid and
    /// resolve independently of mount ownership.
    #[command(long_about, alias = "rm")]
    Remove {
        /// Name of the mount to remove.
        name: String,

        /// Keep the mount's currently-owned `gat.lock` rows instead of
        /// deleting them; only the `mounts:` config entry is removed.
        #[arg(long)]
        detach_only: bool,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Update an existing mount in place, preserving its stable `NAME`.
    /// Recomputes the upstream snapshot (re-resolving `rev_lock` and
    /// re-importing rows), preserving omitted source and selection fields.
    /// Storage setup runs on each invocation unless --no-setup is supplied.
    /// Replaces the mount's owned
    /// `gat.lock` rows with the freshly imported set. Changing `--target`
    /// moves the mount's placement/ownership but never moves a generic
    /// `routes:` entry -- routes stay where they are configured.
    #[command(long_about)]
    Update {
        /// Name of the mount to update (its stable `mounts:` config key).
        name: String,

        /// New upstream Git repository URL (keeps the current URL if
        /// omitted).
        #[arg(long)]
        url: Option<String>,
        /// New destination target this mount owns (keeps the current
        /// target if omitted). Cannot be `.`, and cannot overlap another
        /// mount's target.
        #[arg(long)]
        target: Option<PathBuf>,
        /// New subtree inside the source repository to pull from (keeps the
        /// current `--path` if omitted).
        #[arg(long)]
        path: Option<PathBuf>,
        /// New Git ref to track (keeps the current `--rev` if omitted).
        #[arg(long)]
        rev: Option<String>,
        /// Route the target to this remote, resolving destination names first,
        /// then importing or reusing the source repository's matching remote.
        #[arg(long, conflicts_with = "no_setup")]
        remote: Option<String>,
        /// Skip automatic remote and route setup for this operation.
        #[arg(long)]
        no_setup: bool,
        /// Replace the mount's `--include` globs (keeps the current ones if
        /// omitted). Repeatable.
        #[arg(long, conflicts_with = "clear_include")]
        include: Vec<String>,
        /// Replace the mount's `--exclude` globs (keeps the current ones if
        /// omitted). Repeatable.
        #[arg(long, conflicts_with = "clear_exclude")]
        exclude: Vec<String>,
        /// Clear the saved include filters, selecting all source paths not excluded.
        #[arg(long)]
        clear_include: bool,
        /// Clear the saved exclude filters.
        #[arg(long)]
        clear_exclude: bool,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Show everything about one configured mount: its upstream URL
    /// (sanitized), source `--path`, tracked target, `--rev`/`rev_lock`,
    /// `--include`/`--exclude`, the count of `gat.lock` rows it owns, the
    /// effective route serving its target -- reported by stable name,
    /// matched path, and remote (or the repository default remote if no
    /// route matches) -- and which config layer defines it. Reads the
    /// effective config merged across every scope.
    #[command(long_about)]
    Show {
        /// Name of the mount to inspect.
        name: String,
    },
}

impl MountAction {
    /// The config scope (`--global`/`--project`/`--local`) a mutating
    /// mount action writes to, defaulting to `Project`. The read-only
    /// `list` action carries no scope flags and uses the default.
    #[allow(dead_code)] // Used by the binary build.
    #[must_use]
    pub const fn config_scope(&self) -> ConfigScope {
        match self {
            Self::Add { scope, .. } | Self::Remove { scope, .. } | Self::Update { scope, .. } => {
                scope.resolve()
            }
            Self::List => ConfigScope::Project,
            Self::Show { .. } => ConfigScope::Project,
        }
    }
}

#[derive(Subcommand)]
pub enum RouteAction {
    /// List all configured routes, plus a synthetic `*` row showing the
    /// repository default remote that applies when no route matches.
    #[command(long_about)]
    List,
    /// Add a new named route: tracked paths at or beneath `PATH` are
    /// served by `REMOTE`, unless a more specific route overrides it.
    /// `REMOTE` must already be a named remote (`gat remote add` it
    /// first). Fails if `NAME` already exists in the selected scope --
    /// use `route update` to change an existing route. A route selects
    /// storage only -- it never changes which mount (if any) owns
    /// `PATH`. `NAME` `*` is reserved for `route list`'s synthetic
    /// default-remote row and cannot be used.
    #[command(long_about)]
    Add {
        /// Stable route identity and `routes:` config key.
        name: String,
        /// Name of an existing remote to route `PATH` to.
        remote: String,
        /// Root-relative path this route applies to (and everything
        /// beneath it, unless overridden by a more specific route).
        path: PathBuf,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Update an existing route in place, preserving its stable `NAME`.
    /// Any flag you omit keeps the route's current value.
    #[command(long_about)]
    Update {
        /// Name of the route to update (its stable `routes:` config
        /// key).
        name: String,
        /// New remote to route to (keeps the current remote if
        /// omitted). Must already be a named remote.
        #[arg(long)]
        remote: Option<String>,
        /// New path this route applies to (keeps the current `path` if
        /// omitted).
        #[arg(long)]
        path: Option<PathBuf>,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Remove a named route. Only removes that route definition from the
    /// selected scope; never touches mount configuration, `gat.lock`
    /// ownership, remotes, or other routes.
    #[command(long_about, alias = "rm")]
    Remove {
        /// Name of the route to remove.
        name: String,

        #[command(flatten)]
        scope: ConfigScopeArgs,
    },
    /// Show everything about one configured route: its name, normalized
    /// path, remote, and which config layer defines it. Reads the
    /// effective config merged across every scope.
    #[command(long_about)]
    Show {
        /// Name of the route to inspect.
        name: String,
    },
}

impl RouteAction {
    /// The config scope (`--global`/`--project`/`--local`) a mutating
    /// route action writes to, defaulting to `Project`. Read-only actions
    /// (`list`, `show`) carry no scope flags and use the default.
    #[allow(dead_code)] // Used by the binary build.
    #[must_use]
    pub const fn config_scope(&self) -> ConfigScope {
        match self {
            Self::Add { scope, .. } | Self::Update { scope, .. } | Self::Remove { scope, .. } => {
                scope.resolve()
            }
            Self::List | Self::Show { .. } => ConfigScope::Project,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        let mut full = vec!["gat"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).unwrap()
    }

    #[test]
    fn saved_selection_mutations_require_explicit_fields_not_just_scope() {
        for action in ["add", "update"] {
            for scope in [None, Some("--local"), Some("--project"), Some("--global")] {
                let mut args = vec!["gat", "selection", action, "runtime"];
                args.extend(scope);
                let error = Cli::try_parse_from(&args).err().expect("missing fields");
                assert_eq!(
                    error.kind(),
                    clap::error::ErrorKind::MissingRequiredArgument
                );
                for fields in [
                    vec!["--path", "."],
                    vec!["--include", "**/*.bin"],
                    vec!["--exclude", "scratch/**"],
                    vec![
                        "--path",
                        "models",
                        "--include",
                        "*.bin",
                        "--exclude",
                        "old.bin",
                    ],
                ] {
                    let mut explicit = args.clone();
                    explicit.extend(fields);
                    assert!(Cli::try_parse_from(explicit).is_ok());
                }
                if action == "update" {
                    for clear in ["--clear-include", "--clear-exclude"] {
                        let mut explicit = args.clone();
                        explicit.push(clear);
                        assert!(Cli::try_parse_from(explicit).is_ok());
                    }
                }
            }
        }
        for args in [
            vec![
                "selection",
                "update",
                "runtime",
                "--include",
                "**",
                "--clear-include",
            ],
            vec![
                "selection",
                "update",
                "runtime",
                "--exclude",
                "**",
                "--clear-exclude",
            ],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("gat").chain(args)).is_err());
        }
    }

    #[test]
    fn saved_selection_requirements_do_not_change_operation_or_mount_defaults() {
        for command in [
            "ls-files", "status", "diff", "push", "fetch", "pull", "sync",
        ] {
            assert!(Cli::try_parse_from(["gat", command]).is_ok());
        }
        assert!(Cli::try_parse_from(["gat", "mount", "add", "assets", "../source"]).is_ok());
        assert!(Cli::try_parse_from(["gat", "mount", "update", "assets"]).is_ok());
        let help = Cli::try_parse_from(["gat", "selection", "add", "--help"])
            .err()
            .expect("help display");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("--path ."));
    }

    #[test]
    fn mount_setup_options_are_mutually_exclusive() {
        for args in [
            vec!["gat", "mount", "add", "assets", "../source"],
            vec!["gat", "mount", "update", "assets"],
        ] {
            let mut no_setup = args.clone();
            no_setup.push("--no-setup");
            assert!(Cli::try_parse_from(&no_setup).is_ok());
            no_setup.extend(["--remote", "archive"]);
            assert!(Cli::try_parse_from(&no_setup).is_err());
            let mut explicit = args;
            explicit.extend(["--remote", "archive"]);
            assert!(Cli::try_parse_from(&explicit).is_ok());
        }
    }

    #[test]
    fn no_history_is_only_available_for_gc() {
        for command in ["push", "fetch", "pull", "status"] {
            assert!(Cli::try_parse_from(["gat", command, "--no-history"]).is_err());
        }
        for args in [
            vec!["gat", "gc", "--no-history", "--dry-run"],
            vec![
                "gat",
                "gc",
                "--remote",
                "origin",
                "--no-history",
                "--dry-run",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn no_history_conflicts_with_every_history_flag() {
        let command = "gc";
        assert!(Cli::try_parse_from(["gat", command, "--no-history"]).is_ok());
        for flags in [
            vec!["--rev", "HEAD"],
            vec!["--branches"],
            vec!["--tags"],
            vec!["--all-history"],
            vec!["--ancestors"],
            vec!["--depth", "1"],
            vec!["--since", "2024-01-01"],
            vec!["--until", "2024-01-01"],
            vec!["--first-parent"],
            vec!["--exclude-rev", "HEAD"],
        ] {
            let mut args = vec!["gat", command, "--no-history"];
            args.extend(flags);
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn top_level_command_descriptions_are_concise_and_canonical() {
        let root = Cli::command();
        let expected = [
            (
                "init",
                "Initialize Gat with hooks, merge support, and a local cache.",
            ),
            (
                "add",
                "Track files and directories and store them in the local cache.",
            ),
            (
                "rm",
                "Stop tracking files or directories, optionally keeping local files.",
            ),
            (
                "remote",
                "Configure remote storage including local file remotes.",
            ),
            (
                "config",
                "View or update configuration settings in `gat.yaml`.",
            ),
            (
                "selection",
                "Manage named path selections and the default selection.",
            ),
            ("status", "Check file status locally or on remote storage."),
            (
                "diff",
                "Compare files across revisions or against the working tree.",
            ),
            (
                "ls-files",
                "List files with optional path and glob filters.",
            ),
            ("push", "Upload tracked objects to remote storage."),
            (
                "fetch",
                "Download objects from remote storage into the local cache.",
            ),
            (
                "pull",
                "Download objects and materialize files in the working tree.",
            ),
            (
                "gc",
                "Remove objects from the local cache or remote storage.",
            ),
            ("sync", "Reconcile the working tree and materialize files."),
            (
                "system",
                "Inspect, repair, or clean internals and local cache state.",
            ),
            ("mv", "Move or rename files and update `gat.lock`."),
            ("mount", "Import files from another Git repository by path."),
            ("route", "Route files to specific storage remotes by path."),
        ];
        assert_eq!(
            root.get_subcommands()
                .filter(|command| !command.is_hide_set())
                .count(),
            18
        );
        for (name, about) in expected {
            assert_eq!(
                root.find_subcommand(name)
                    .and_then(|command| command.get_about())
                    .map(ToString::to_string)
                    .as_deref(),
                Some(about)
            );
        }
    }

    #[test]
    fn every_visible_command_has_short_and_long_help() {
        fn collect_missing_help(
            command: &clap::Command,
            parent_path: &[String],
            visible_count: &mut usize,
            missing: &mut Vec<String>,
        ) {
            for subcommand in command
                .get_subcommands()
                .filter(|subcommand| subcommand.get_name() != "help" && !subcommand.is_hide_set())
            {
                let mut path = parent_path.to_vec();
                path.push(subcommand.get_name().to_owned());
                *visible_count += 1;

                if subcommand.get_about().is_none() {
                    missing.push(format!("gat {} (short help)", path.join(" ")));
                }
                if subcommand.get_long_about().is_none() {
                    missing.push(format!("gat {} (long help)", path.join(" ")));
                }

                collect_missing_help(subcommand, &path, visible_count, missing);
            }
        }

        let root = Cli::command();
        let mut visible_count = 0;
        let mut missing = Vec::new();
        collect_missing_help(&root, &[], &mut visible_count, &mut missing);

        assert_eq!(visible_count, 43);
        assert!(
            missing.is_empty(),
            "visible commands missing help:\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn parses_add_with_multiple_paths() {
        let cli = parse(&["add", "big.bin", "data/"]);
        let Command::Add { paths, force } = cli.command else {
            panic!("expected Add");
        };
        assert_eq!(
            paths,
            vec![PathBuf::from("big.bin"), PathBuf::from("data/")]
        );
        assert!(!force);
    }

    #[test]
    fn parses_add_with_force_flag() {
        let cli = parse(&["add", "--force", "big.bin"]);
        let Command::Add { paths, force } = cli.command else {
            panic!("expected Add");
        };
        assert_eq!(paths, vec![PathBuf::from("big.bin")]);
        assert!(force);
    }

    #[test]
    fn parses_rm() {
        let cli = parse(&["rm", "big.bin"]);
        let Command::Rm { paths, cached } = cli.command else {
            panic!("expected Rm");
        };
        assert_eq!(paths, vec![PathBuf::from("big.bin")]);
        assert!(!cached);
    }

    #[test]
    fn parses_rm_cached_flag() {
        let cli = parse(&["rm", "--cached", "big.bin", "data/"]);
        let Command::Rm { paths, cached } = cli.command else {
            panic!("expected Rm");
        };
        assert_eq!(
            paths,
            vec![PathBuf::from("big.bin"), PathBuf::from("data/")]
        );
        assert!(cached);
    }

    #[test]
    fn parses_remote_with_no_args() {
        assert!(Cli::try_parse_from(["gat", "remote"]).is_err());
    }

    #[test]
    fn parses_remote_list_and_add() {
        let cli = parse(&["remote", "list"]);
        let Command::Remote { action, .. } = cli.command else {
            panic!("expected Remote");
        };
        assert!(matches!(action, RemoteAction::List));

        assert!(Cli::try_parse_from(["gat", "remote", "-v"]).is_err());

        let cli = parse(&["remote", "add", "backup", "s3://bucket/backup"]);
        let Command::Remote { action, .. } = cli.command else {
            panic!("expected Remote");
        };
        let RemoteAction::Add { name, url, .. } = action else {
            panic!("expected Add");
        };
        assert_eq!(name, "backup");
        assert_eq!(url, "s3://bucket/backup");
    }

    #[test]
    fn parses_remote_update_and_show() {
        let cli = parse(&[
            "remote",
            "update",
            "backup",
            "--url",
            "s3://bucket/new",
            "--local",
        ]);
        let Command::Remote {
            action: RemoteAction::Update {
                name, url, scope, ..
            },
        } = cli.command
        else {
            panic!("expected update");
        };
        assert_eq!(name, "backup");
        assert_eq!(url.as_deref(), Some("s3://bucket/new"));
        assert_eq!(scope.resolve(), ConfigScope::Local);
        let cli = parse(&["remote", "show", "backup"]);
        assert!(
            matches!(cli.command, Command::Remote { action: RemoteAction::Show { name } } if name == "backup")
        );
        for args in [
            vec!["remote", "get-url", "backup"],
            vec!["remote", "set-url", "backup", "s3://bucket"],
            vec!["remote", "rename", "backup", "archive"],
            vec!["remote", "show", "backup", "--local"],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("gat").chain(args)).is_err());
        }
    }

    #[test]
    fn parses_remote_remove() {
        let cli = parse(&["remote", "remove", "backup"]);
        let Command::Remote { action, .. } = cli.command else {
            panic!("expected Remote");
        };
        let RemoteAction::Remove { name, .. } = action else {
            panic!("expected Remove");
        };
        assert_eq!(name, "backup");

        let cli = parse(&["remote", "rm", "backup"]);
        let Command::Remote { action, .. } = cli.command else {
            panic!("expected Remote");
        };
        assert!(matches!(action, RemoteAction::Remove { name, .. } if name == "backup"));
    }

    #[test]
    fn parses_config_get_and_set() {
        let cli = parse(&["config", "cache.materialization_strategy"]);
        let Command::Config { key, values, .. } = cli.command else {
            panic!("expected Config");
        };
        assert_eq!(key, "cache.materialization_strategy");
        assert_eq!(values, Vec::<String>::new());

        let cli = parse(&["config", "cache.location", ".gat-cache"]);
        let Command::Config { key, values, .. } = cli.command else {
            panic!("expected Config");
        };
        assert_eq!(key, "cache.location");
        assert_eq!(values, vec![".gat-cache".to_string()]);
    }

    /// A list-valued key accepts one positional argument per element --
    /// no `,`-delimited value.
    #[test]
    fn parses_config_set_with_multiple_positional_values() {
        let cli = parse(&[
            "config",
            "cache.materialization_strategy",
            "reflink",
            "hardlink",
            "copy",
        ]);
        let Command::Config { key, values, .. } = cli.command else {
            panic!("expected Config");
        };
        assert_eq!(key, "cache.materialization_strategy");
        assert_eq!(
            values,
            vec![
                "reflink".to_string(),
                "hardlink".to_string(),
                "copy".to_string(),
            ]
        );
    }

    #[test]
    fn parses_config_clear_and_unset_flags() {
        let cli = parse(&["config", "git.ignore_patterns", "--clear"]);
        let Command::Config { clear, unset, .. } = cli.command else {
            panic!("expected Config");
        };
        assert!(clear);
        assert!(!unset);

        let cli = parse(&["config", "git.ignore_patterns", "--unset"]);
        let Command::Config { clear, unset, .. } = cli.command else {
            panic!("expected Config");
        };
        assert!(!clear);
        assert!(unset);
    }

    #[test]
    fn parses_config_rejects_clear_and_unset_together() {
        let result =
            Cli::try_parse_from(["gat", "config", "git.ignore_patterns", "--clear", "--unset"]);
        assert!(result.is_err());
    }

    /// `--clear`/`--unset` must also conflict with positional values --
    /// otherwise `gat config git.ignore_patterns '*.bin' --clear` could
    /// silently discard the positional value in favor of `Clear`.
    #[test]
    fn parses_config_rejects_clear_or_unset_with_positional_values() {
        let result = Cli::try_parse_from([
            "gat",
            "config",
            "git.ignore_patterns",
            "models/**",
            "--clear",
        ]);
        assert!(result.is_err());

        let result = Cli::try_parse_from([
            "gat",
            "config",
            "git.ignore_patterns",
            "models/**",
            "--unset",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn config_scope_defaults_to_project_when_no_flag_is_given() {
        let cli = parse(&["config", "cache.materialization_strategy"]);
        let Command::Config { scope, .. } = cli.command else {
            panic!("expected Config");
        };
        assert_eq!(scope.resolve(), ConfigScope::Project);
    }

    #[test]
    fn config_scope_flags_select_global_project_and_local() {
        let cli = parse(&["config", "cache.materialization_strategy", "--global"]);
        let Command::Config { scope, .. } = cli.command else {
            panic!("expected Config");
        };
        assert_eq!(scope.resolve(), ConfigScope::Global);

        let cli = parse(&["config", "cache.materialization_strategy", "--project"]);
        let Command::Config { scope, .. } = cli.command else {
            panic!("expected Config");
        };
        assert_eq!(scope.resolve(), ConfigScope::Project);

        let cli = parse(&["config", "cache.materialization_strategy", "--local"]);
        let Command::Config { scope, .. } = cli.command else {
            panic!("expected Config");
        };
        assert_eq!(scope.resolve(), ConfigScope::Local);
    }

    #[test]
    fn config_scope_flags_are_mutually_exclusive() {
        assert!(
            Cli::try_parse_from([
                "gat",
                "config",
                "cache.materialization_strategy",
                "--global",
                "--local"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "gat",
                "config",
                "cache.materialization_strategy",
                "--project",
                "--global"
            ])
            .is_err()
        );
    }

    #[test]
    fn mutating_remote_and_mount_actions_accept_scope_flags() {
        let cli = parse(&["remote", "add", "backup", "s3://bucket", "--global"]);
        let Command::Remote { action, .. } = cli.command else {
            panic!("expected Remote");
        };
        let RemoteAction::Add { scope, .. } = action else {
            panic!("expected Remote::Add");
        };
        assert_eq!(scope.resolve(), ConfigScope::Global);

        let cli = parse(&["mount", "remove", "models", "--local"]);
        let Command::Mount { action } = cli.command else {
            panic!("expected Mount");
        };
        let MountAction::Remove { scope, .. } = action else {
            panic!("expected Mount::Remove");
        };
        assert_eq!(scope.resolve(), ConfigScope::Local);
    }

    #[test]
    fn read_only_remote_and_mount_list_reject_scope_flags() {
        // Read-only `list` actions carry no write-scope flags.
        assert!(Cli::try_parse_from(["gat", "remote", "list", "--global"]).is_err());
        assert!(Cli::try_parse_from(["gat", "mount", "list", "--local"]).is_err());
    }

    #[test]
    fn parses_gc_dry_run_flag() {
        let cli = parse(&["gc"]);
        let Command::Gc {
            dry_run,
            r#unsafe,
            remote,
            ..
        } = cli.command
        else {
            panic!("expected Gc");
        };
        assert!(!dry_run);
        assert!(!r#unsafe);
        assert_eq!(remote, None);

        let cli = parse(&["gc", "--dry-run"]);
        let Command::Gc {
            dry_run,
            r#unsafe,
            remote,
            ..
        } = cli.command
        else {
            panic!("expected Gc");
        };
        assert!(dry_run);
        assert!(!r#unsafe);
        assert_eq!(remote, None);
    }

    #[test]
    fn parses_repeatable_gc_repositories() {
        let cli = parse(&[
            "gc",
            "--repository",
            "../peer",
            "--repository",
            "file:///other",
            "--depth",
            "2",
        ]);
        let Command::Gc { repositories, .. } = cli.command else {
            panic!("expected Gc");
        };
        assert_eq!(repositories, vec!["../peer", "file:///other"]);
    }

    #[test]
    fn parses_gc_remote_flag() {
        let cli = parse(&["gc", "--remote", "backup"]);
        let Command::Gc { remote, .. } = cli.command else {
            panic!("expected Gc");
        };
        assert_eq!(remote.as_deref(), Some("backup"));
    }

    #[test]
    fn parses_gc_unsafe_flag() {
        let cli = parse(&["gc", "--unsafe"]);
        let Command::Gc { r#unsafe, .. } = cli.command else {
            panic!("expected Gc");
        };
        assert!(r#unsafe);
    }

    #[test]
    fn parses_gc_history_flags() {
        let cli = parse(&["gc"]);
        let Command::Gc { history, .. } = cli.command else {
            panic!("expected Gc");
        };
        assert_eq!(history.rev, Vec::<String>::new());
        assert!(!history.branches);
        assert!(!history.tags);
        assert!(!history.all_history);
        assert!(!history.ancestors);
        assert_eq!(history.depth, None);
        assert_eq!(history.since, None);
        assert_eq!(history.until, None);
        assert!(!history.first_parent);
        assert_eq!(history.exclude_rev, Vec::<String>::new());

        let cli = parse(&[
            "gc",
            "--rev",
            "abc123",
            "--rev",
            "def456",
            "--branches",
            "--tags",
            "--depth",
            "5",
            "--since",
            "2024-01-01",
            "--until",
            "2024-12-31",
            "--first-parent",
            "--exclude-rev",
            "badcommit",
        ]);
        let Command::Gc { history, .. } = cli.command else {
            panic!("expected Gc");
        };
        assert_eq!(history.rev, vec!["abc123", "def456"]);
        assert!(history.branches);
        assert!(history.tags);
        assert!(!history.all_history);
        assert_eq!(history.depth, std::num::NonZeroUsize::new(5));
        assert_eq!(history.since.as_deref(), Some("2024-01-01"));
        assert_eq!(history.until.as_deref(), Some("2024-12-31"));
        assert!(history.first_parent);
        assert_eq!(history.exclude_rev, vec!["badcommit"]);
    }

    #[test]
    fn parses_gc_all_history_flag() {
        let cli = parse(&["gc", "--all-history", "--depth", "5"]);
        let Command::Gc { history, .. } = cli.command else {
            panic!("expected Gc");
        };
        assert!(history.all_history);
        assert_eq!(history.depth, std::num::NonZeroUsize::new(5));
    }

    #[test]
    fn parses_simple_subcommands() {
        assert!(matches!(
            parse(&["init"]).command,
            Command::Init {
                no_hooks: false,
                no_merge_driver: false,
                example_config: false
            }
        ));
        assert!(matches!(
            parse(&["init", "--no-hooks"]).command,
            Command::Init {
                no_hooks: true,
                no_merge_driver: false,
                example_config: false
            }
        ));
        assert!(matches!(
            parse(&["init", "--no-merge-driver"]).command,
            Command::Init {
                no_hooks: false,
                no_merge_driver: true,
                example_config: false
            }
        ));
        assert!(matches!(
            parse(&["init", "--example-config"]).command,
            Command::Init {
                no_hooks: false,
                no_merge_driver: false,
                example_config: true
            }
        ));
        assert!(matches!(
            parse(&["status"]).command,
            Command::Status { remote: None, .. }
        ));
        assert!(matches!(
            parse(&["ls-files"]).command,
            Command::LsFiles { .. }
        ));
        assert!(matches!(
            parse(&["push"]).command,
            Command::Push { remote: None, .. }
        ));
        assert!(matches!(
            parse(&["fetch"]).command,
            Command::Fetch { remote: None, .. }
        ));
        assert!(matches!(
            parse(&["pull"]).command,
            Command::Pull { remote: None, .. }
        ));
        assert!(matches!(
            parse(&["sync"]).command,
            Command::Sync {
                force: false,
                dry_run: false,
                trust_state: false,
                fetch: false,
                repair: false,
                remote: None,
                rematerialize: false,
                ..
            }
        ));
    }

    #[test]
    fn parses_sync_include_and_exclude_flags() {
        let cli = parse(&["sync", "--include", "**/*.onnx", "--exclude", "tests/**"]);
        let Command::Sync { selection, .. } = cli.command else {
            panic!("expected Sync");
        };
        assert_eq!(selection.include, vec!["**/*.onnx".to_string()]);
        assert_eq!(selection.exclude, vec!["tests/**".to_string()]);
    }

    #[test]
    fn parses_sync_flags() {
        let cli = parse(&["sync", "--path", "data", "--force", "--dry-run"]);
        let Command::Sync {
            selection,
            force,
            dry_run,
            trust_state,
            ..
        } = cli.command
        else {
            panic!("expected Sync");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data"));
        assert!(force);
        assert!(dry_run);
        assert!(!trust_state);
    }

    #[test]
    fn parses_sync_trust_state_flag() {
        let cli = parse(&["sync", "--trust-state"]);
        let Command::Sync { trust_state, .. } = cli.command else {
            panic!("expected Sync");
        };
        assert!(trust_state);
    }

    #[test]
    fn parses_sync_trust_state_flag_once() {
        let cli = parse(&["sync", "--trust-state"]);
        let Command::Sync { trust_state, .. } = cli.command else {
            panic!("expected Sync");
        };
        assert!(trust_state);
    }

    #[test]
    fn parses_sync_repair_and_remote_flags() {
        let cli = parse(&["sync", "--repair", "--remote", "backup"]);
        let Command::Sync { repair, remote, .. } = cli.command else {
            panic!("expected Sync");
        };
        assert!(repair);
        assert_eq!(remote, Some("backup".to_string()));
    }

    #[test]
    fn parses_sync_fetch_flag() {
        let cli = parse(&["sync", "--fetch"]);
        let Command::Sync { fetch, .. } = cli.command else {
            panic!("expected Sync");
        };
        assert!(fetch);
    }

    #[test]
    fn parses_sync_rematerialize_flag() {
        let cli = parse(&["sync", "--rematerialize"]);
        let Command::Sync { rematerialize, .. } = cli.command else {
            panic!("expected Sync");
        };
        assert!(rematerialize);
    }

    #[test]
    fn parses_sync_rematerialize_with_dry_run_and_force() {
        let cli = parse(&["sync", "--rematerialize", "--dry-run", "--force"]);
        let Command::Sync {
            rematerialize,
            dry_run,
            force,
            ..
        } = cli.command
        else {
            panic!("expected Sync");
        };
        assert!(rematerialize);
        assert!(dry_run);
        assert!(force);
    }

    #[test]
    fn parses_system_scopes_and_flags() {
        let cli = parse(&["system", "inspect"]);
        let Command::System { action } = cli.command else {
            panic!("expected System");
        };
        let SystemAction::Inspect(args) = action else {
            panic!("expected System::Inspect");
        };
        assert_eq!(args.scope, None);

        let cli = parse(&["system", "inspect", "cache"]);
        let Command::System { action } = cli.command else {
            panic!("expected System");
        };
        let SystemAction::Inspect(args) = action else {
            panic!("expected System::Inspect");
        };
        assert_eq!(args.scope, Some(SystemScope::Cache));

        let cli = parse(&[
            "system",
            "repair",
            "lock",
            "--restore-backup",
            "--transaction",
            "txn-1",
        ]);
        let Command::System { action } = cli.command else {
            panic!("expected System");
        };
        let SystemAction::Repair(args) = action else {
            panic!("expected System::Repair");
        };
        assert_eq!(args.scope, Some(SystemScope::Lock));
        assert!(args.restore_backup);
        assert!(!args.promote_staged);
        assert_eq!(args.transaction.as_deref(), Some("txn-1"));

        let cli = parse(&["system", "clean", "all", "--purge-objects"]);
        let Command::System { action } = cli.command else {
            panic!("expected System");
        };
        let SystemAction::Clean(args) = action else {
            panic!("expected System::Clean");
        };
        assert_eq!(args.scope, Some(SystemScope::All));
        assert!(args.purge_objects);
        assert!(!args.purge_temporary);

        let cli = parse(&["system", "clean", "cache", "--purge-temporary"]);
        let Command::System { action } = cli.command else {
            panic!("expected System");
        };
        let SystemAction::Clean(args) = action else {
            panic!("expected System::Clean");
        };
        assert!(args.purge_temporary);
        assert!(!args.purge_objects);
    }

    #[test]
    fn system_repair_choice_flags_conflict() {
        assert!(
            Cli::try_parse_from([
                "gat",
                "system",
                "repair",
                "lock",
                "--restore-backup",
                "--promote-staged",
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_hook_with_args() {
        let cli = parse(&["hook", "post-checkout", "abc", "def", "1"]);
        let Command::Hook { name, args } = cli.command else {
            panic!("expected Hook");
        };
        assert_eq!(name, "post-checkout");
        assert_eq!(args, vec!["abc", "def", "1"]);
    }

    /// `gat hooks` is not a recognized top-level command, while the hidden
    /// `gat hook <name>` dispatcher installed hook scripts invoke remains
    /// parseable.
    #[test]
    fn hooks_is_no_longer_a_recognized_command() {
        assert!(Cli::try_parse_from(["gat", "hooks", "install"]).is_err());
        assert!(Cli::try_parse_from(["gat", "hooks", "uninstall"]).is_err());
    }

    #[test]
    fn checkout_is_no_longer_a_subcommand() {
        assert!(Cli::try_parse_from(["gat", "checkout"]).is_err());
    }

    #[test]
    fn parses_mv() {
        let cli = parse(&["mv", "a.bin", "b.bin"]);
        let Command::Mv { src, dst, force } = cli.command else {
            panic!("expected Mv");
        };
        assert_eq!(src, PathBuf::from("a.bin"));
        assert_eq!(dst, PathBuf::from("b.bin"));
        assert!(!force);
    }

    #[test]
    fn parses_mv_with_force() {
        let cli = parse(&["mv", "--force", "a.bin", "b.bin"]);
        let Command::Mv { src, dst, force } = cli.command else {
            panic!("expected Mv");
        };
        assert_eq!(src, PathBuf::from("a.bin"));
        assert_eq!(dst, PathBuf::from("b.bin"));
        assert!(force);
    }

    #[test]
    fn parses_mount_add_with_all_options() {
        let cli = parse(&[
            "mount",
            "add",
            "resnet50",
            // hygiene-ok: pure CLI-arg string fed straight into the parser under test; never dialed as a real URL.
            "https://github.com/acme/models",
            "releases/resnet50",
            "--rev",
            "main",
            "--include",
            "**/*.onnx",
            "--exclude",
            "tests/**",
        ]);
        let Command::Mount { action } = cli.command else {
            panic!("expected Mount");
        };
        let MountAction::Add {
            name,
            url,
            target,
            selection,
            rev,
            ..
        } = action
        else {
            panic!("expected Mount::Add");
        };
        assert_eq!(name, "resnet50");
        // hygiene-ok: asserting the parser round-tripped the pure CLI-arg string above; never dialed as a real URL.
        assert_eq!(url, "https://github.com/acme/models");
        assert_eq!(target, Some(PathBuf::from("releases/resnet50")));
        assert_eq!(selection.path_arg(), PathBuf::from("."));
        assert_eq!(rev, Some("main".to_string()));
        assert_eq!(selection.include, vec!["**/*.onnx".to_string()]);
        assert_eq!(selection.exclude, vec!["tests/**".to_string()]);
    }

    #[test]
    fn parses_mount_add_with_explicit_path() {
        let cli = parse(&[
            "mount",
            "add",
            "tf",
            "../my_models_repo",
            "models/tf",
            "--path",
            "models",
        ]);
        let Command::Mount { action } = cli.command else {
            panic!("expected Mount");
        };
        let MountAction::Add {
            name,
            url,
            target,
            selection,
            ..
        } = action
        else {
            panic!("expected Mount::Add");
        };
        assert_eq!(name, "tf");
        assert_eq!(url, "../my_models_repo");
        assert_eq!(target, Some(PathBuf::from("models/tf")));
        assert_eq!(selection.path_arg(), PathBuf::from("models"));
    }

    #[test]
    fn parses_mount_add_with_omitted_target() {
        let cli = parse(&[
            "mount",
            "add",
            "models",
            // hygiene-ok: pure CLI-arg string fed straight into the parser under test; never dialed as a real URL.
            "https://github.com/acme/model-assets.git",
        ]);
        let Command::Mount { action } = cli.command else {
            panic!("expected Mount");
        };
        let MountAction::Add {
            name, url, target, ..
        } = action
        else {
            panic!("expected Mount::Add");
        };
        assert_eq!(name, "models");
        // hygiene-ok: asserting the parser round-tripped the pure CLI-arg string above; never dialed as a real URL.
        assert_eq!(url, "https://github.com/acme/model-assets.git");
        assert_eq!(target, None);
    }

    #[test]
    fn parses_mount_list_and_remove() {
        let cli = parse(&["mount", "list"]);
        assert!(matches!(
            cli.command,
            Command::Mount {
                action: MountAction::List,
            }
        ));

        let cli = parse(&["mount", "remove", "models"]);
        let Command::Mount { action } = cli.command else {
            panic!("expected Mount");
        };
        assert!(matches!(action, MountAction::Remove { name, .. } if name == "models"));
    }

    #[test]
    fn parses_route_list() {
        let cli = parse(&["route", "list"]);
        assert!(matches!(
            cli.command,
            Command::Route {
                action: RouteAction::List,
            }
        ));
    }

    #[test]
    fn parses_route_add_with_required_and_optional_args() {
        let cli = parse(&["route", "add", "models", "backup", "vendor/models"]);
        let Command::Route { action } = cli.command else {
            panic!("expected Route");
        };
        let RouteAction::Add {
            name,
            remote,
            path,
            scope,
        } = action
        else {
            panic!("expected Route::Add");
        };
        assert_eq!(name, "models");
        assert_eq!(remote, "backup");
        assert_eq!(path, PathBuf::from("vendor/models"));
        assert_eq!(scope.resolve(), ConfigScope::Project);

        let cli = parse(&[
            "route",
            "add",
            "models",
            "backup",
            "vendor/models",
            "--global",
        ]);
        let Command::Route { action } = cli.command else {
            panic!("expected Route");
        };
        let RouteAction::Add { scope, .. } = action else {
            panic!("expected Route::Add");
        };
        assert_eq!(scope.resolve(), ConfigScope::Global);
    }

    /// `route add` requires a positional `PATH`; without it, parsing
    /// fails rather than silently defaulting to some path.
    #[test]
    fn parses_route_add_requires_path_argument() {
        assert!(Cli::try_parse_from(["gat", "route", "add", "models", "backup"]).is_err());
    }

    #[test]
    fn parses_route_update_with_optional_fields() {
        // Every mutable field may be omitted -- only `NAME` is required.
        let cli = parse(&["route", "update", "models"]);
        let Command::Route { action } = cli.command else {
            panic!("expected Route");
        };
        let RouteAction::Update {
            name, remote, path, ..
        } = action
        else {
            panic!("expected Route::Update");
        };
        assert_eq!(name, "models");
        assert_eq!(remote, None);
        assert_eq!(path, None);

        let cli = parse(&[
            "route",
            "update",
            "models",
            "--remote",
            "cold",
            "--path",
            "vendor/models2",
            "--local",
        ]);
        let Command::Route { action } = cli.command else {
            panic!("expected Route");
        };
        let RouteAction::Update {
            name,
            remote,
            path,
            scope,
        } = action
        else {
            panic!("expected Route::Update");
        };
        assert_eq!(name, "models");
        assert_eq!(remote, Some("cold".to_string()));
        assert_eq!(path, Some(PathBuf::from("vendor/models2")));
        assert_eq!(scope.resolve(), ConfigScope::Local);
    }

    #[test]
    fn parses_route_remove_and_its_rm_alias() {
        let cli = parse(&["route", "remove", "models"]);
        let Command::Route { action } = cli.command else {
            panic!("expected Route");
        };
        assert!(matches!(action, RouteAction::Remove { name, .. } if name == "models"));

        let cli = parse(&["route", "rm", "models"]);
        let Command::Route { action } = cli.command else {
            panic!("expected Route");
        };
        assert!(matches!(action, RouteAction::Remove { name, .. } if name == "models"));
    }

    #[test]
    fn parses_route_show() {
        let cli = parse(&["route", "show", "models"]);
        let Command::Route { action } = cli.command else {
            panic!("expected Route");
        };
        assert!(matches!(action, RouteAction::Show { name } if name == "models"));
    }

    /// Read-only `route` actions (`list`, `show`) carry no write-scope
    /// flags, matching `remote`/`mount`'s read-only actions.
    #[test]
    fn read_only_route_actions_reject_scope_flags() {
        assert!(Cli::try_parse_from(["gat", "route", "list", "--global"]).is_err());
        assert!(Cli::try_parse_from(["gat", "route", "show", "models", "--local"]).is_err());
    }

    /// Mutating `route` actions accept the shared `--global`/`--project`/
    /// `--local` scope flags, exactly like `remote`/`mount`.
    #[test]
    fn mutating_route_actions_accept_scope_flags() {
        let cli = parse(&[
            "route",
            "add",
            "models",
            "backup",
            "vendor/models",
            "--local",
        ]);
        let Command::Route { action } = cli.command else {
            panic!("expected Route");
        };
        let RouteAction::Add { scope, .. } = action else {
            panic!("expected Route::Add");
        };
        assert_eq!(scope.resolve(), ConfigScope::Local);
    }

    #[test]
    fn parses_path_filter_on_status_and_sync() {
        let cli = parse(&["status", "--path", "data"]);
        let Command::Status {
            selection, remote, ..
        } = cli.command
        else {
            panic!("expected Status");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data"));
        assert_eq!(remote, None);

        let cli = parse(&["ls-files", "--path", "data"]);
        let Command::LsFiles { selection } = cli.command else {
            panic!("expected LsFiles");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data"));

        let cli = parse(&["sync", "--path", "data/big.bin"]);
        let Command::Sync { selection, .. } = cli.command else {
            panic!("expected Sync");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data/big.bin"));
    }

    #[test]
    fn selection_include_exclude_parse_on_every_selection_aware_command() {
        // Every selection-aware command flattens the same `SelectionArgs`,
        // so `--path`/`--include`/`--exclude` parse identically on all of
        // them.
        for cmd in [
            "status", "ls-files", "diff", "push", "fetch", "pull", "sync",
        ] {
            let cli = parse(&[
                cmd,
                "--path",
                "data",
                "--include",
                "**/*.onnx",
                "--exclude",
                "tests/**",
            ]);
            let (Command::Status { selection, .. }
            | Command::LsFiles { selection }
            | Command::Diff { selection, .. }
            | Command::Push { selection, .. }
            | Command::Fetch { selection, .. }
            | Command::Pull { selection, .. }
            | Command::Sync { selection, .. }) = cli.command
            else {
                panic!("unexpected command for {cmd}");
            };
            assert_eq!(
                selection.path_arg(),
                PathBuf::from("data"),
                "path for {cmd}"
            );
            assert_eq!(
                selection.include,
                vec!["**/*.onnx".to_string()],
                "include for {cmd}"
            );
            assert_eq!(
                selection.exclude,
                vec!["tests/**".to_string()],
                "exclude for {cmd}"
            );
        }
    }

    #[test]
    fn status_remote_bare_flag_selects_the_default_remote() {
        let cli = parse(&["status", "--remote"]);
        let Command::Status {
            remote, history, ..
        } = cli.command
        else {
            panic!("expected Status");
        };
        assert_eq!(remote, Some(String::new()));
        assert!(history.rev.is_empty());
        assert!(!history.branches);
    }

    #[test]
    fn status_remote_named_flag_parses_the_name_and_shared_history_args() {
        let cli = parse(&["status", "--remote", "backup", "--rev", "HEAD~1"]);
        let Command::Status {
            remote, history, ..
        } = cli.command
        else {
            panic!("expected Status");
        };
        assert_eq!(remote, Some("backup".to_string()));
        assert_eq!(history.rev, vec!["HEAD~1"]);
    }

    #[test]
    fn status_history_flags_without_remote_still_parse_but_should_be_rejected_by_dispatch() {
        // `cli.rs` (parsing) doesn't know about the local-vs-remote status
        // policy -- that rejection lives in `app::run`, exercised in
        // `app.rs`'s own tests. This only proves history flags remain
        // syntactically legal without `--remote`, so the dispatcher (not
        // clap) is the one place enforcing the restriction. Checked via
        // the raw flag values (not `app::resolve_history_selection`, which
        // isn't available when `cli.rs` is compiled standalone by the doc
        // generator).
        let cli = parse(&["status", "--rev", "HEAD~1"]);
        let Command::Status {
            remote, history, ..
        } = cli.command
        else {
            panic!("expected Status");
        };
        assert_eq!(remote, None);
        assert_eq!(history.rev, vec!["HEAD~1"]);
    }

    #[test]
    fn parses_diff_with_zero_one_and_two_revisions_and_a_path_flag() {
        let cli = parse(&["diff"]);
        let Command::Diff {
            rev1,
            rev2,
            selection,
        } = cli.command
        else {
            panic!("expected Diff");
        };
        assert_eq!(rev1, None);
        assert_eq!(rev2, None);
        assert_eq!(selection.path_arg(), PathBuf::from("."));

        let cli = parse(&["diff", "v1"]);
        let Command::Diff { rev1, rev2, .. } = cli.command else {
            panic!("expected Diff");
        };
        assert_eq!(rev1, Some("v1".to_string()));
        assert_eq!(rev2, None);

        let cli = parse(&["diff", "v1", "v2", "--path", "data/big.bin"]);
        let Command::Diff {
            rev1,
            rev2,
            selection,
        } = cli.command
        else {
            panic!("expected Diff");
        };
        assert_eq!(rev1, Some("v1".to_string()));
        assert_eq!(rev2, Some("v2".to_string()));
        assert_eq!(selection.path_arg(), PathBuf::from("data/big.bin"));
    }

    #[test]
    fn parses_remote_flag_on_push_fetch_pull() {
        let cli = parse(&["push", "--path", "data", "--remote", "backup"]);
        let Command::Push {
            selection,
            remote,
            history,
        } = cli.command
        else {
            panic!("expected Push");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data"));
        assert_eq!(remote, Some("backup".to_string()));
        assert!(history.rev.is_empty());

        let cli = parse(&["fetch", "--remote", "backup"]);
        let Command::Fetch {
            selection,
            remote,
            history,
        } = cli.command
        else {
            panic!("expected Fetch");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("."));
        assert_eq!(remote, Some("backup".to_string()));
        assert!(history.rev.is_empty());

        let cli = parse(&["pull", "--path", "data", "--remote", "backup"]);
        let Command::Pull {
            selection,
            remote,
            history,
        } = cli.command
        else {
            panic!("expected Pull");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data"));
        assert_eq!(remote, Some("backup".to_string()));
        assert!(history.rev.is_empty());
    }

    /// `push`/`fetch`/`pull` all flatten the exact same `HistoryArgs` as
    /// `gc`: one representative history flag proves reuse rather
    /// than repeating every `HistoryArgs` field/permutation for each
    /// command (`gc`'s own tests already own that matrix).
    #[test]
    fn push_fetch_pull_reuse_history_args_alongside_path_and_remote() {
        let cli = parse(&[
            "push", "--path", "data", "--remote", "backup", "--rev", "HEAD~1",
        ]);
        let Command::Push {
            selection,
            remote,
            history,
        } = cli.command
        else {
            panic!("expected Push");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data"));
        assert_eq!(remote, Some("backup".to_string()));
        assert_eq!(history.rev, vec!["HEAD~1".to_string()]);

        let cli = parse(&["fetch", "--path", "data", "--rev", "HEAD~1"]);
        let Command::Fetch {
            selection, history, ..
        } = cli.command
        else {
            panic!("expected Fetch");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data"));
        assert_eq!(history.rev, vec!["HEAD~1".to_string()]);

        let cli = parse(&["pull", "--path", "data", "--rev", "HEAD~1"]);
        let Command::Pull {
            selection, history, ..
        } = cli.command
        else {
            panic!("expected Pull");
        };
        assert_eq!(selection.path_arg(), PathBuf::from("data"));
        assert_eq!(history.rev, vec!["HEAD~1".to_string()]);
    }

    #[test]
    fn rejects_unknown_subcommand() {
        assert!(Cli::try_parse_from(["gat", "bogus"]).is_err());
    }
}
