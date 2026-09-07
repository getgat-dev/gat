//! The application boundary between process bootstrap and command
//! orchestration (`gat-command`): takes an already-parsed [`Cli`] and an
//! explicit [`Context`], dispatches to the right command, and returns a
//! structured [`Outcome`] instead of printing anything itself. Keeping
//! this separate from process bootstrap means every command can be
//! exercised end-to-end (dispatch included) without going through real
//! bootstrap (opendal registry init, a Tokio runtime, actual argv), and
//! it's the one place command-selection policy -- which commands sync
//! `.git/info/exclude` first, which suppress routine sync output, how
//! `gat sync`'s validation/fetch defaults are resolved -- lives, instead
//! of being spread across process adapters and individual commands.
//! Rendering an `Outcome` (and mapping process exit codes) is not this
//! module's job: see `output::render` and `exit_result` below, which
//! process adapters call after dispatch instead of this module printing
//! or importing `ui`/`anstream` itself.

use crate::cli::{
    Cli, Command, HistoryArgs, MountAction, RemoteAction, SelectionArgs, SystemAction,
    SystemScope as CliSystemScope,
};
use crate::error::Failure;
use crate::lifecycle::{self, Lifecycle, LifecycleObserve};
use crate::progress::{NoopProgress, ProgressReporter};
use gat_command::{
    AddRequest, DiffRequest, DiffTarget, GcRequest, HookRequest, LsFilesRequest,
    MergeDriverRequest, MountRequest, MoveRequest, PullRequest, RemoteRequest, RemoteStatusRequest,
    RemoveRequest, RouteRequest, StatusRequest, SyncRequest,
};
use gat_core::endpoint::RemoteUrlTemplate;
use gat_core::git::GitRevisionSpec;
use gat_core::globs::GatGlobPattern;
use gat_core::history::{
    HistoryRequest, HistoryRoot, HistorySelection, HistoryTraversal, ParentMode, TimeWindow,
};
use gat_core::name::RemoteName;
use gat_core::path_scope::normalize_path_scope;
use gat_core::selection::Selection;
use gat_engine::{Repository, parse_cli_date};

/// Every dispatch/policy function in this module returns this alias
/// rather than spelling out `Failure` everywhere: every typed subsystem
/// command error propagates through it via `?` (each has its own
/// `From<SubsystemError> for Failure` in `crate::error::map`), while this
/// module's own app-level policy errors ([`AppError`]) construct a typed
/// [`Failure`] directly instead of ever formatting an anonymous error
/// string.
type Result<T> = std::result::Result<T, Failure>;

/// Typed application-level policy errors: cross-flag/cross-command policy
/// decisions `app::run`'s dispatch enforces itself, independent of any
/// subsystem command, kept separately constructible/testable from the
/// full CLI dispatch path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppError {
    /// `gat status` was given a history-selection flag
    /// (`--rev`/`--branches`/`--tags`/`--all-history`/`--ancestors`/
    /// `--depth`/`--since`/`--until`/`--first-parent`/`--exclude-rev`)
    /// without `--remote`: history only has meaning when comparing
    /// against a remote's history, not the local working tree.
    HistoryRequiresRemote,
    /// `gat sync --dry-run --fetch`: dry-run's whole point is "touch
    /// nothing", so an explicit `--fetch` alongside it is a contradiction,
    /// not a stricter request.
    DryRunConflictsWithFetch,
    /// `gat sync --dry-run --repair`, for the same reason.
    DryRunConflictsWithRepair,
}

/// Rejects `gat sync --dry-run` combined with an explicit `--fetch`/
/// `--repair`.
/// Kept as app-level policy (not a `commands::sync` concern) since it's a
/// cross-flag contradiction, not anything about executing a sync;
/// `AppError`'s variants make it independently testable without going
/// through the full CLI dispatch path.
fn reject_dry_run_conflicts(dry_run: bool, fetch: bool, repair: bool) -> Result<()> {
    if dry_run && fetch {
        return Err(AppError::DryRunConflictsWithFetch.into());
    }
    if dry_run && repair {
        return Err(AppError::DryRunConflictsWithRepair.into());
    }
    Ok(())
}

/// Normalize the CLI scope and compile its patterns. Mount-source selection
/// uses this directly; repository commands preserve omission through
/// `resolve_default_selection` until their configuration is available.
fn resolve_selection(args: &SelectionArgs) -> Result<Selection> {
    let scope = normalize_path_scope(args.path_arg())?;
    let include = selection_globs(args.include_globs())?;
    let exclude = selection_globs(args.exclude_globs())?;
    Ok(Selection::from_scope_patterns(scope, include, exclude))
}

fn selection_globs(values: &[String]) -> Result<Vec<GatGlobPattern>> {
    Ok(values
        .iter()
        .map(|value| GatGlobPattern::parse(value))
        .collect::<std::result::Result<_, _>>()?)
}

/// Preserve omission until the command's effective configuration is available.
fn resolve_default_selection(
    repo: &Repository,
    args: &crate::cli::RepositorySelectionArgs,
) -> Result<Option<Selection>> {
    if let Some(name) = &args.name {
        return Ok(Some(gat_command::named_selection(
            repo,
            &name.clone().into(),
        )?));
    }
    args.is_explicit()
        .then(|| resolve_selection(args))
        .transpose()
}

fn resolve_selection_request(
    action: crate::cli::SelectionAction,
) -> Result<gat_command::SelectionRequest> {
    use crate::cli::SelectionAction as A;
    use gat_command::SelectionRequest as R;
    use gat_core::lexical_path::GatSubpath;
    Ok(match action {
        A::List => R::List,
        A::Show { name } => R::Show { name: name.into() },
        A::Remove { name, scope } => R::Remove {
            name: name.into(),
            scope: scope.resolve(),
        },
        A::Default { name, unset, scope } => R::Default {
            name: name.map(Into::into),
            unset,
            scope: scope.resolve(),
        },
        A::Add {
            name,
            selection,
            scope,
        } => R::Add {
            name: name.into(),
            definition: gat_core::config::SelectionConfig {
                path: GatSubpath::normalize(selection.path_arg())?,
                include: Some(selection_globs(selection.include_globs())?),
                exclude: Some(selection_globs(selection.exclude_globs())?),
            },
            scope: scope.resolve(),
        },
        A::Update {
            name,
            path,
            include,
            exclude,
            clear_include,
            clear_exclude,
            scope,
        } => R::Update {
            name: name.into(),
            path: path.map(GatSubpath::normalize).transpose()?,
            include: if clear_include || !include.is_empty() {
                Some(selection_globs(&include)?)
            } else {
                None
            },
            exclude: if clear_exclude || !exclude.is_empty() {
                Some(selection_globs(&exclude)?)
            } else {
                None
            },
            scope: scope.resolve(),
        },
    })
}

fn resolve_remote_request(action: RemoteAction) -> RemoteRequest {
    match action {
        RemoteAction::Default { name, unset, scope } => RemoteRequest::Default {
            name: name.map(Into::into),
            unset,
            scope: scope.resolve(),
        },
        RemoteAction::List => RemoteRequest::List,
        RemoteAction::Add { name, url, scope } => RemoteRequest::Add {
            name: RemoteName::from_string(name),
            url: RemoteUrlTemplate::from_string(url),
            scope: scope.resolve(),
        },
        RemoteAction::Remove { name, scope } => RemoteRequest::Remove {
            name: RemoteName::from_string(name),
            scope: scope.resolve(),
        },
        RemoteAction::Show { name } => RemoteRequest::Show {
            name: RemoteName::from_string(name),
        },
        RemoteAction::Update { name, url, scope } => RemoteRequest::Update {
            name: RemoteName::from_string(name),
            url: url.map(RemoteUrlTemplate::from_string),
            scope: scope.resolve(),
        },
    }
}

fn resolve_system_scope(scope: Option<CliSystemScope>) -> gat_command::SystemScope {
    match scope.unwrap_or(CliSystemScope::All) {
        CliSystemScope::Lock => gat_command::SystemScope::Lock,
        CliSystemScope::State => gat_command::SystemScope::State,
        CliSystemScope::Cache => gat_command::SystemScope::Cache,
        CliSystemScope::Git => gat_command::SystemScope::Git,
        CliSystemScope::All => gat_command::SystemScope::All,
    }
}

fn resolve_system_request(action: SystemAction) -> gat_command::SystemRequest {
    match action {
        SystemAction::Inspect(args) => gat_command::SystemRequest::Inspect {
            scope: resolve_system_scope(args.scope),
        },
        SystemAction::Repair(args) => gat_command::SystemRequest::Repair {
            scope: resolve_system_scope(args.scope),
            transaction: args.transaction,
            recovery: if args.restore_backup {
                Some(gat_command::RecoveryChoice::RestoreBackup)
            } else if args.promote_staged {
                Some(gat_command::RecoveryChoice::PromoteStaged)
            } else {
                None
            },
        },
        SystemAction::Clean(args) => gat_command::SystemRequest::Clean {
            scope: resolve_system_scope(args.scope),
            purge_temporary: args.purge_temporary,
            purge_objects: args.purge_objects,
        },
    }
}

fn resolve_route_request(action: crate::cli::RouteAction) -> Result<RouteRequest> {
    use crate::cli::RouteAction;
    use gat_core::config::normalize_route_path;
    use gat_core::name::{RemoteName, RouteName};

    Ok(match action {
        RouteAction::List => RouteRequest::List,
        RouteAction::Add {
            name,
            remote,
            path,
            scope,
        } => RouteRequest::Add {
            name: RouteName::from_string(name),
            remote: RemoteName::from_string(remote),
            path: normalize_route_path(path)?,
            scope: scope.resolve(),
        },
        RouteAction::Update {
            name,
            remote,
            path,
            scope,
        } => RouteRequest::Update {
            name: RouteName::from_string(name),
            remote: remote.map(RemoteName::from_string),
            path: path.map(normalize_route_path).transpose()?,
            scope: scope.resolve(),
        },
        RouteAction::Remove { name, scope } => RouteRequest::Remove {
            name: RouteName::from_string(name),
            scope: scope.resolve(),
        },
        RouteAction::Show { name } => RouteRequest::Show {
            name: RouteName::from_string(name),
        },
    })
}

fn mount_globs(
    name: &gat_core::name::MountName,
    field: &'static str,
    patterns: &[String],
) -> Result<Vec<gat_core::globs::GatGlobPattern>> {
    patterns
        .iter()
        .map(|pattern| {
            gat_core::globs::GatGlobPattern::parse(pattern).map_err(|source| {
                gat_core::config::ConfigError::InvalidMountGlobPattern {
                    name: name.to_string(),
                    field,
                    pattern: pattern.clone(),
                    source: Box::new(source),
                }
                .into()
            })
        })
        .collect()
}

fn resolve_mount_request(action: MountAction) -> Result<MountRequest> {
    use gat_core::git_location::GitLocationSpec;
    use gat_core::lexical_path::{GatPath, GatSubpath};
    use gat_core::name::MountName;

    Ok(match action {
        MountAction::List => MountRequest::List,
        MountAction::Show { name } => MountRequest::Show {
            name: MountName::from_string(name),
        },
        MountAction::Add {
            name,
            url,
            target,
            selection,
            rev,
            remote,
            no_setup,
            scope,
        } => {
            let name = MountName::from_string(name);
            MountRequest::Add {
                name: name.clone(),
                location: GitLocationSpec::from_string(url),
                target: target.map(GatPath::normalize).transpose()?,
                path: GatSubpath::normalize(selection.path_arg())?,
                revision: rev.map(GitRevisionSpec::from_string),
                remote: remote.map(RemoteName::from_string),
                no_setup,
                include: mount_globs(&name, "include", selection.include_globs())?,
                exclude: mount_globs(&name, "exclude", selection.exclude_globs())?,
                scope: scope.resolve(),
            }
        }
        MountAction::Update {
            name,
            url,
            target,
            path,
            rev,
            remote,
            no_setup,
            include,
            exclude,
            scope,
        } => {
            let name = MountName::from_string(name);
            MountRequest::Update {
                name: name.clone(),
                location: url.map(GitLocationSpec::from_string),
                target: target.map(GatPath::normalize).transpose()?,
                path: path.map(GatSubpath::normalize).transpose()?,
                revision: rev.map(GitRevisionSpec::from_string),
                remote: remote.map(RemoteName::from_string),
                no_setup,
                include: (!include.is_empty())
                    .then(|| mount_globs(&name, "include", &include))
                    .transpose()?,
                exclude: (!exclude.is_empty())
                    .then(|| mount_globs(&name, "exclude", &exclude))
                    .transpose()?,
                scope: scope.resolve(),
            }
        }
        MountAction::Remove {
            name,
            detach_only,
            scope,
        } => MountRequest::Remove {
            name: MountName::from_string(name),
            detach_only,
            scope: scope.resolve(),
        },
    })
}

/// Whether `args` requires walking ancestry rather than just inspecting
/// the selected roots themselves.
///
/// A scope selector (`--rev`, `--branches`, `--tags`) on its own selects
/// exactly those commits -- selecting a revision does not inherently mean
/// selecting all of its ancestors too. Ancestry is only walked when the
/// user explicitly asks for it (`--ancestors`, `--depth`) or when a flag
/// is logically meaningless without walking history: `--since`/`--until`
/// must search backwards through commits to find ones in the time
/// window, `--first-parent` only affects which parent edges an ancestry
/// walk follows, `--exclude-rev` only hides commits *during* a walk (a
/// lone root has no ancestors to hide in the first place), and
/// `--all-history` is a broad scope that always implies walking each
/// ref's full history rather than just listing every ref tip.
const fn history_requires_ancestry(args: &HistoryArgs) -> bool {
    args.ancestors
        || args.depth.is_some()
        || args.since.is_some()
        || args.until.is_some()
        || args.all_history
        || args.first_parent
        || !args.exclude_rev.is_empty()
}

/// Derives the traversal implied by `args`: [`HistoryTraversal::Tips`]
/// for a plain scope selection, [`HistoryTraversal::Ancestors`] when
/// [`history_requires_ancestry`] is true. Kept separate from
/// [`resolve_history_selection`] so the decision itself is directly
/// unit-testable.
const fn history_traversal(args: &HistoryArgs) -> HistoryTraversal {
    if history_requires_ancestry(args) {
        HistoryTraversal::Ancestors {
            per_root: args.depth,
        }
    } else {
        HistoryTraversal::Tips
    }
}

/// Converts the raw CLI [`HistoryArgs`] into a [`HistorySelection`], or
/// `None` if the user gave no history-selection flag at all -- callers
/// should fall back to their own default in that case (e.g. `gc`'s
/// `HistorySelection::conservative_default`). This is the CLI-to-domain
/// conversion boundary: `HistoryArgs` is a CLI-owned type and
/// `HistorySelection` is a Gix-independent domain/engine-facing type
/// (`gat_core::history`), so the conversion lives here rather than
/// as an inherent method on either type.
///
/// Scope mutual exclusivity (`--all-history` vs. `--rev`/`--branches`/
/// `--tags`) and traversal mutual exclusivity (`--ancestors` vs.
/// `--depth`) are enforced by `clap` itself (`conflicts_with` on the
/// `HistoryArgs` fields), so this function can assume both are already
/// validated by the time it runs.
pub(crate) fn resolve_history_selection(args: HistoryArgs) -> Result<Option<HistorySelection>> {
    let untouched = args.rev.is_empty()
        && !args.branches
        && !args.tags
        && !args.all_history
        && !args.ancestors
        && args.depth.is_none()
        && args.since.is_none()
        && args.until.is_none()
        && !args.first_parent
        && args.exclude_rev.is_empty();
    if untouched {
        return Ok(None);
    }

    let traversal = history_traversal(&args);

    let mut roots = Vec::new();
    if args.all_history {
        roots.push(HistoryRoot::AllRefs);
    } else {
        if args.branches {
            roots.push(HistoryRoot::Branches);
        }
        if args.tags {
            roots.push(HistoryRoot::Tags);
        }
        for rev in args.rev {
            roots.push(HistoryRoot::Revision(rev.into()));
        }
    }
    // No scope selector at all (e.g. just `--ancestors`/`--since`):
    // default to the current commit, matching plain `git log`'s
    // behavior.
    if roots.is_empty() {
        roots.push(HistoryRoot::Head);
    }

    let time = TimeWindow {
        since: args.since.as_deref().map(parse_cli_date).transpose()?,
        until: args.until.as_deref().map(parse_cli_date).transpose()?,
    };

    Ok(Some(HistorySelection {
        roots,
        traversal,
        time,
        parents: if args.first_parent {
            ParentMode::First
        } else {
            ParentMode::All
        },
        excluded: args.exclude_rev.into_iter().map(Into::into).collect(),
    }))
}

/// Everything a command needs beyond its own CLI arguments: the
/// discovered repository, and the invocation-scoped [`Lifecycle`] sink
/// that CLI dispatch and individual commands report observed
/// product surfaces into. Constructed once after process bootstrap
/// (opendal, Tokio runtime) and passed down explicitly, rather than
/// commands reaching for process-global state.
pub struct Context {
    pub repo: Repository,
    pub lifecycle: Lifecycle,
}

impl Context {
    #[must_use]
    pub fn new(repo: Repository) -> Self {
        Self {
            repo,
            lifecycle: Lifecycle::new(),
        }
    }
}

/// The result of running a command, structured so a process adapter can
/// render it without `app::run` (or the commands it calls) printing
/// directly.
pub enum Outcome {
    Initialized(gat_command::InitOutcome),
    Added(gat_command::AddOutcome),
    Removed(gat_command::RemoveOutcome),
    Remote(gat_command::RemoteOutcome),
    Selection(gat_command::SelectionOutcome),
    Configured(gat_command::ConfigOutcome),
    Status(gat_command::StatusOutcome),
    RemoteStatus(gat_command::RemoteStatusOutcome),
    Diff(gat_command::DiffOutcome),
    ListedFiles(gat_command::LsFilesOutcome),
    Pushed(gat_command::PushOutcome),
    /// `fetch` (standalone, or the implicit fetch inside `sync`) pulled
    /// `count` objects; rendered as the "sync complete" summary line.
    Fetched {
        scope: gat_command::SelectionScope,
        count: usize,
        /// Whether an explicit historical selection may be incomplete
        /// because the repository is a shallow clone. Always `false` for
        /// no-history fetch and the implicit fetch inside `sync`.
        shallow: bool,
    },
    Pulled(gat_command::SyncOutcome),
    Synced(gat_command::SyncOutcome),
    Hooked(gat_command::SyncOutcome),
    /// A clean `gat merge-driver` run; deliberately silent (see
    /// `render::render`) so a successfully auto-resolved semantic merge
    /// produces no extra output during a Git merge, matching Git's own
    /// merge-driver conventions.
    MergeDriverApplied,
    GarbageCollected(gat_command::GcOutcome),
    Moved(gat_command::MoveOutcome),
    Mount(gat_command::MountOutcome),
    Route(gat_command::RouteOutcome),
    System(gat_command::SystemOutcome),
}

/// Run one parsed command against `context`, dispatching to the right
/// command and returning a `Result<Outcome>` -- structured data, never
/// anything printed. Lifecycle notices are *not* threaded
/// through this return value: the specific match arms below (and the
/// typed commands they call (for example, `gat_command::config`) report which product
/// surface they actually observed directly into `context.lifecycle`
/// as dispatch happens, and the process adapter renders/drains that sink
/// itself (see `output::notices::emit`) after clearing progress but
/// before handling this function's `Result` -- so a *failing* command
/// still carries its lifecycle notices out to the caller (a destructive
/// `gat gc` that errors partway through must still tell the user it's
/// experimental) without `Outcome`'s `Result` growing a side channel.
/// `progress` is the transient progress reporter for this invocation (a
/// real terminal one, or a no-op) -- built once by the process adapter
/// via `output::progress::for_environment` and threaded down to whichever
/// commands can report something meaningful; commands that only need a
/// generic spinner have it started here at the dispatch boundary instead.
pub fn run(cli: Cli, context: &Context, progress: &dyn ProgressReporter) -> Result<Outcome> {
    let repo = &context.repo;
    let lifecycle_sink = &context.lifecycle;

    // No blanket `.git/info/exclude` refresh here: reconciling excludes on
    // every invocation (even read-only ones like `status`/`ls-files`, or
    // ones that don't touch `gat.lock` like `push`/`fetch`/`gc`) would
    // write to `.git/info/exclude` when nothing warrants it, and could
    // fail outright if that file happens to be unwritable. Instead, every
    // command that can actually change `gat.lock` (`add`/`rm`/`mv`) or
    // that reconciles the working tree against it (`sync`/`pull`, and Git
    // hooks via `hook`) refreshes excludes itself, right after saving the
    // lock. That per-command choice is the explicit exclude-sync policy;
    // this dispatcher deliberately does not add a second, blanket one.
    let dispatch = move || -> Result<Outcome> {
        // Commands with a coherent acquisition barrier recover there, under
        // the same lock as config/state capture. System repair must enter
        // unrecovered so it can repair an interrupted reshape first.
        if !matches!(
            cli.command,
            Command::System { .. }
                | Command::Add { .. }
                | Command::Rm { .. }
                | Command::Mv { .. }
                | Command::Push { .. }
                | Command::Fetch { .. }
                | Command::Pull { .. }
                | Command::Sync { .. }
                | Command::Hook { .. }
                | Command::Status {
                    remote: Some(_),
                    ..
                }
        ) {
            repo.mounts().recover_pending(progress)?;
        }
        match cli.command {
            Command::Init {
                no_hooks,
                no_merge_driver,
                example_config,
            } => Ok(Outcome::Initialized(gat_command::init(
                repo,
                gat_command::InitRequest {
                    no_hooks,
                    no_merge_driver,
                    example_config,
                },
            )?)),
            Command::Add { paths, force } => {
                let paths = paths
                    .iter()
                    .map(gat_core::path_scope::normalize_path_scope)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(Outcome::Added(gat_command::add_with_lifecycle_observer(
                    repo,
                    AddRequest { paths, force },
                    progress,
                    &|surface| lifecycle_sink.observe(surface),
                )?))
            }
            Command::Rm { paths, cached } => {
                let paths = paths
                    .iter()
                    .map(gat_core::path_scope::normalize_path_scope)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(Outcome::Removed(gat_command::remove_with_progress(
                    repo,
                    RemoveRequest { paths, cached },
                    progress,
                )?))
            }
            Command::Selection { action } => {
                lifecycle_sink.observe(lifecycle::Surface::Command("selection"));
                Ok(Outcome::Selection(gat_command::saved_selection(
                    repo,
                    resolve_selection_request(action)?,
                )?))
            }
            Command::Remote { action } => Ok(Outcome::Remote(gat_command::remote(
                repo,
                resolve_remote_request(action),
            )?)),
            Command::Config {
                key,
                values,
                clear,
                unset,
                scope,
            } => {
                // Mutually exclusive by construction (`clap`'s
                // `conflicts_with_all` on `--clear`/`--unset`), and a bare
                // `gat config <key>` (no values, no flags) means "get".
                let action = match (clear, unset, values.is_empty()) {
                    (true, _, _) => gat_command::ConfigAction::Clear,
                    (_, true, _) => gat_command::ConfigAction::Unset,
                    (false, false, true) => gat_command::ConfigAction::Get,
                    (false, false, false) => gat_command::ConfigAction::Set(values),
                };
                let request = gat_command::ConfigRequest::from_raw(key, action, scope.resolve())?;
                Ok(Outcome::Configured(
                    gat_command::config_with_lifecycle_observer(repo, request, &|surface| {
                        lifecycle_sink.observe(surface);
                    })?,
                ))
            }
            Command::Status {
                selection,
                remote,
                history,
            } => {
                let history_selection = resolve_history_selection(history)?;
                let selection = resolve_default_selection(repo, &selection)?;
                match remote {
                    None => {
                        if history_selection.is_some() {
                            return Err(AppError::HistoryRequiresRemote.into());
                        }
                        Ok(Outcome::Status(gat_command::status(
                            repo,
                            StatusRequest { selection },
                            progress,
                        )?))
                    }
                    Some(name) => {
                        let remote_name = if name.is_empty() {
                            None
                        } else {
                            Some(RemoteName::from_string(name))
                        };
                        Ok(Outcome::RemoteStatus(gat_command::remote_status(
                            repo,
                            RemoteStatusRequest {
                                selection: selection.as_ref(),
                                remote: remote_name.as_ref(),
                                history: history_selection.as_ref(),
                            },
                            progress,
                        )?))
                    }
                }
            }
            Command::Diff {
                rev1,
                rev2,
                selection,
            } => {
                let from = rev1.map_or_else(
                    || GitRevisionSpec::from("HEAD"),
                    GitRevisionSpec::from_string,
                );
                let to = rev2
                    .map(GitRevisionSpec::from_string)
                    .map_or(DiffTarget::WorkingTree, DiffTarget::Revision);
                let request = DiffRequest {
                    from,
                    to,
                    selection: resolve_default_selection(repo, &selection)?,
                };
                Ok(Outcome::Diff(gat_command::diff(repo, request, progress)?))
            }
            Command::LsFiles { selection } => {
                let request = LsFilesRequest {
                    selection: resolve_default_selection(repo, &selection)?,
                };
                Ok(Outcome::ListedFiles(gat_command::ls_files(
                    repo, request, progress,
                )?))
            }
            Command::Push {
                selection,
                remote,
                history,
            } => {
                let selection = resolve_default_selection(repo, &selection)?;
                let history = resolve_history_selection(history)?;
                let remote = remote.map(RemoteName::from_string);
                let source = match history.as_ref() {
                    Some(history) => gat_command::PushSource::History(history),
                    None => gat_command::PushSource::Current,
                };
                Ok(Outcome::Pushed(gat_command::push(
                    repo,
                    gat_command::PushRequest {
                        selection: selection.as_ref(),
                        remote: remote.as_ref(),
                        source,
                    },
                    progress,
                )?))
            }
            Command::Fetch {
                selection,
                remote,
                history,
            } => {
                let selection = resolve_default_selection(repo, &selection)?;
                let history = resolve_history_selection(history)?;
                let remote = remote.map(RemoteName::from_string);
                let source = match history.as_ref() {
                    Some(history) => gat_command::FetchSource::History(history),
                    None => gat_command::FetchSource::Current,
                };
                let outcome = gat_command::fetch(
                    repo,
                    gat_command::FetchRequest {
                        selection: selection.as_ref(),
                        remote: remote.as_ref(),
                        source,
                    },
                    progress,
                )?;
                Ok(Outcome::Fetched {
                    scope: outcome.scope,
                    count: outcome.fetched,
                    shallow: outcome.shallow,
                })
            }
            Command::Pull {
                selection,
                remote,
                history,
            } => Ok(Outcome::Pulled(gat_command::recover_incomplete(
                gat_command::pull(
                    repo,
                    PullRequest {
                        selection: resolve_default_selection(repo, &selection)?,
                        remote: remote.map(RemoteName::from_string),
                        history: resolve_history_selection(history)?,
                    },
                    progress,
                ),
            )?)),
            Command::Sync {
                selection,
                force,
                dry_run,
                trust_state,
                fetch,
                repair,
                remote,
                rematerialize,
            } => {
                reject_dry_run_conflicts(dry_run, fetch, repair)?;
                Ok(Outcome::Synced(gat_command::recover_incomplete(
                    gat_command::sync(
                        repo,
                        SyncRequest {
                            selection: resolve_default_selection(repo, &selection)?,
                            force,
                            dry_run,
                            trust_state,
                            fetch,
                            repair,
                            remote: remote.map(RemoteName::from_string),
                            rematerialize,
                        },
                        progress,
                    ),
                )?))
            }
            Command::System { action } => Ok(Outcome::System(
                gat_command::system_with_lifecycle_observer(
                    repo,
                    resolve_system_request(action),
                    progress,
                    &|surface| lifecycle_sink.observe(surface),
                )?,
            )),
            Command::Hook { name, args: _ } => {
                if name == "post-rewrite" {
                    let mut discard = Vec::new();
                    let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut discard);
                }
                Ok(Outcome::Hooked(gat_command::recover_incomplete(
                    gat_command::hook(repo, HookRequest, &NoopProgress),
                )?))
            }
            Command::MergeDriver {
                ancestor,
                ours,
                theirs,
            } => {
                gat_command::merge_driver(MergeDriverRequest {
                    ancestor: &ancestor,
                    ours: &ours,
                    theirs: &theirs,
                })?;
                Ok(Outcome::MergeDriverApplied)
            }
            Command::Gc {
                repositories,
                dry_run,
                r#unsafe,
                remote,
                history,
                no_history,
            } => Ok(Outcome::GarbageCollected(
                gat_command::gc_with_lifecycle_observer(
                    repo,
                    GcRequest {
                        repositories: repositories
                            .into_iter()
                            .map(gat_core::git_location::GitLocationSpec::from_string)
                            .collect(),
                        dry_run,
                        unsafe_override: r#unsafe,
                        remote: remote.map(RemoteName::from_string),
                        history: if no_history {
                            HistoryRequest::Disabled
                        } else {
                            resolve_history_selection(history)?
                                .map_or(HistoryRequest::CommandDefault, HistoryRequest::Selected)
                        },
                    },
                    progress,
                    &|surface| lifecycle_sink.observe(surface),
                )?,
            )),
            Command::Mv { src, dst, force } => Ok(Outcome::Moved(gat_command::move_with_progress(
                repo,
                MoveRequest {
                    src: gat_core::lexical_path::GatPath::normalize(&src)?,
                    dst: gat_core::lexical_path::GatPath::normalize(&dst)?,
                    force,
                },
                progress,
            )?)),
            Command::Mount { action } => {
                lifecycle_sink.observe(lifecycle::Surface::Command("mount"));
                Ok(Outcome::Mount(gat_command::mount(
                    repo,
                    resolve_mount_request(action)?,
                    progress,
                )?))
            }
            Command::Route { action } => {
                lifecycle_sink.observe(lifecycle::Surface::Command("route"));
                Ok(Outcome::Route(gat_command::route(
                    repo,
                    resolve_route_request(action)?,
                )?))
            }
        }
    };
    dispatch()
}

/// Map a completed [`Outcome`] to a process-level result: the one place
/// to decide whether an otherwise-successful command (dispatch returned
/// `Ok`) should still make the process exit non-zero -- e.g. a
/// `sync`/`pull`/`hook` that finished but left conflicts/missing objects
/// behind. Kept separate from rendering: this never prints anything.
/// `outcome.completion`'s structured
/// [`gat_command::SyncCompletionStatus::Incomplete`] counts are
/// passed directly to `error::map::app::sync_completion_conflict`, which
/// authors the one Gat-owned summary text at its own rendering boundary
/// -- this function never builds an intermediate `String` itself.
pub fn exit_result(outcome: &Outcome) -> Result<()> {
    match outcome {
        Outcome::Pulled(outcome) | Outcome::Synced(outcome) | Outcome::Hooked(outcome) => {
            if let gat_command::SyncCompletionStatus::Incomplete {
                conflicts,
                missing,
                corrupted,
            } = outcome.completion
            {
                return Err(crate::error::map::app::sync_completion_conflict(
                    conflicts, missing, corrupted,
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::error::ErrorCode;
    use crate::progress::NoopProgress;
    use clap::Parser;
    use test_support::{git_repo_with_initial_commit as test_repo, remote_add_with_default};
    use test_support_git::commit_all;

    mod selection_conversion {
        use super::*;
        use std::path::PathBuf;

        fn args(path: &str, include: &[&str], exclude: &[&str]) -> SelectionArgs {
            SelectionArgs::new(
                PathBuf::from(path),
                include.iter().map(|value| (*value).to_string()).collect(),
                exclude.iter().map(|value| (*value).to_string()).collect(),
            )
        }

        #[test]
        fn all_seven_commands_preserve_omission_and_any_explicit_selector() {
            for command in [
                "sync", "pull", "fetch", "ls-files", "status", "diff", "push",
            ] {
                for selectors in [
                    vec![],
                    vec!["--path", "."],
                    vec!["--path", "models"],
                    vec!["--include", "**/*.onnx"],
                    vec!["--exclude", "tmp/**"],
                ] {
                    let mut argv = vec!["gat", command];
                    argv.extend(selectors.iter().copied());
                    let parsed = Cli::parse_from(argv);
                    let (Command::Sync { selection, .. }
                    | Command::Pull { selection, .. }
                    | Command::Fetch { selection, .. }
                    | Command::LsFiles { selection }
                    | Command::Status { selection, .. }
                    | Command::Diff { selection, .. }
                    | Command::Push { selection, .. }) = parsed.command
                    else {
                        unreachable!()
                    };
                    assert_eq!(
                        resolve_default_selection(
                            &Repository::at(std::env::temp_dir()),
                            &selection
                        )
                        .unwrap()
                        .is_some(),
                        !selectors.is_empty(),
                        "{command} {selectors:?}"
                    );
                }
            }
            let parsed = Cli::parse_from(["gat", "fetch", "--remote", "backup", "--all-history"]);
            let Command::Fetch { selection, .. } = parsed.command else {
                unreachable!()
            };
            assert!(
                resolve_default_selection(&Repository::at(std::env::temp_dir()), &selection)
                    .unwrap()
                    .is_none()
            );
        }

        #[test]
        fn normalizes_scope_and_patterns_once_at_the_application_boundary() {
            let selection = resolve_selection(&args(
                r".\data",
                &[r"nested\**\*.onnx"],
                &[r"nested\skip\**"],
            ))
            .unwrap();

            assert_eq!(
                selection
                    .scope_path()
                    .map(gat_core::lexical_path::GatPath::as_str),
                Some("data")
            );
            assert_eq!(selection.include_globs()[0].as_str(), "nested/**/*.onnx");
            assert!(selection.matches_str("data/nested/model.onnx"));
            assert!(!selection.matches_str("data/nested/skip/model.onnx"));
        }

        #[test]
        fn rejects_the_path_before_attempting_to_parse_globs() {
            let failure = resolve_selection(&args("../escape", &["["], &[])).unwrap_err();
            assert_eq!(failure.diagnostic().subject(), Some("../escape"));
            assert_eq!(
                failure.diagnostic().code(),
                ErrorCode::PathOutsideRepository
            );
        }

        #[test]
        fn preserves_invalid_glob_context_and_source_chain() {
            let failure = resolve_selection(&args(".", &["data/["], &[])).unwrap_err();
            assert_eq!(failure.diagnostic().subject(), Some("data/["));
            assert!(failure.technical_source().is_some());
        }

        #[cfg(unix)]
        #[test]
        fn rejects_non_utf8_scope_without_lossy_conversion() {
            use std::os::unix::ffi::OsStringExt;

            let failure = resolve_selection(&SelectionArgs::new(
                PathBuf::from(std::ffi::OsString::from_vec(vec![0xff])),
                Vec::new(),
                Vec::new(),
            ))
            .unwrap_err();
            assert_eq!(failure.diagnostic().code(), ErrorCode::InvalidPath);
        }
    }

    mod history_selection {
        use super::*;
        use crate::cli::HistoryArgs;
        use gat_core::history::{HistoryRoot, HistoryTraversal, ParentMode};
        use std::num::NonZeroUsize;

        fn args() -> HistoryArgs {
            HistoryArgs::default()
        }

        #[test]
        fn no_history_flags_returns_none() {
            assert!(resolve_history_selection(args()).unwrap().is_none());
        }

        #[test]
        fn rev_alone_selects_exactly_that_revision_via_tips() {
            let selection = resolve_history_selection(HistoryArgs {
                rev: vec!["HEAD~1".to_string()],
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(
                selection.roots,
                vec![HistoryRoot::Revision("HEAD~1".into())]
            );
            assert_eq!(selection.traversal, HistoryTraversal::Tips);
        }

        #[test]
        fn repeated_rev_selects_both_roots_via_tips() {
            let selection = resolve_history_selection(HistoryArgs {
                rev: vec!["HEAD~1".to_string(), "HEAD~2".to_string()],
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(
                selection.roots,
                vec![
                    HistoryRoot::Revision("HEAD~1".into()),
                    HistoryRoot::Revision("HEAD~2".into())
                ]
            );
            assert_eq!(selection.traversal, HistoryTraversal::Tips);
        }

        #[test]
        fn branches_alone_selects_branch_tips_only() {
            let selection = resolve_history_selection(HistoryArgs {
                branches: true,
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Branches]);
            assert_eq!(selection.traversal, HistoryTraversal::Tips);
        }

        #[test]
        fn tags_alone_selects_tagged_commits_only() {
            let selection = resolve_history_selection(HistoryArgs {
                tags: true,
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Tags]);
            assert_eq!(selection.traversal, HistoryTraversal::Tips);
        }

        #[test]
        fn branches_and_tags_selects_both_root_kinds_via_tips() {
            let selection = resolve_history_selection(HistoryArgs {
                branches: true,
                tags: true,
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(
                selection.roots,
                vec![HistoryRoot::Branches, HistoryRoot::Tags]
            );
            assert_eq!(selection.traversal, HistoryTraversal::Tips);
        }

        #[test]
        fn rev_with_depth_walks_ancestors_limited_to_depth() {
            let selection = resolve_history_selection(HistoryArgs {
                rev: vec!["HEAD".to_string()],
                depth: NonZeroUsize::new(3),
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors {
                    per_root: NonZeroUsize::new(3)
                }
            );
        }

        #[test]
        fn branches_with_depth_walks_ancestors_limited_per_root() {
            let selection = resolve_history_selection(HistoryArgs {
                branches: true,
                depth: NonZeroUsize::new(5),
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Branches]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors {
                    per_root: NonZeroUsize::new(5)
                }
            );
        }

        #[test]
        fn tags_with_depth_walks_ancestors_limited_per_root() {
            let selection = resolve_history_selection(HistoryArgs {
                tags: true,
                depth: NonZeroUsize::new(2),
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Tags]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors {
                    per_root: NonZeroUsize::new(2)
                }
            );
        }

        #[test]
        fn rev_with_ancestors_walks_unbounded_ancestry() {
            let selection = resolve_history_selection(HistoryArgs {
                rev: vec!["HEAD".to_string()],
                ancestors: true,
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors { per_root: None }
            );
        }

        #[test]
        fn all_history_selects_every_ref_with_unbounded_ancestry() {
            let selection = resolve_history_selection(HistoryArgs {
                all_history: true,
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::AllRefs]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors { per_root: None }
            );
        }

        /// Deliberately preserved combination: `--all-history --depth N`
        /// means "up to N ancestors from every ref", not a conflict --
        /// `--depth` still narrows whatever roots were selected, exactly
        /// like `--branches --depth N` does.
        #[test]
        fn all_history_with_depth_narrows_ancestry_to_depth_per_root() {
            let selection = resolve_history_selection(HistoryArgs {
                all_history: true,
                depth: NonZeroUsize::new(5),
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::AllRefs]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors {
                    per_root: NonZeroUsize::new(5)
                }
            );
        }

        #[test]
        fn since_alone_defaults_to_head_and_walks_ancestors() {
            let selection = resolve_history_selection(HistoryArgs {
                since: Some("2020-01-01".to_string()),
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Head]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors { per_root: None }
            );
            assert!(selection.time.since.is_some());
        }

        #[test]
        fn until_alone_defaults_to_head_and_walks_ancestors() {
            let selection = resolve_history_selection(HistoryArgs {
                until: Some("2030-01-01".to_string()),
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Head]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors { per_root: None }
            );
            assert!(selection.time.until.is_some());
        }

        /// `--depth` alone (no scope selector) must default its root to
        /// `HEAD`, not to every ref -- only `--all-history` implies that
        /// broader root set.
        #[test]
        fn depth_alone_defaults_to_head_and_bounds_its_ancestry() {
            let selection = resolve_history_selection(HistoryArgs {
                depth: NonZeroUsize::new(5),
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Head]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors {
                    per_root: NonZeroUsize::new(5)
                }
            );
        }

        /// `--ancestors` alone (no scope selector) must default its root
        /// to `HEAD`, not to every ref.
        #[test]
        fn ancestors_alone_defaults_to_head_and_walks_unbounded_ancestry() {
            let selection = resolve_history_selection(HistoryArgs {
                ancestors: true,
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Head]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors { per_root: None }
            );
        }

        #[test]
        fn rev_with_since_walks_ancestors_from_that_root_with_the_time_filter() {
            let selection = resolve_history_selection(HistoryArgs {
                rev: vec!["HEAD".to_string()],
                since: Some("2020-01-01".to_string()),
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Revision("HEAD".into())]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors { per_root: None }
            );
            assert!(selection.time.since.is_some());
        }

        #[test]
        fn first_parent_alone_defaults_to_head_ancestors_and_first_parent_mode() {
            let selection = resolve_history_selection(HistoryArgs {
                first_parent: true,
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Head]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors { per_root: None }
            );
            assert_eq!(selection.parents, ParentMode::First);
        }

        #[test]
        fn exclude_rev_alone_defaults_to_head_ancestors_and_hides_the_excluded_revision() {
            let selection = resolve_history_selection(HistoryArgs {
                exclude_rev: vec!["HEAD~1".to_string()],
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.roots, vec![HistoryRoot::Head]);
            assert_eq!(
                selection.traversal,
                HistoryTraversal::Ancestors { per_root: None }
            );
            assert_eq!(selection.excluded, vec!["HEAD~1".into()]);
        }

        /// A pure scope selection (`--rev`/`--branches`/`--tags` alone or
        /// combined) must never accidentally become ancestor traversal
        /// just because more than one scope selector was given.
        #[test]
        fn combined_scope_only_selectors_stay_tips() {
            let selection = resolve_history_selection(HistoryArgs {
                rev: vec!["HEAD".to_string()],
                branches: true,
                tags: true,
                ..args()
            })
            .unwrap()
            .unwrap();
            assert_eq!(selection.traversal, HistoryTraversal::Tips);
        }

        #[test]
        fn no_history_flags_still_returns_none_even_with_all_defaults() {
            assert!(
                resolve_history_selection(HistoryArgs::default())
                    .unwrap()
                    .is_none()
            );
        }
    }

    fn parse(args: &[&str]) -> Cli {
        let mut full = vec!["gat"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    /// The real top-level `gat pull` dispatch (`app::run`)
    /// -- not a direct `gat_command::pull` call in a unit test -- must build
    /// (and reuse) exactly one
    /// coherent operation acquisition for the whole invocation: resolving
    /// the CLI/config selection filter from it, then threading that same
    /// snapshot through fetch, sync, and any implicit repair. Before this
    /// test, `app::run` called `repo.load_config()` once to resolve the
    /// filter and then built a second, independent acquisition (and its
    /// own config load) inside pull orchestration -- a real double-load this
    /// test would have caught.
    #[test]
    fn pull_dispatch_loads_effective_config_exactly_once() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        gat_engine::initialize_backends();
        let tmp = test_repo();
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        let repo = Repository::at(tmp.path().to_path_buf());
        ::test_support::add(&repo, &[std::path::PathBuf::from("a.bin")], &NoopProgress).unwrap();
        commit_all(tmp.path(), "add a.bin");

        let remote_dir = tempfile::tempdir().unwrap();
        let url = gat_io::remote_file_url_for_test(remote_dir.path());
        remote_add_with_default(&repo, "origin", url).unwrap();
        let selection = Selection::root();
        gat_command::push(
            &repo,
            gat_command::PushRequest {
                selection: Some(&selection),
                remote: None,
                source: gat_command::PushSource::Current,
            },
            &NoopProgress,
        )
        .unwrap();

        let context = Context::new(repo);
        let cli = parse(&["pull"]);

        let config_loads_before = gat_engine::test_support::config_loads();
        run(cli, &context, &NoopProgress).unwrap();

        assert_eq!(
            gat_engine::test_support::config_loads() - config_loads_before,
            1,
            "one `gat pull` dispatch (selection-filter resolution, fetch, \
             sync, and any implicit repair) must load effective config \
             exactly once"
        );
    }

    /// Every [`AppError`] variant converts to a [`Failure`] whose
    /// diagnostic is specific, stable, and mentions no low-level/
    /// third-party text (there is none to mention -- `AppError` carries
    /// no technical source at all), directly, without going through the
    /// full CLI dispatch path.
    #[test]
    fn every_app_error_variant_converts_to_a_specific_diagnostic() {
        let history: Failure = AppError::HistoryRequiresRemote.into();
        assert_eq!(
            history.diagnostic().summary(),
            "History flags require `--remote`"
        );
        assert!(history.diagnostic().detail().is_some());
        assert_eq!(history.diagnostic().hints().len(), 1);

        let fetch: Failure = AppError::DryRunConflictsWithFetch.into();
        assert!(fetch.diagnostic().summary().contains("--dry-run"));
        assert!(fetch.diagnostic().summary().contains("--fetch"));

        let repair: Failure = AppError::DryRunConflictsWithRepair.into();
        assert!(repair.diagnostic().summary().contains("--dry-run"));
        assert!(repair.diagnostic().summary().contains("--repair"));
    }

    /// Every `Failure` produced from an `AppError` still exits `1`, same
    /// as any other application failure: app-level policy
    /// errors don't get their own exit-code carve-out.
    #[test]
    fn app_error_failures_still_exit_code_one() {
        let failure: Failure = AppError::HistoryRequiresRemote.into();
        assert_eq!(failure.exit_code(), 1);
    }

    #[test]
    fn reject_dry_run_conflicts_allows_every_non_conflicting_combination() {
        assert!(reject_dry_run_conflicts(false, false, false).is_ok());
        assert!(reject_dry_run_conflicts(true, false, false).is_ok());
        assert!(reject_dry_run_conflicts(false, true, false).is_ok());
        assert!(reject_dry_run_conflicts(false, false, true).is_ok());
    }

    #[test]
    fn reject_dry_run_conflicts_rejects_dry_run_with_fetch() {
        let err = reject_dry_run_conflicts(true, true, false).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.diagnostic().summary().contains("--fetch"));
    }

    #[test]
    fn reject_dry_run_conflicts_rejects_dry_run_with_repair() {
        let err = reject_dry_run_conflicts(true, false, true).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.diagnostic().summary().contains("--repair"));
    }

    /// `exit_result` turns a sync/pull/hook outcome's structured
    /// `completion` status directly into a `Failure`'s diagnostic (not
    /// through an anonymous `bail!("{err}")` string), and leaves every
    /// other outcome variant (including a clean sync) untouched.
    #[test]
    fn exit_result_maps_unresolved_sync_outcome_to_a_conflict_diagnostic() {
        let outcome = Outcome::Synced(gat_command::SyncOutcome {
            scope: gat_command::SelectionScope::Unrestricted,
            outcome: gat_engine::SyncOutcome::default(),
            fetched: 0,
            repaired: 0,
            repair_failures: Vec::new(),
            reshaped: None,
            shallow: false,
            completion: gat_command::SyncCompletionStatus::Incomplete {
                conflicts: 2,
                missing: 3,
                corrupted: 4,
            },
        });
        let err = exit_result(&outcome).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.diagnostic().code(), ErrorCode::Conflict);
        let summary = err.diagnostic().summary().to_string();
        assert!(summary.contains("2 conflict(s)"));
        assert!(summary.contains("3 missing object(s)"));
        assert!(summary.contains("4 corrupted object(s)"));
    }

    #[test]
    fn exit_result_is_ok_for_a_clean_sync_outcome() {
        let outcome = Outcome::Synced(gat_command::SyncOutcome {
            scope: gat_command::SelectionScope::Unrestricted,
            outcome: gat_engine::SyncOutcome::default(),
            fetched: 0,
            repaired: 0,
            repair_failures: Vec::new(),
            reshaped: None,
            shallow: false,
            completion: gat_command::SyncCompletionStatus::Clean,
        });
        assert!(exit_result(&outcome).is_ok());
    }
}
