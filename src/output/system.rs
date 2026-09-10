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
use crate::output::layout::DetailMode;
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
    detail: RowDetail,
}

impl SystemRow {
    fn explanation(&self) -> Option<&UserLine> {
        (self.status >= SystemStatus::Warning).then(|| self.detail.line())
    }

    pub(crate) fn metadata(&self, mode: DetailMode) -> &UserLine {
        // Findings have complete paragraphs; repeating them in full-mode rows
        // would duplicate the explanation and allow it to overflow unwrapped.
        self.detail.display_line(if self.explanation().is_some() {
            DetailMode::Compact
        } else {
            mode
        })
    }

    /// Every system state has compact wording and a complete explanation.
    fn new(
        status: SystemStatus,
        label: UserLine,
        annotation: impl Into<UserLine>,
        detail: RowDetail,
    ) -> Self {
        Self {
            status,
            label,
            detail: detail.with_annotation(annotation),
        }
    }
}

fn count_annotation(count: usize, state: &'static str) -> UserLine {
    UserLine::compose([
        UserLine::number(count as i64),
        UserLine::authored(" "),
        UserLine::authored(state),
    ])
}

pub struct SystemGroup {
    pub(crate) hints: Vec<UserLine>,
    pub(crate) title: UserLine,
    pub(crate) rows: Vec<SystemRow>,
}

impl SystemGroup {
    pub(crate) fn status(&self) -> SystemStatus {
        self.rows
            .iter()
            .map(|row| row.status)
            .max()
            .unwrap_or(SystemStatus::Success)
    }

    /// Retain every distinct explanation, including hidden rows, without
    /// repeating identical prose for each affected identity. Row order supplies
    /// stable first-occurrence order; the table retains the individual labels.
    pub(crate) fn explanations(&self) -> impl Iterator<Item = UserLine> + '_ {
        let mut indices = std::collections::HashMap::new();
        let mut groups: Vec<(&SystemRow, usize)> = Vec::new();
        for (row, explanation) in self
            .rows
            .iter()
            .filter_map(|row| row.explanation().map(|explanation| (row, explanation)))
        {
            let next = groups.len();
            let index = *indices.entry(explanation).or_insert(next);
            if index == next {
                groups.push((row, 1));
            } else {
                groups[index].1 += 1;
            }
        }
        groups.into_iter().map(|(row, count)| {
            let subject = if count == 1 {
                row.label.clone()
            } else {
                UserLine::compose([
                    UserLine::number(count as i64),
                    UserLine::authored(" findings"),
                ])
            };
            UserLine::compose([subject, UserLine::authored(": "), row.detail.line().clone()])
        })
    }
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
    let mut status = SystemStatus::Success;
    for fact in outcome.facts {
        let (group, lines) = render_domain(fact);
        status = status.max(group.status());
        groups.push(group);
        for line in lines {
            if !footer.iter().any(|existing| existing == &line) {
                footer.push(line);
            }
        }
    }

    RenderedSystemOutcome {
        status,
        title: UserLine::authored(title),
        summary: UserLine::authored(if status >= SystemStatus::Warning {
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
        UserLine::authored("Run `"),
        UserLine::compose([
            UserLine::authored("gat system repair lock "),
            UserLine::authored(flag),
            UserLine::authored(" --transaction "),
            UserLine::identifier(txn_id),
        ])
        .unbroken(),
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
            "valid",
            RowDetail::message(valid_candidate_line(shard_levels, entries)),
        )),
        LiveLockState::Invalid { reason } => {
            let problem = problem::live_lock_invalid_problem(reason);
            Some(SystemRow::new(
                SystemStatus::Error,
                UserLine::identifier("gat.lock"),
                "unreadable",
                RowDetail::from(problem),
            ))
        }
    }
}

fn render_lock_inspect(state: LockState) -> (SystemGroup, Vec<UserLine>) {
    let LockState { live, transactions } = state;
    let mut rows = Vec::new();
    let mut footer = Vec::new();

    if let Some(row) = live_lock_row(live) {
        rows.push(row);
    }
    for txn in transactions {
        let TransactionState { id, kind, .. } = txn;
        match kind {
            TransactionKind::ScratchOnly => {
                rows.push(SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::with_identifier("txn ", &id, ""),
                    "scratch",
                    RowDetail::authored("incomplete transaction scratch remains"),
                ));
            }
            TransactionKind::Malformed { reason } => {
                let problem = problem::transaction_malformed_problem(reason);
                rows.push(SystemRow::new(
                    SystemStatus::Error,
                    UserLine::with_identifier("txn ", &id, ""),
                    "malformed",
                    RowDetail::from(problem),
                ));
            }
            TransactionKind::Prepared(prepared) => match prepared.status {
                PreparedTxnStatus::CleanablePrepared | PreparedTxnStatus::CompletedNotCleaned => {
                    rows.push(SystemRow::new(
                        SystemStatus::Warning,
                        UserLine::with_identifier("txn ", &id, ""),
                        "completed",
                        RowDetail::authored("completed, scratch not yet cleaned"),
                    ));
                }
                PreparedTxnStatus::RecoveryRequired
                | PreparedTxnStatus::AmbiguousRecovery
                | PreparedTxnStatus::CorruptRecoveryState => {
                    rows.push(SystemRow::new(
                        SystemStatus::Warning,
                        UserLine::with_identifier("txn ", &id, ""),
                        "interrupted",
                        RowDetail::authored("interrupted reshape requires recovery"),
                    ));
                    footer.push(UserLine::compose([
                        UserLine::authored("Run `"),
                        UserLine::identifier("gat system repair lock"),
                        UserLine::authored("` to inspect recovery options."),
                    ]));
                }
            },
        }
    }
    if rows.is_empty() {
        rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("gat.lock"),
            "absent",
            RowDetail::authored("no lock present"),
        ));
    }
    (
        SystemGroup {
            hints: Vec::new(),
            title: UserLine::identifier("Lock"),
            rows,
        },
        footer,
    )
}

fn render_lock_repair(repair: LockRepair) -> (SystemGroup, Vec<UserLine>) {
    let complete = repair.is_complete();
    match repair {
        LockRepair::NothingToDo => (
            SystemGroup {
                hints: Vec::new(),
                title: UserLine::identifier("Lock"),
                rows: vec![SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("gat.lock"),
                    "current",
                    RowDetail::authored("no repair needed"),
                )],
            },
            Vec::new(),
        ),
        LockRepair::CleanScratch { count } => (
            SystemGroup {
                hints: vec![UserLine::compose([
                    UserLine::authored("Run "),
                    UserLine::authored("`gat system clean lock`").unbroken(),
                    UserLine::authored(" to remove disposable reshape scratch."),
                ])],
                title: UserLine::identifier("Lock"),
                rows: vec![SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("transactions"),
                    count_annotation(count, "disposable"),
                    RowDetail::message(UserLine::compose([
                        UserLine::number(count as i64),
                        UserLine::authored(" disposable transaction director"),
                        UserLine::authored(if count == 1 { "y" } else { "ies" }),
                        UserLine::authored(" ready to clean"),
                    ])),
                )],
            },
            Vec::new(),
        ),
        LockRepair::Recovered { choice, state, .. } => {
            let mut group = SystemGroup {
                hints: vec![UserLine::compose([
                    UserLine::authored("Run "),
                    UserLine::authored("`gat system clean lock`").unbroken(),
                    UserLine::authored(" to remove the now-completed transaction scratch."),
                ])],
                title: UserLine::identifier("Lock"),
                rows: vec![SystemRow::new(
                    SystemStatus::Success,
                    UserLine::authored("recovery"),
                    "recovered",
                    RowDetail::message(recovery_result_text(choice)),
                )],
            };
            let footer = if complete {
                Vec::new()
            } else {
                let (remaining, choices) = render_lock_recovery(state);
                group.rows.extend(remaining.rows);
                choices
            };
            (group, footer)
        }
        LockRepair::Unresolved { state } => render_lock_recovery(state),
        LockRepair::RecoveryNotSelected { state, reason } => {
            let (mut group, choices) = render_lock_recovery(state);
            let (annotation, detail) = match reason {
                gat_command::RecoverySelectionFailure::NoMatch => (
                    "unavailable",
                    RowDetail::authored("no transaction matches the requested recovery choice"),
                ),
                gat_command::RecoverySelectionFailure::Ambiguous => (
                    "ambiguous",
                    RowDetail::message(UserLine::compose([
                        UserLine::authored(
                            "multiple transactions support the requested recovery choice; select one with ",
                        ),
                        UserLine::authored("`--transaction`").unbroken(),
                    ])),
                ),
            };
            group.rows.push(SystemRow::new(
                SystemStatus::Warning,
                UserLine::authored("recovery"),
                annotation,
                detail,
            ));
            (group, choices)
        }
    }
}

/// Keep the complete inspection alongside recovery options: malformed or
/// unrelated transactions must not disappear when another has a valid candidate.
fn render_lock_recovery(state: LockState) -> (SystemGroup, Vec<UserLine>) {
    let mut choices = Vec::new();
    for txn in &state.transactions {
        let TransactionKind::Prepared(prepared) = &txn.kind else {
            continue;
        };
        if matches!(
            prepared.status,
            PreparedTxnStatus::CleanablePrepared | PreparedTxnStatus::CompletedNotCleaned
        ) {
            continue;
        }
        if matches!(prepared.backup.outcome, CandidateOutcome::Valid { .. }) {
            choices.push(explicit_recovery_command(
                &txn.id,
                RecoveryChoice::RestoreBackup,
            ));
        }
        if matches!(prepared.staged.outcome, CandidateOutcome::Valid { .. }) {
            choices.push(explicit_recovery_command(
                &txn.id,
                RecoveryChoice::PromoteStaged,
            ));
        }
    }
    let (group, _) = render_lock_inspect(state);
    (group, choices)
}

fn render_lock_clean(clean: &LockClean) -> (SystemGroup, Vec<UserLine>) {
    let mut rows = Vec::new();
    let mut footer = Vec::new();
    if clean.removed > 0 {
        rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("transactions"),
            count_annotation(clean.removed, "removed"),
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
            "none",
            RowDetail::authored("no disposable reshape scratch"),
        ));
    }
    if clean.unresolved {
        rows.push(SystemRow::new(
            SystemStatus::Warning,
            UserLine::identifier("recovery"),
            "preserved",
            RowDetail::authored("preserved interrupted or ambiguous transaction state"),
        ));
        footer.push(UserLine::compose([
            UserLine::authored("Run `"),
            UserLine::identifier("gat system repair lock"),
            UserLine::authored("` to inspect recovery options."),
        ]));
    }
    (
        SystemGroup {
            hints: Vec::new(),
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
                "current",
                RowDetail::authored("current"),
            )];
            let mut footer = Vec::new();
            if validation_required {
                rows.push(SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("materialized"),
                    "unvalidated",
                    RowDetail::authored("provenance reset; validation required"),
                ));
                footer.push(validation_required_footer());
            } else {
                rows.push(SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("materialized"),
                    "current",
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
                    "absent",
                    RowDetail::authored("absent"),
                ),
                SystemRow::new(
                    SystemStatus::Success,
                    UserLine::identifier("materialized"),
                    "absent",
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
                    "outdated",
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("outdated metadata format version "),
                        UserLine::number(version),
                    ])),
                ),
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("materialized"),
                    "outdated",
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("outdated metadata format version "),
                        UserLine::number(version),
                    ])),
                ),
            ],
            vec![UserLine::compose([
                UserLine::authored("Run `"),
                UserLine::identifier("gat system repair state"),
                UserLine::authored("` to rebuild state metadata."),
            ])],
        ),
        StateDbState::NewerVersion(version) => (
            vec![
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("desired"),
                    "newer schema",
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("metadata format version "),
                        UserLine::number(version),
                        UserLine::authored(" is newer than this build supports"),
                    ])),
                ),
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("materialized"),
                    "newer schema",
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("metadata format version "),
                        UserLine::number(version),
                        UserLine::authored(" is newer than this build supports"),
                    ])),
                ),
            ],
            vec![UserLine::authored(
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
                        "unreadable",
                        RowDetail::composed("unreadable (", problem.clone(), ")"),
                    ),
                    SystemRow::new(
                        SystemStatus::Error,
                        UserLine::identifier("materialized"),
                        "unreadable",
                        RowDetail::composed("unreadable (", problem, ")"),
                    ),
                ],
                vec![UserLine::compose([
                    UserLine::authored("Run `"),
                    UserLine::identifier("gat system repair state"),
                    UserLine::authored("` to rebuild state metadata."),
                ])],
            )
        }
    }
}

fn validation_required_footer() -> UserLine {
    UserLine::compose([
        UserLine::authored("Run `"),
        UserLine::identifier("gat sync"),
        UserLine::authored("` (or `"),
        UserLine::identifier("gat status"),
        UserLine::authored("`) to validate the materialized worktree."),
    ])
}

fn render_state_inspect(inspect: StateInspect) -> (SystemGroup, Vec<UserLine>) {
    let StateInspect {
        db,
        stale_sidecars,
        validation_required,
    } = inspect;
    let (mut rows, mut footer) = state_db_row(db, validation_required);
    if stale_sidecars > 0 {
        rows.push(SystemRow::new(
            SystemStatus::Warning,
            UserLine::identifier("sidecars"),
            count_annotation(stale_sidecars, "stale"),
            RowDetail::message(UserLine::compose([
                UserLine::number(stale_sidecars as i64),
                UserLine::authored(" stale sidecar file"),
                UserLine::authored(if stale_sidecars == 1 { "" } else { "s" }),
            ])),
        ));
        footer.push(UserLine::compose([
            UserLine::authored("Run `"),
            UserLine::identifier("gat system clean state"),
            UserLine::authored("` to remove obsolete state sidecars."),
        ]));
    }
    (
        SystemGroup {
            hints: Vec::new(),
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
                title: UserLine::identifier("State"),
                rows: vec![SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("desired"),
                    "newer schema",
                    RowDetail::message(UserLine::compose([
                        UserLine::authored("metadata format version "),
                        UserLine::number(*version),
                        UserLine::authored(
                            " is newer than this build supports; refusing to rewrite it",
                        ),
                    ])),
                )],
            },
            vec![UserLine::authored(
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
                        title: UserLine::identifier("State"),
                        rows: vec![
                            SystemRow::new(
                                SystemStatus::Success,
                                UserLine::identifier("desired"),
                                "valid",
                                RowDetail::authored("already valid"),
                            ),
                            SystemRow::new(
                                SystemStatus::Warning,
                                UserLine::identifier("materialized"),
                                "unvalidated",
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
                        title: UserLine::identifier("State"),
                        rows: vec![
                            SystemRow::new(
                                SystemStatus::Success,
                                UserLine::identifier("desired"),
                                "valid",
                                RowDetail::authored("already valid"),
                            ),
                            SystemRow::new(
                                SystemStatus::Success,
                                UserLine::identifier("materialized"),
                                "valid",
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
                title: UserLine::identifier("State"),
                rows: vec![
                    SystemRow::new(
                        SystemStatus::Success,
                        UserLine::identifier("desired"),
                        "rebuilt",
                        RowDetail::authored("rebuilt"),
                    ),
                    SystemRow::new(
                        SystemStatus::Warning,
                        UserLine::identifier("materialized"),
                        "unvalidated",
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
            "none",
            RowDetail::authored("no obsolete state artifacts"),
        )]
    } else {
        vec![SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("artifacts"),
            count_annotation(clean.removed, "removed"),
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
            title: UserLine::identifier("State"),
            rows,
        },
        Vec::new(),
    )
}

fn compatible_cache_version_guidance() -> UserLine {
    UserLine::authored(
        "Unsupported-schema cache metadata left untouched; use a compatible `gat` \
         version to repair it.",
    )
}

fn purge_temporary_guidance() -> UserLine {
    UserLine::compose([
        UserLine::authored("Run "),
        UserLine::authored("`gat system clean cache --purge-temporary`").unbroken(),
        UserLine::authored(" to remove unverified temporary objects."),
    ])
}

fn cache_db_row(db: CacheDbState) -> (SystemRow, Vec<UserLine>) {
    match db {
        CacheDbState::Absent => (
            SystemRow::new(
                SystemStatus::Success,
                UserLine::identifier("cache metadata"),
                "absent",
                RowDetail::authored("absent"),
            ),
            Vec::new(),
        ),
        CacheDbState::Healthy => (
            SystemRow::new(
                SystemStatus::Success,
                UserLine::identifier("cache metadata"),
                "healthy",
                RowDetail::authored("healthy"),
            ),
            Vec::new(),
        ),
        CacheDbState::UnsupportedVersion(version) => (
            SystemRow::new(
                SystemStatus::Warning,
                UserLine::identifier("cache metadata"),
                "unsupported",
                RowDetail::message(UserLine::compose([
                    UserLine::authored("metadata format version "),
                    UserLine::number(version),
                    UserLine::authored(" is not supported by this build"),
                ])),
            ),
            vec![compatible_cache_version_guidance()],
        ),
        CacheDbState::Unreadable(reason) => {
            let problem = problem::db_unreadable_problem(reason);
            (
                SystemRow::new(
                    SystemStatus::Warning,
                    UserLine::identifier("cache metadata"),
                    "disabled",
                    RowDetail::composed("disabled (", problem, ")"),
                ),
                vec![UserLine::compose([
                    UserLine::authored("Run `"),
                    UserLine::identifier("gat system repair cache"),
                    UserLine::authored("` to rebuild cache metadata."),
                ])],
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
    let mut rows = vec![db_row];
    if temporary == 0 {
        rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("temporary objects"),
            "none",
            RowDetail::authored("none"),
        ));
    } else {
        rows.push(SystemRow::new(
            SystemStatus::Warning,
            UserLine::identifier("temporary objects"),
            count_annotation(temporary, "unverified"),
            RowDetail::message(UserLine::compose([
                UserLine::number(temporary as i64),
                UserLine::authored(" unverified temp file"),
                UserLine::authored(if temporary == 1 { "" } else { "s" }),
            ])),
        ));
        footer.push(purge_temporary_guidance());
    }
    (
        SystemGroup {
            hints: Vec::new(),
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
            "unsupported",
            RowDetail::message(UserLine::compose([
                UserLine::authored("metadata format version "),
                UserLine::number(*version),
                UserLine::authored(" is not supported by this build; refusing to rewrite it"),
            ])),
        ),
        CacheRepair::AlreadyValid => SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("cache metadata"),
            "valid",
            RowDetail::authored("already valid"),
        ),
        CacheRepair::Rebuilt => SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("cache metadata"),
            "rebuilt",
            RowDetail::authored("rebuilt and verified"),
        ),
    };
    let footer = if matches!(repair, CacheRepair::UnsupportedVersion { .. }) {
        vec![compatible_cache_version_guidance()]
    } else {
        Vec::new()
    };
    (
        SystemGroup {
            hints: Vec::new(),
            title: UserLine::identifier("Cache"),
            rows: vec![row],
        },
        footer,
    )
}

fn render_cache_clean(clean: &CacheClean) -> (SystemGroup, Vec<UserLine>) {
    use gat_command::TemporaryCleanOutcome;
    let CacheClean {
        temporary,
        objects_purged,
    } = clean;
    let mut rows = Vec::new();
    let mut footer = Vec::new();

    match temporary {
        TemporaryCleanOutcome::NonePresent => rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("temporary"),
            "none",
            RowDetail::authored("no temporary objects"),
        )),
        TemporaryCleanOutcome::Preserved { count } => {
            footer.push(purge_temporary_guidance());

            rows.push(SystemRow::new(
                SystemStatus::Warning,
                UserLine::identifier("temporary"),
                count_annotation(*count, "preserved"),
                RowDetail::message(UserLine::compose([
                    UserLine::number(*count as i64),
                    UserLine::authored(" temporary object"),
                    UserLine::authored(if *count == 1 { "" } else { "s" }),
                    UserLine::authored(" preserved"),
                ])),
            ));
        }
        TemporaryCleanOutcome::NoneToPurge => rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("temporary"),
            "none",
            RowDetail::authored("no temporary objects to purge"),
        )),
        TemporaryCleanOutcome::Purged { count } => rows.push(SystemRow::new(
            SystemStatus::Success,
            UserLine::identifier("temporary"),
            count_annotation(*count, "purged"),
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
            count_annotation(*purged, "purged"),
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
            title: UserLine::identifier("Cache"),
            rows,
        },
        footer,
    )
}

fn render_git(fact: GitFact) -> (SystemGroup, Vec<UserLine>) {
    match fact {
        GitFact::Inspect(inspect) => render_git_inspect(&inspect),
        GitFact::Repair(repair) => render_git_repair(&repair),
        GitFact::Clean(clean) => render_git_clean(&clean),
    }
}

fn git_group(
    status: SystemStatus,
    annotation: &'static str,
    detail: &'static str,
) -> (SystemGroup, Vec<UserLine>) {
    (
        SystemGroup {
            hints: Vec::new(),
            title: UserLine::identifier("Git"),
            rows: vec![SystemRow::new(
                status,
                UserLine::identifier("excludes"),
                annotation,
                RowDetail::authored(detail),
            )],
        },
        Vec::new(),
    )
}

fn render_git_inspect(inspect: &GitInspect) -> (SystemGroup, Vec<UserLine>) {
    let (status, annotation, detail) = match inspect {
        GitInspect::Current => (SystemStatus::Success, "current", "current"),
        GitInspect::Stale => (SystemStatus::Warning, "stale", "stale, will be regenerated"),
        GitInspect::PresentButUnvalidated => (
            SystemStatus::Warning,
            "unvalidated",
            "present but could not be validated",
        ),
        GitInspect::UnableToDeriveExpected => (
            SystemStatus::Error,
            "unavailable",
            "could not determine the expected excludes",
        ),
    };
    git_group(status, annotation, detail)
}

fn render_git_repair(repair: &GitRepair) -> (SystemGroup, Vec<UserLine>) {
    let (status, annotation, detail) = match repair {
        GitRepair::Rebuilt => (SystemStatus::Success, "rebuilt", "rebuilt"),
        GitRepair::AlreadyCurrent => (SystemStatus::Success, "current", "already current"),
    };
    git_group(status, annotation, detail)
}

fn render_git_clean(clean: &GitClean) -> (SystemGroup, Vec<UserLine>) {
    let (status, annotation, detail) = match clean {
        GitClean::NoManagedArtifacts => (
            SystemStatus::Success,
            "absent",
            "no managed excludes present",
        ),
        GitClean::RemovedStale => (
            SystemStatus::Success,
            "removed",
            "removed stale managed excludes",
        ),
        GitClean::NoStaleArtifacts => (
            SystemStatus::Success,
            "current",
            "managed excludes already current",
        ),
        GitClean::PreservedUnvalidated => (
            SystemStatus::Warning,
            "unvalidated",
            "managed excludes could not be validated, preserved",
        ),
    };
    git_group(status, annotation, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_preserves_the_strongest_group_severity_in_either_order() {
        for reverse in [false, true] {
            for (fact, expected, summary) in [
                (GitInspect::Current, SystemStatus::Success, "healthy"),
                (
                    GitInspect::Stale,
                    SystemStatus::Warning,
                    "attention required",
                ),
                (
                    GitInspect::UnableToDeriveExpected,
                    SystemStatus::Error,
                    "attention required",
                ),
            ] {
                let mut facts = vec![
                    DomainFact::Git(GitFact::Inspect(fact)),
                    DomainFact::Git(GitFact::Inspect(GitInspect::Current)),
                ];
                if reverse {
                    facts.reverse();
                }
                let report = render(gat_command::SystemOutcome {
                    verb: gat_command::SystemVerb::Inspect,
                    facts,
                });
                assert_eq!(report.status, expected);
                assert_eq!(report.summary.as_str(), summary);
            }
        }
    }

    #[test]
    fn repair_heading_preserves_an_invalid_live_lock_error() {
        let report = render(gat_command::SystemOutcome {
            verb: gat_command::SystemVerb::Repair,
            facts: vec![DomainFact::Lock(LockFact::Repair(
                LockRepair::RecoveryNotSelected {
                    reason: gat_command::RecoverySelectionFailure::NoMatch,
                    state: LockState {
                        live: LiveLockState::Invalid {
                            reason: gat_command::LiveLockInvalidReason::NeitherFileNorShardTree,
                        },
                        transactions: Vec::new(),
                    },
                },
            ))],
        });
        assert_eq!(report.status, SystemStatus::Error);
        assert_eq!(report.groups[0].status(), SystemStatus::Error);
        assert_eq!(report.summary.as_str(), "incomplete");
        assert_eq!(report.groups[0].explanations().count(), 2);
    }

    #[test]
    fn recovery_selection_failure_retains_its_reason_without_a_live_lock() {
        for (reason, expected) in [
            (
                gat_command::RecoverySelectionFailure::NoMatch,
                "recovery: no transaction matches the requested recovery choice",
            ),
            (
                gat_command::RecoverySelectionFailure::Ambiguous,
                "recovery: multiple transactions support the requested recovery choice; select one with `--transaction`",
            ),
        ] {
            let (group, _) = render_lock_repair(LockRepair::RecoveryNotSelected {
                reason,
                state: LockState {
                    live: LiveLockState::Missing,
                    transactions: Vec::new(),
                },
            });
            assert_eq!(group.status(), SystemStatus::Warning);
            assert_eq!(group.explanations().next().unwrap().as_str(), expected);
        }
    }

    #[test]
    fn explicit_recovery_commands_are_atomic_across_dynamic_fragments() {
        let line = explicit_recovery_command("txn-123", RecoveryChoice::RestoreBackup);
        let command = "`gat system repair lock --restore-backup --transaction txn-123`";
        assert!(line.wrapping_words().any(|word| word == command));
        let rendered = super::super::flow::wrap("", "", &line, 40);
        assert!(rendered.lines().any(|line| line == command));
        assert!(rendered.ends_with("to recover it."));
    }

    #[test]
    fn completed_maintenance_reports_results_once_as_successes() {
        for choice in [RecoveryChoice::RestoreBackup, RecoveryChoice::PromoteStaged] {
            let (group, _) = render_lock_repair(LockRepair::Recovered {
                txn_id: "recovery".into(),
                choice,
                state: LockState {
                    live: LiveLockState::Valid {
                        shard_levels: gat_core::lock::LockShardLevels::FLAT,
                        entries: 0,
                    },
                    transactions: Vec::new(),
                },
            });
            assert_eq!(group.rows.len(), 1);
            assert_eq!(group.rows[0].status, SystemStatus::Success);
            assert_eq!(group.rows[0].detail.line(), &recovery_result_text(choice));
        }
        let (group, _) = render_lock_clean(&LockClean {
            removed: 2,
            unresolved: false,
        });
        assert_eq!(group.rows.len(), 1);
        assert_eq!(group.rows[0].status, SystemStatus::Success);
        assert_eq!(
            group.rows[0].detail.as_str(),
            "removed 2 disposable transaction directories"
        );
        let (group, _) = render_git_repair(&GitRepair::Rebuilt);
        assert_eq!(group.status(), SystemStatus::Success);
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
        assert_eq!(group.rows[1].detail.as_str(), "purged 2 cached objects");
    }

    #[test]
    fn recovered_action_does_not_hide_remaining_transaction_errors() {
        let report = render(gat_command::SystemOutcome {
            verb: gat_command::SystemVerb::Repair,
            facts: vec![DomainFact::Lock(LockFact::Repair(LockRepair::Recovered {
                txn_id: "recovered".into(),
                choice: RecoveryChoice::RestoreBackup,
                state: LockState {
                    live: LiveLockState::Valid {
                        shard_levels: gat_core::lock::LockShardLevels::FLAT,
                        entries: 1,
                    },
                    transactions: vec![TransactionState {
                        id: "unrelated".into(),
                        kind: TransactionKind::Malformed {
                            reason: gat_command::TransactionMalformedReason::UnrecognizedPhase,
                        },
                    }],
                },
            }))],
        });
        assert_eq!(report.status, SystemStatus::Error);
        assert_eq!(report.summary.as_str(), "incomplete");
        let group = &report.groups[0];
        assert_eq!(
            group.rows[0].detail.line(),
            &recovery_result_text(RecoveryChoice::RestoreBackup)
        );
        assert!(
            group
                .explanations()
                .any(|line| line.as_str().starts_with("txn unrelated:"))
        );
        assert!(
            group
                .rows
                .iter()
                .any(|row| row.label.as_str() == "gat.lock" && row.status == SystemStatus::Success)
        );
    }

    #[test]
    fn successful_cleanup_does_not_hide_preserved_recovery_state() {
        let (group, footer) = render_lock_clean(&LockClean {
            removed: 2,
            unresolved: true,
        });
        assert_eq!(group.status(), SystemStatus::Warning);
        assert_eq!(group.rows.len(), 2);
        assert_eq!(group.rows[0].status, SystemStatus::Success);
        assert_eq!(group.rows[1].status, SystemStatus::Warning);
        assert!(!footer.is_empty());
    }
}
