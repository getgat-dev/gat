//! Presentation for `gat system inspect`/`repair`/`clean`: turns the
//! typed, presentation-free facts returned by `gat-command` (a
//! [`gat_command::DomainFact`] per resolved domain) into rendered
//! [`SystemGroup`]/[`SystemRow`]s and footer text. The command layer
//! itself carries zero `crate::error`/`UserLine`/`RowDetail` -- this
//! module (not the command layer) is solely responsible for authoring
//! labels, composing identifiers/numbers into prose, and invoking
//! `crate::error::map::problem::*` to turn a raw reason enum into a
//! `UserProblem`.

use crate::error::map::problem;
use crate::output::rows::RowDetail;
use crate::presentation::UserLine;
use gat_command::{
    CacheClean, CacheDbState, CacheFact, CacheInspect, CacheRepair, CandidateOutcome, DomainFact,
    GitClean, GitFact, GitInspect, GitRepair, LiveLockState, LockClean, LockFact, LockRepair,
    LockState, PreparedTxnStatus, RecoveryChoice, StateClean, StateDbState, StateFact,
    StateInspect, StateRepair, TransactionKind, TransactionState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SystemStatus {
    Success,
    Warning,
    Error,
}

pub struct SystemRow {
    pub(crate) status: SystemStatus,
    pub(crate) label: UserLine,
    pub(crate) detail: Option<RowDetail>,
}

impl SystemRow {
    const fn new(status: SystemStatus, label: UserLine, detail: RowDetail) -> Self {
        Self {
            status,
            label,
            detail: Some(detail),
        }
    }

    const fn detail_less(status: SystemStatus, label: UserLine) -> Self {
        Self {
            status,
            label,
            detail: None,
        }
    }
}

pub struct SystemGroup {
    pub(crate) hints: Vec<UserLine>,
    pub(crate) status: SystemStatus,
    pub(crate) title: UserLine,
    pub(crate) rows: Vec<SystemRow>,
}

pub struct RenderedSystemOutcome {
    pub(crate) status: SystemStatus,
    pub(crate) title: UserLine,
    pub(crate) summary: UserLine,
    pub(crate) groups: Vec<SystemGroup>,
    pub(crate) footer: Vec<UserLine>,
}

pub fn render(outcome: gat_command::SystemOutcome) -> RenderedSystemOutcome {
    let (title, success_summary, warning_summary) = match outcome.verb {
        gat_command::SystemVerb::Inspect => ("System state", "healthy", "attention required"),
        gat_command::SystemVerb::Repair => ("System repair", "complete", "incomplete"),
        gat_command::SystemVerb::Clean => ("System cleanup", "complete", "attention required"),
    };

    let mut groups = Vec::with_capacity(outcome.facts.len());
    let mut footer: Vec<UserLine> = Vec::new();
    let mut attention = false;
    for fact in outcome.facts {
        let (group, lines) = render_domain(fact);
        if group.status >= SystemStatus::Warning {
            attention = true;
        }
        groups.push(group);
        for line in lines {
            if !footer.iter().any(|existing| existing == &line) {
                footer.push(line);
            }
        }
    }

    RenderedSystemOutcome {
        status: if attention {
            SystemStatus::Warning
        } else {
            SystemStatus::Success
        },
        title: UserLine::identifier(title),
        summary: UserLine::identifier(if attention {
            warning_summary
        } else {
            success_summary
        }),
        groups,
        footer,
    }
}

fn render_domain(fact: DomainFact) -> (SystemGroup, Vec<UserLine>) {
    match fact {
        DomainFact::Lock(fact) => render_lock(fact),
        DomainFact::State(fact) => render_state(fact),
        DomainFact::Cache(fact) => render_cache(fact),
        DomainFact::Git(fact) => render_git(fact),
    }
}

fn recovery_result_text(choice: RecoveryChoice) -> UserLine {
    match choice {
        RecoveryChoice::RestoreBackup => UserLine::authored("backup restored"),
        RecoveryChoice::PromoteStaged => UserLine::authored("staged shape promoted"),
    }
}

fn explicit_recovery_command(txn_id: &str, choice: RecoveryChoice) -> UserLine {
    let flag = match choice {
        RecoveryChoice::RestoreBackup => "--restore-backup",
        RecoveryChoice::PromoteStaged => "--promote-staged",
    };
    UserLine::compose([
        UserLine::authored("Run `gat system repair lock "),
        UserLine::authored(flag),
        UserLine::authored(" --transaction "),
        UserLine::identifier(txn_id),
        UserLine::authored("` to recover it."),
    ])
}

fn render_lock(fact: LockFact) -> (SystemGroup, Vec<UserLine>) {
    match fact {
        LockFact::Inspect(state) => render_lock_inspect(state),
        LockFact::Repair(repair) => render_lock_repair(repair),
        LockFact::Clean(clean) => render_lock_clean(&clean),
    }
}

/// Authors the summary text for a *validated* reshape-recovery candidate
/// -- a successful fact, not a nonfatal problem, so it is built directly
/// as a [`UserLine`] here in `output` rather than routed through
/// `UserProblem`, which is reserved for actual nonfatal problems.
fn valid_candidate_line(shard_levels: gat_core::lock::LockShardLevels, entries: usize) -> UserLine {
    let shape_line = if shard_levels.is_flat() {
        UserLine::authored("flat")
    } else {
        UserLine::compose([
            UserLine::authored("sharded ("),
            UserLine::number(i64::from(shard_levels.get())),
            UserLine::authored(if shard_levels.get() == 1 {
                " level)"
            } else {
                " levels)"
            }),
        ])
    };
    UserLine::compose([
        UserLine::authored("validated "),
        shape_line,
        UserLine::authored(" lock ("),
        UserLine::number(entries as i64),
        UserLine::authored(if entries == 1 { " entry)" } else { " entries)" }),
    ])
}

fn live_lock_row(live: LiveLockState) -> Option<SystemRow> {
    match live {
        LiveLockState::Missing => None,
        LiveLockState::Valid {
            shard_levels,
            entries,
        } => Some(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("gat.lock"),
            RowDetail::message(valid_candidate_line(shard_levels, entries)),
        )),
        LiveLockState::Invalid { reason } => {
            let problem = problem::live_lock_invalid_problem(reason);
            Some(SystemRow::new(
                SystemStatus::Error,
                UserLine::identifier("gat.lock"),
                RowDetail::from(problem),
            ))
        }
    }
}

fn render_lock_inspect(state: LockState) -> (SystemGroup, Vec<UserLine>) {
    let LockState { live, transactions } = state;
    let mut rows = Vec::new();
    let mut footer = Vec::new();
    let mut status = SystemStatus::Success;
    if let Some(row) = live_lock_row(live) {
        status = status.max(row.status);
        rows.push(row);
    }
    for txn in transactions {
        let TransactionState { id, kind, .. } = txn;
        match kind {
            TransactionKind::ScratchOnly => {
                status = status.max(SystemStatus::Warning);
                rows.push(SystemRow::detail_less(
                    SystemStatus::Warning,
                    UserLine::with_identifier("txn ", &id, ""),
                ));
            }
            TransactionKind::Malformed { reason } => {
                status = status.max(SystemStatus::Error);
                let problem = problem::transaction_malformed_problem(reason);
                rows.push(SystemRow::new(
                    SystemStatus::Error,
                    UserLine::with_identifier("txn ", &id, ""),
                    RowDetail::from(problem),
                ));
            }
            TransactionKind::Prepared(prepared) => match prepared.status {
                PreparedTxnStatus::CleanablePrepared | PreparedTxnStatus::CompletedNotCleaned => {
                    status = status.max(SystemStatus::Warning);
                    rows.push(SystemRow::new(
                        SystemStatus::Warning,
                        UserLine::with_identifier("txn ", &id, ""),
                        RowDetail::authored("completed, scratch not yet cleaned"),
                    ));
                }
                PreparedTxnStatus::RecoveryRequired
                | PreparedTxnStatus::AmbiguousRecovery
                | PreparedTxnStatus::CorruptRecoveryState => {
                    status = status.max(SystemStatus::Warning);
                    rows.push(SystemRow::new(
                        SystemStatus::Warning,
                        UserLine::with_identifier("txn ", &id, ""),
                        RowDetail::authored("interrupted reshape requires recovery"),
                    ));
                    footer.push(UserLine::identifier(
                        "Run `gat system repair lock` to inspect recovery options.",
                    ));
                }
            },
        }
    }
    if rows.is_empty() {
        rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("gat.lock"),
            RowDetail::authored("no lock present"),
        ));
    }
    (
        SystemGroup {
            hints: Vec::new(),
            status,
            title: UserLine::identifier("Lock"),
            rows,
        },
        footer,
    )
}

fn render_lock_repair(repair: LockRepair) -> (SystemGroup, Vec<UserLine>) {
    match repair {
        LockRepair::NothingToDo => (
            SystemGroup {
                hints: Vec::new(),
                status: SystemStatus::Success,
                title: UserLine::identifier("Lock"),
                rows: vec![SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("gat.lock"),
                    RowDetail::authored("no repair needed"),
                )],
            },
            Vec::new(),
        ),
        LockRepair::CleanScratch { count } => (
            SystemGroup {
                hints: vec![UserLine::authored(
                    "Run `gat system clean lock` to remove completed reshape scratch.",
                )],
                status: SystemStatus::Success,
                title: UserLine::identifier("Lock"),
                rows: vec![SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("transactions"),
                    RowDetail::message(UserLine::compose([
                        UserLine::number(count as i64),
                        UserLine::authored(" completed transaction director"),
                        UserLine::authored(if count == 1 { "y" } else { "ies" }),
                        UserLine::authored(" ready to clean"),
                    ])),
                )],
            },
            Vec::new(),
        ),
        LockRepair::Recovered { choice, .. } => (
            SystemGroup {
                hints: vec![UserLine::authored(
                    "Run `gat system clean lock` to remove the now-completed transaction scratch.",
                )],
                status: SystemStatus::Success,
                title: UserLine::identifier("Lock"),
                rows: vec![SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("gat.lock"),
                    RowDetail::message(recovery_result_text(choice)),
                )],
            },
            Vec::new(),
        ),
        LockRepair::ExplicitChoiceRequired { live, unresolved } => {
            let mut rows = Vec::new();
            if let Some(row) = live_lock_row(live) {
                rows.push(row);
            }
            let mut footer = Vec::new();
            for txn in unresolved {
                let TransactionState { id, kind, .. } = txn;
                let TransactionKind::Prepared(prepared) = kind else {
                    continue;
                };
                rows.push(SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::with_identifier("txn ", &id, ""),
                    RowDetail::authored("interrupted reshape requires an explicit choice"),
                ));
                if matches!(prepared.backup.outcome, CandidateOutcome::Valid { .. }) {
                    footer.push(explicit_recovery_command(
                        &id,
                        RecoveryChoice::RestoreBackup,
                    ));
                }
                if matches!(prepared.staged.outcome, CandidateOutcome::Valid { .. }) {
                    footer.push(explicit_recovery_command(
                        &id,
                        RecoveryChoice::PromoteStaged,
                    ));
                }
            }
            (
                SystemGroup {
                    hints: Vec::new(),
                    status: SystemStatus::Warning,
                    title: UserLine::identifier("Lock"),
                    rows,
                },
                footer,
            )
        }
    }
}

fn render_lock_clean(clean: &LockClean) -> (SystemGroup, Vec<UserLine>) {
    let mut rows = Vec::new();
    let mut footer = Vec::new();
    let status = if clean.unresolved {
        SystemStatus::Warning
    } else {
        SystemStatus::Success
    };
    if clean.removed > 0 {
        rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("transactions"),
            RowDetail::message(UserLine::compose([
                UserLine::authored("removed "),
                UserLine::number(clean.removed as i64),
                UserLine::authored(" disposable transaction director"),
                UserLine::authored(if clean.removed == 1 { "y" } else { "ies" }),
            ])),
        ));
    } else {
        rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("transactions"),
            RowDetail::authored("no disposable reshape scratch"),
        ));
    }
    if clean.unresolved {
        rows.push(SystemRow::new(
            SystemStatus::Warning,
            UserLine::identifier("recovery"),
            RowDetail::authored("preserved interrupted or ambiguous transaction state"),
        ));
        footer.push(UserLine::identifier(
            "Run `gat system repair lock` to inspect recovery options.",
        ));
    }
    (
        SystemGroup {
            hints: Vec::new(),
            status,
            title: UserLine::identifier("Lock"),
            rows,
        },
        footer,
    )
}

fn render_state(fact: StateFact) -> (SystemGroup, Vec<UserLine>) {
    match fact {
        StateFact::Inspect(inspect) => render_state_inspect(inspect),
        StateFact::Repair(repair) => render_state_repair(&repair),
        StateFact::Clean(clean) => render_state_clean(&clean),
    }
}

fn state_db_row(db: StateDbState, validation_required: bool) -> (Vec<SystemRow>, Vec<UserLine>) {
    match db {
        StateDbState::Healthy => {
            let mut rows = vec![SystemRow::new(
                SystemStatus::Success,
                UserLine::identifier("desired"),
                RowDetail::authored("current"),
            )];
            let mut footer = Vec::new();
            if validation_required {
                rows.push(SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("materialized"),
                    RowDetail::authored("provenance reset; validation required"),
                ));
                footer.push(validation_required_footer());
            } else {
                rows.push(SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("materialized"),
                    RowDetail::authored("current"),
                ));
            }
            (rows, footer)
        }
        StateDbState::Absent => (
            vec![
                SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("desired"),
                    RowDetail::authored("absent"),
                ),
                SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("materialized"),
                    RowDetail::authored("absent"),
                ),
            ],
            Vec::new(),
        ),
        StateDbState::Outdated(version) => (
            vec![
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("desired"),
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("outdated metadata format version "),
                        UserLine::number(version),
                    ])),
                ),
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("materialized"),
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("outdated metadata format version "),
                        UserLine::number(version),
                    ])),
                ),
            ],
            vec![UserLine::identifier(
                "Run `gat system repair state` to rebuild state metadata.",
            )],
        ),
        StateDbState::NewerVersion(version) => (
            vec![
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("desired"),
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("metadata format version "),
                        UserLine::number(version),
                        UserLine::authored(" is newer than this build supports"),
                    ])),
                ),
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("materialized"),
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("metadata format version "),
                        UserLine::number(version),
                        UserLine::authored(" is newer than this build supports"),
                    ])),
                ),
            ],
            vec![UserLine::identifier(
                "Newer-schema state metadata left untouched; upgrade `gat` to repair it.",
            )],
        ),
        StateDbState::Unreadable(reason) => {
            let problem = problem::db_unreadable_problem(reason);
            (
                vec![
                    SystemRow::new(
                        SystemStatus::Error,
                        UserLine::identifier("desired"),
                        RowDetail::composed("unreadable (", problem.clone(), ")"),
                    ),
                    SystemRow::new(
                        SystemStatus::Error,
                        UserLine::identifier("materialized"),
                        RowDetail::composed("unreadable (", problem, ")"),
                    ),
                ],
                vec![UserLine::identifier(
                    "Run `gat system repair state` to rebuild state metadata.",
                )],
            )
        }
    }
}

fn validation_required_footer() -> UserLine {
    UserLine::identifier("Run `gat sync` (or `gat status`) to validate the materialized worktree.")
}

fn render_state_inspect(inspect: StateInspect) -> (SystemGroup, Vec<UserLine>) {
    let StateInspect {
        db,
        stale_sidecars,
        validation_required,
    } = inspect;
    let (mut rows, mut footer) = state_db_row(db, validation_required);
    let mut status = rows
        .iter()
        .map(|row| row.status)
        .max()
        .unwrap_or(SystemStatus::Success);
    if stale_sidecars > 0 {
        status = status.max(SystemStatus::Warning);
        rows.push(SystemRow::new(
            SystemStatus::Warning,
            UserLine::identifier("sidecars"),
            RowDetail::message(UserLine::compose([
                UserLine::number(stale_sidecars as i64),
                UserLine::authored(" stale sidecar file"),
                UserLine::authored(if stale_sidecars == 1 { "" } else { "s" }),
            ])),
        ));
        footer.push(UserLine::identifier(
            "Run `gat system clean state` to remove obsolete state sidecars.",
        ));
    }
    (
        SystemGroup {
            hints: Vec::new(),
            status,
            title: UserLine::identifier("State"),
            rows,
        },
        footer,
    )
}

fn render_state_repair(repair: &StateRepair) -> (SystemGroup, Vec<UserLine>) {
    match repair {
        StateRepair::NewerVersion { version } => (
            SystemGroup {
                hints: Vec::new(),
                status: SystemStatus::Warning,
                title: UserLine::identifier("State"),
                rows: vec![SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("desired"),
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("metadata format version "),
                        UserLine::number(*version),
                        UserLine::authored(
                            " is newer than this build supports; refusing to rewrite it",
                        ),
                    ])),
                )],
            },
            vec![UserLine::identifier(
                "Upgrade `gat` to repair newer-schema state metadata.",
            )],
        ),
        StateRepair::AlreadyValid {
            validation_required,
        } => {
            if *validation_required {
                (
                    SystemGroup {
                        hints: Vec::new(),
                        status: SystemStatus::Warning,
                        title: UserLine::identifier("State"),
                        rows: vec![
                            SystemRow::new(
                                SystemStatus::Success,
                                UserLine::identifier("desired"),
                                RowDetail::authored("already valid"),
                            ),
                            SystemRow::new(
                                SystemStatus::Warning,
                                UserLine::identifier("materialized"),
                                RowDetail::authored("provenance reset; validation required"),
                            ),
                        ],
                    },
                    vec![validation_required_footer()],
                )
            } else {
                (
                    SystemGroup {
                        hints: Vec::new(),
                        status: SystemStatus::Success,
                        title: UserLine::identifier("State"),
                        rows: vec![
                            SystemRow::new(
                                SystemStatus::Success,
                                UserLine::identifier("desired"),
                                RowDetail::authored("already valid"),
                            ),
                            SystemRow::new(
                                SystemStatus::Success,
                                UserLine::identifier("materialized"),
                                RowDetail::authored("already valid"),
                            ),
                        ],
                    },
                    Vec::new(),
                )
            }
        }
        StateRepair::Rebuilt => (
            SystemGroup {
                hints: Vec::new(),
                status: SystemStatus::Warning,
                title: UserLine::identifier("State"),
                rows: vec![
                    SystemRow::new(
                        SystemStatus::Success,
                        UserLine::identifier("desired"),
                        RowDetail::authored("rebuilt"),
                    ),
                    SystemRow::new(
                        SystemStatus::Warning,
                        UserLine::identifier("materialized"),
                        RowDetail::authored("provenance reset; validation required"),
                    ),
                ],
            },
            vec![validation_required_footer()],
        ),
    }
}

fn render_state_clean(clean: &StateClean) -> (SystemGroup, Vec<UserLine>) {
    let rows = if clean.removed == 0 {
        vec![SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("artifacts"),
            RowDetail::authored("no obsolete state artifacts"),
        )]
    } else {
        vec![SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("artifacts"),
            RowDetail::message(UserLine::compose([
                UserLine::authored("removed "),
                UserLine::number(clean.removed as i64),
                UserLine::authored(" obsolete state sidecar file"),
                UserLine::authored(if clean.removed == 1 { "" } else { "s" }),
            ])),
        )]
    };
    (
        SystemGroup {
            hints: Vec::new(),
            status: SystemStatus::Success,
            title: UserLine::identifier("State"),
            rows,
        },
        Vec::new(),
    )
}

fn cache_db_row(db: CacheDbState) -> (SystemRow, Vec<UserLine>) {
    match db {
        CacheDbState::Absent => (
            SystemRow::new(
                SystemStatus::Success,
                UserLine::identifier("cache metadata"),
                RowDetail::authored("absent"),
            ),
            Vec::new(),
        ),
        CacheDbState::Healthy => (
            SystemRow::new(
                SystemStatus::Success,
                UserLine::identifier("cache metadata"),
                RowDetail::authored("healthy"),
            ),
            Vec::new(),
        ),
        CacheDbState::UnsupportedVersion(version) => (
            SystemRow::new(
                SystemStatus::Warning,
                UserLine::identifier("cache metadata"),
                RowDetail::message(UserLine::compose([
                    UserLine::authored("metadata format version "),
                    UserLine::number(version),
                    UserLine::authored(" is not supported by this build"),
                ])),
            ),
            vec![UserLine::identifier(
                "Unsupported-schema cache metadata left untouched; use a compatible `gat` \
                 version to repair it.",
            )],
        ),
        CacheDbState::Unreadable(reason) => {
            let problem = problem::db_unreadable_problem(reason);
            (
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("cache metadata"),
                    RowDetail::composed("disabled (", problem, ")"),
                ),
                vec![UserLine::identifier(
                    "Run `gat system repair cache` to rebuild cache metadata.",
                )],
            )
        }
    }
}

fn render_cache(fact: CacheFact) -> (SystemGroup, Vec<UserLine>) {
    match fact {
        CacheFact::Inspect(inspect) => render_cache_inspect(inspect),
        CacheFact::Repair(repair) => render_cache_repair(&repair),
        CacheFact::Clean(clean) => render_cache_clean(&clean),
    }
}

fn render_cache_inspect(inspect: CacheInspect) -> (SystemGroup, Vec<UserLine>) {
    let CacheInspect { db, temporary } = inspect;
    let (db_row, mut footer) = cache_db_row(db);
    let mut status = db_row.status;
    let mut rows = vec![db_row];
    if temporary == 0 {
        rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("temporary objects"),
            RowDetail::authored("none"),
        ));
    } else {
        status = status.max(SystemStatus::Warning);
        rows.push(SystemRow::new(
            SystemStatus::Warning,
            UserLine::identifier("temporary objects"),
            RowDetail::message(UserLine::compose([
                UserLine::number(temporary as i64),
                UserLine::authored(" unverified temp file"),
                UserLine::authored(if temporary == 1 { "" } else { "s" }),
            ])),
        ));
        footer.push(UserLine::identifier(
            "Run `gat system clean cache` to remove unverified temporary objects.",
        ));
    }
    (
        SystemGroup {
            hints: Vec::new(),
            status,
            title: UserLine::identifier("Cache"),
            rows,
        },
        footer,
    )
}

fn render_cache_repair(repair: &CacheRepair) -> (SystemGroup, Vec<UserLine>) {
    let row = match repair {
        CacheRepair::UnsupportedVersion { version } => SystemRow::new(
            SystemStatus::Warning,
            UserLine::identifier("cache metadata"),
            RowDetail::message(UserLine::compose([
                UserLine::authored("metadata format version "),
                UserLine::number(*version),
                UserLine::authored(" is not supported by this build; refusing to rewrite it"),
            ])),
        ),
        CacheRepair::AlreadyValid => SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("cache metadata"),
            RowDetail::authored("already valid"),
        ),
        CacheRepair::Rebuilt => SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("cache metadata"),
            RowDetail::authored("rebuilt and verified"),
        ),
    };
    let status = row.status;
    (
        SystemGroup {
            hints: Vec::new(),
            status,
            title: UserLine::identifier("Cache"),
            rows: vec![row],
        },
        Vec::new(),
    )
}

fn render_cache_clean(clean: &CacheClean) -> (SystemGroup, Vec<UserLine>) {
    use gat_command::TemporaryCleanOutcome;
    let CacheClean {
        temporary,
        objects_purged,
    } = clean;
    let mut rows = Vec::new();
    let mut status = SystemStatus::Success;
    match temporary {
        TemporaryCleanOutcome::NonePresent => rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("temporary"),
            RowDetail::authored("no temporary objects"),
        )),
        TemporaryCleanOutcome::Preserved { count } => {
            status = status.max(SystemStatus::Warning);
            rows.push(SystemRow::new(
                SystemStatus::Warning,
                UserLine::identifier("temporary"),
                RowDetail::message(UserLine::compose([
                    UserLine::number(*count as i64),
                    UserLine::authored(" temporary object"),
                    UserLine::authored(if *count == 1 { "" } else { "s" }),
                    UserLine::authored(" preserved (use --purge-temporary to remove)"),
                ])),
            ));
        }
        TemporaryCleanOutcome::NoneToPurge => rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("temporary"),
            RowDetail::authored("no temporary objects to purge"),
        )),
        TemporaryCleanOutcome::Purged { count } => rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("temporary"),
            RowDetail::message(UserLine::compose([
                UserLine::authored("purged "),
                UserLine::number(*count as i64),
                UserLine::authored(" temporary object"),
                UserLine::authored(if *count == 1 { "" } else { "s" }),
            ])),
        )),
    }
    if let Some(purged) = objects_purged {
        rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("objects"),
            RowDetail::message(UserLine::compose([
                UserLine::authored("purged "),
                UserLine::number(*purged as i64),
                UserLine::authored(" cached object"),
                UserLine::authored(if *purged == 1 { "" } else { "s" }),
            ])),
        ));
    }
    (
        SystemGroup {
            hints: Vec::new(),
            status,
            title: UserLine::identifier("Cache"),
            rows,
        },
        Vec::new(),
    )
}

fn render_git(fact: GitFact) -> (SystemGroup, Vec<UserLine>) {
    match fact {
        GitFact::Inspect(inspect) => render_git_inspect(&inspect),
        GitFact::Repair(repair) => render_git_repair(&repair),
        GitFact::Clean(clean) => render_git_clean(&clean),
    }
}

fn git_group(status: SystemStatus, detail: &'static str) -> (SystemGroup, Vec<UserLine>) {
    (
        SystemGroup {
            hints: Vec::new(),
            status,
            title: UserLine::identifier("Git"),
            rows: vec![SystemRow::new(
                status,
                UserLine::identifier("excludes"),
                RowDetail::authored(detail),
            )],
        },
        Vec::new(),
    )
}

fn render_git_inspect(inspect: &GitInspect) -> (SystemGroup, Vec<UserLine>) {
    let (status, detail) = match inspect {
        GitInspect::Current => (SystemStatus::Success, "current"),
        GitInspect::Stale => (SystemStatus::Warning, "stale, will be regenerated"),
        GitInspect::PresentButUnvalidated => {
            (SystemStatus::Warning, "present but could not be validated")
        }
        GitInspect::UnableToDeriveExpected => (
            SystemStatus::Error,
            "could not determine the expected excludes",
        ),
    };
    git_group(status, detail)
}

fn render_git_repair(repair: &GitRepair) -> (SystemGroup, Vec<UserLine>) {
    let (status, detail) = match repair {
        GitRepair::Rebuilt => (SystemStatus::Success, "rebuilt"),
        GitRepair::AlreadyCurrent => (SystemStatus::Success, "already current"),
    };
    git_group(status, detail)
}

fn render_git_clean(clean: &GitClean) -> (SystemGroup, Vec<UserLine>) {
    let (status, detail) = match clean {
        GitClean::NoManagedArtifacts => (SystemStatus::Success, "no managed excludes present"),
        GitClean::RemovedStale => (SystemStatus::Success, "removed stale managed excludes"),
        GitClean::NoStaleArtifacts => (SystemStatus::Success, "managed excludes already current"),
        GitClean::PreservedUnvalidated => (
            SystemStatus::Warning,
            "managed excludes could not be validated, preserved",
        ),
    };
    git_group(status, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_maintenance_reports_results_once_as_successes() {
        for choice in [RecoveryChoice::RestoreBackup, RecoveryChoice::PromoteStaged] {
            let (group, _) = render_lock_repair(LockRepair::Recovered {
                txn_id: "recovery".into(),
                choice,
            });
            assert_eq!(group.rows.len(), 1);
            assert_eq!(group.rows[0].status, SystemStatus::Success);
            assert_eq!(
                group.rows[0].detail.as_ref().unwrap().line(),
                &recovery_result_text(choice)
            );
        }
        let (group, _) = render_lock_clean(&LockClean {
            removed: 2,
            unresolved: false,
        });
        assert_eq!(group.rows.len(), 1);
        assert_eq!(group.rows[0].status, SystemStatus::Success);
        assert_eq!(
            group.rows[0].detail.as_ref().unwrap().as_str(),
            "removed 2 disposable transaction directories"
        );
        let (group, _) = render_git_repair(&GitRepair::Rebuilt);
        assert_eq!(group.status, SystemStatus::Success);
        let (group, _) = render_cache_clean(&CacheClean {
            temporary: gat_command::TemporaryCleanOutcome::NonePresent,
            objects_purged: Some(2),
        });
        assert!(
            group
                .rows
                .iter()
                .all(|row| row.status == SystemStatus::Success)
        );
        assert_eq!(
            group.rows[1].detail.as_ref().unwrap().as_str(),
            "purged 2 cached objects"
        );
    }

    #[test]
    fn successful_cleanup_does_not_hide_preserved_recovery_state() {
        let (group, footer) = render_lock_clean(&LockClean {
            removed: 2,
            unresolved: true,
        });
        assert_eq!(group.status, SystemStatus::Warning);
        assert_eq!(group.rows.len(), 2);
        assert_eq!(group.rows[0].status, SystemStatus::Success);
        assert_eq!(group.rows[1].status, SystemStatus::Warning);
        assert!(!footer.is_empty());
    }
}
