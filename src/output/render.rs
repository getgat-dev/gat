//! Durable rendering of completed command outcomes to borrowed writers.
//! This module owns wording, styling, and stream ordering; the process adapter
//! owns destinations, color policy, and write-failure exit codes.

use crate::app::Outcome;
use crate::output::rows;
use crate::output::rows::RowDetail;
use crate::output::terminal as ui;
use crate::output::{Output, Stream, WriteFailure};
use crate::presentation::UserLine;
use gat_command::{DiffOutcome, DiffTarget};

/// Composes `<count> <suffix>` (e.g. `24 file(s)`) as a [`UserLine`],
/// the shared shape for every count-style summary/footer/heading,
/// avoiding each call site hand-rolling
/// its own `format!("{n} ...")`.
fn count_line(count: i64, suffix: &'static str) -> UserLine {
    UserLine::compose([UserLine::number(count), UserLine::authored(suffix)])
}

fn render_message(output: &mut Output<'_>, message: &rows::Message) -> Result<(), WriteFailure> {
    match message.kind() {
        rows::MessageKind::Action => ui::action(output, message.line()),
        rows::MessageKind::Success => ui::success(output, message.line()),
        rows::MessageKind::Caution => ui::caution(output, message.line()),
    }
}

/// Renders one always-on `gat init` Git-integration step (the merge-driver
/// config, or its `info/attributes` selector) -- `label` names which one.
/// Every outcome is quiet/routine: `gat init` is a declarative convergence
/// command, so "already installed" and "already absent" are both expected
/// steady states, not something the user needs to act on.
fn render_git_integration_step(
    output: &mut Output<'_>,
    label: &str,
    outcome: gat_command::InitGitIntegrationOutcome,
) -> Result<(), WriteFailure> {
    let (prefix, suffix) = match outcome {
        gat_command::InitGitIntegrationOutcome::Installed => ("Installed ", "."),
        gat_command::InitGitIntegrationOutcome::AlreadyInstalled => ("", " already installed."),
        gat_command::InitGitIntegrationOutcome::Removed => ("Removed ", "."),
        gat_command::InitGitIntegrationOutcome::AlreadyAbsent => ("", " already absent."),
    };
    ui::success(
        output,
        &UserLine::compose([
            UserLine::authored(prefix),
            UserLine::identifier(label),
            UserLine::authored(suffix),
        ]),
    )
}

/// Prints the shared caution for a history-aware command (`status
/// --remote`, `push`, `fetch`, `pull`) whose explicit selection may be
/// incomplete because the repository is a shallow clone -- one shared
/// message instead of one per command, since the underlying condition and
/// recommended action (unshallow, or accept the local view) are the same
/// everywhere. A no-op when `shallow` is `false`.
fn render_shallow_caution(output: &mut Output<'_>, shallow: bool) -> Result<(), WriteFailure> {
    if shallow {
        ui::caution(
            output,
            &UserLine::authored(
                "this repository is a shallow clone: history beyond the shallow boundary is \
             unavailable, so the selected history may be incomplete",
            ),
        )?;
    }
    Ok(())
}

/// Reports a `routes:` entry `gat mount add`/`update` bootstrapped at a
/// mount's target: an action line under the primary success
/// message, noting the remote it now routes to (and, if it replaced a
/// different existing route, what changed).
fn render_mount_route_bootstrap(
    output: &mut Output<'_>,
    route: &gat_command::MountRouteBootstrap,
) -> Result<(), WriteFailure> {
    let text = match &route.previous {
        Some(previous) => UserLine::compose([
            UserLine::authored("Route `"),
            UserLine::identifier(route.name.as_str()),
            UserLine::authored("` updated: `"),
            UserLine::identifier(route.path.as_str()),
            UserLine::authored("` → `"),
            UserLine::identifier(route.remote.as_str()),
            UserLine::authored("` (was `"),
            UserLine::identifier(previous.as_str()),
            UserLine::authored("`)."),
        ]),
        None => UserLine::compose([
            UserLine::authored("Route `"),
            UserLine::identifier(route.name.as_str()),
            UserLine::authored("` added: `"),
            UserLine::identifier(route.path.as_str()),
            UserLine::authored("` → `"),
            UserLine::identifier(route.remote.as_str()),
            UserLine::authored("`."),
        ]),
    };
    render_message(
        output,
        &rows::Message::new(rows::MessageKind::Success, text),
    )?;
    Ok(())
}

fn config_scalar_line(value: &gat_command::ConfigScalarValue) -> UserLine {
    match value {
        gat_command::ConfigScalarValue::CacheLocation(value) => UserLine::path(value.as_path()),
        gat_command::ConfigScalarValue::Path(value) => UserLine::path(value),
        gat_command::ConfigScalarValue::IngestStrategy(value) => {
            UserLine::identifier(&value.to_string())
        }
        gat_command::ConfigScalarValue::Boolean(value) => {
            UserLine::authored(if *value { "true" } else { "false" })
        }
        gat_command::ConfigScalarValue::ShardLevels(value) => {
            UserLine::number(i64::from(value.get()))
        }
    }
}

fn render_config_values(
    output: &mut Output<'_>,
    key: gat_core::config_keys::ConfigKey,
    values: &[UserLine],
    source: gat_command::ConfigSource,
) -> Result<(), WriteFailure> {
    resource_heading(output, "Config", UserLine::authored(key.as_str()))?;
    output.stdout(format_args!(""))?;
    if values.is_empty() {
        output.stdout(format_args!("  (empty)"))?;
    } else {
        for value in values {
            output.stdout(format_args!("  {value}"))?;
        }
    }
    let source = match source {
        gat_command::ConfigSource::Default => UserLine::authored("built-in default"),
        gat_command::ConfigSource::Scope(scope) => scope_line(scope),
        gat_command::ConfigSource::CacheEnvironment => {
            UserLine::authored("GAT_CACHE_DIR environment variable")
        }
    };
    let metadata = UserLine::compose([UserLine::authored("Source: "), source]);
    output.stdout(format_args!("  {}", ui::dim(metadata.as_str())))
}

fn resource_heading(
    output: &mut Output<'_>,
    label: &'static str,
    value: UserLine,
) -> Result<(), WriteFailure> {
    output.stdout(format_args!(
        "{}",
        ui::success_heading(&UserLine::authored(label), &value)
    ))
}

fn resource_details(
    output: &mut Output<'_>,
    label: &'static str,
    value: UserLine,
    fields: &[(UserLine, UserLine)],
    chosen_in: Option<gat_core::config::ConfigScope>,
    defined_in: Option<gat_core::config::ConfigScope>,
) -> Result<(), WriteFailure> {
    resource_heading(output, label, value)?;
    let mut scopes = Vec::new();
    for (label, scope) in [("Chosen in: ", chosen_in), ("Defined in: ", defined_in)] {
        if let Some(scope) = scope {
            if !scopes.is_empty() {
                scopes.push(UserLine::authored("  ·  "));
            }
            scopes.extend([UserLine::authored(label), scope_line(scope)]);
        }
    }
    if !scopes.is_empty() {
        output.stdout(format_args!(
            "  {}",
            ui::dim(UserLine::compose(scopes).as_str())
        ))?;
    }
    if !fields.is_empty() {
        output.stdout(format_args!(""))?;
        for line in ui::fields(fields) {
            output.stdout(format_args!("{line}"))?;
        }
    }
    Ok(())
}

fn resource_list(
    output: &mut Output<'_>,
    label: &'static str,
    configured: usize,
    empty_hint: &'static str,
    rows: &[rows::ListRow],
) -> Result<(), WriteFailure> {
    resource_heading(
        output,
        label,
        if configured == 0 {
            UserLine::authored("none configured")
        } else {
            count_line(configured as i64, " configured")
        },
    )?;
    if !rows.is_empty() {
        output.stdout(format_args!(""))?;
        render_rows(output, rows, Stream::Stdout)?;
    }
    if configured == 0 {
        output.stdout(format_args!(""))?;
        output.stdout(format_args!(
            "{}",
            ui::list_hint(&UserLine::authored(empty_hint))
        ))?;
    }
    Ok(())
}

fn scope_line(scope: gat_core::config::ConfigScope) -> UserLine {
    UserLine::authored(match scope {
        gat_core::config::ConfigScope::Global => "global",
        gat_core::config::ConfigScope::Project => "project",
        gat_core::config::ConfigScope::Local => "local",
    })
}

fn resource_confirmation(
    output: &mut Output<'_>,
    label: &'static str,
    value: UserLine,
) -> Result<(), WriteFailure> {
    ui::success(
        output,
        &UserLine::compose([UserLine::authored(label), value]),
    )
}

fn revealed_definition(
    output: &mut Output<'_>,
    scope: gat_core::config::ConfigScope,
) -> Result<(), WriteFailure> {
    ui::action(
        output,
        &UserLine::compose([
            UserLine::authored("Revealed definition in: "),
            scope_line(scope),
        ]),
    )
}

fn selection_fields(record: &gat_command::SelectionRecord) -> Vec<(UserLine, UserLine)> {
    let mut fields = vec![
        (
            UserLine::authored("Default"),
            UserLine::authored(if record.is_default { "yes" } else { "no" }),
        ),
        (
            UserLine::authored("Path"),
            UserLine::gat_subpath(&record.definition.path),
        ),
    ];
    for (label, patterns) in [
        ("Include", &record.definition.include),
        ("Exclude", &record.definition.exclude),
    ] {
        for pattern in patterns.as_deref().unwrap_or_default() {
            fields.push((
                UserLine::authored(label),
                UserLine::identifier(pattern.as_str()),
            ));
        }
    }
    fields
}

fn render_saved_selection(
    output: &mut Output<'_>,
    outcome: &gat_command::SelectionOutcome,
) -> Result<(), WriteFailure> {
    use gat_command::SelectionOutcome as O;
    match outcome {
        O::List(records) => {
            let rows: Vec<_> = records
                .iter()
                .map(|record| {
                    rows::ListRow::with_metadata(
                        rows::ListStatus::Success,
                        UserLine::identifier(record.name.as_str()),
                        RowDetail::message(UserLine::compose([
                            UserLine::gat_subpath(&record.definition.path),
                            UserLine::authored(if record.is_default { "  (default)" } else { "" }),
                        ])),
                    )
                })
                .collect();
            resource_list(
                output,
                "Selections",
                records.len(),
                "Run `gat selection add <name>`",
                &rows,
            )?;
        }
        O::Show(record) => resource_details(
            output,
            "Selection",
            UserLine::identifier(record.name.as_str()),
            &selection_fields(record),
            None,
            Some(record.scope),
        )?,
        O::Saved { name } => resource_confirmation(
            output,
            "Saved selection: ",
            UserLine::identifier(name.as_str()),
        )?,
        O::Removed { name, revealed } => {
            resource_confirmation(
                output,
                "Removed selection: ",
                UserLine::identifier(name.as_str()),
            )?;
            if let Some(scope) = revealed {
                revealed_definition(output, *scope)?;
            }
        }
        O::Default { record, chosen_in } => {
            let mut fields = Vec::new();
            if let Some(record) = record {
                fields.extend(selection_fields(record));
            }
            resource_details(
                output,
                "Default selection",
                record.as_ref().map_or_else(
                    || UserLine::authored("none (whole repository)"),
                    |r| UserLine::identifier(r.name.as_str()),
                ),
                &fields,
                *chosen_in,
                record.as_ref().map(|record| record.scope),
            )?;
        }
    }
    Ok(())
}

/// Renders `gat mount show <NAME>`: the mount's upstream/target/selection
/// facts plus derived storage-route and scope information, as a labeled
/// key/value block under a success heading.
fn render_mount_details(
    output: &mut Output<'_>,
    details: &gat_command::MountDetails,
) -> Result<(), WriteFailure> {
    // Display the effective named route explicitly
    // (name, matched path, remote), clearly distinguishing repository-
    // default fallback from a configured route.
    let route = match &details.route {
        Some(matched) => UserLine::compose([
            UserLine::authored("`"),
            UserLine::identifier(matched.name.as_str()),
            UserLine::authored("` ("),
            UserLine::gat_path(&matched.path),
            UserLine::authored(") → "),
            UserLine::identifier(matched.remote.as_str()),
        ]),
        None => match &details.default_remote {
            Some(remote) => UserLine::compose([
                UserLine::authored("(default remote → "),
                UserLine::identifier(remote.as_str()),
                UserLine::authored(")"),
            ]),
            None => UserLine::authored("(no remote configured)"),
        },
    };
    let mut fields: Vec<(UserLine, UserLine)> = vec![
        (
            "URL".into(),
            UserLine::redacted_url(&crate::redaction::RedactedUrl::render(
                details.location.as_location_str(),
            )),
        ),
        ("Path".into(), UserLine::gat_subpath(&details.path)),
        ("Target".into(), UserLine::gat_path(&details.target)),
        (
            "Revision".into(),
            match &details.revision {
                Some(rev) => UserLine::identifier(rev.as_str()),
                None => UserLine::authored("(default branch)"),
            },
        ),
        (
            "Locked revision".into(),
            match &details.revision_lock {
                Some(rev_lock) => UserLine::oid(&rev_lock.to_string()),
                None => UserLine::authored("(unset)"),
            },
        ),
        (
            "Tracked files".into(),
            UserLine::number(details.tracked_rows as i64),
        ),
        ("Route".into(), route),
    ];
    for (label, patterns) in [("Include", &details.include), ("Exclude", &details.exclude)] {
        for pattern in patterns {
            fields.push((
                UserLine::authored(label),
                UserLine::identifier(pattern.as_str()),
            ));
        }
    }
    resource_details(
        output,
        "Mount",
        UserLine::identifier(details.name.as_str()),
        &fields,
        None,
        Some(details.scope),
    )
}

/// Renders `gat route show <NAME>`: the route's path/remote/scope facts
/// as a labeled key/value block under a success heading (same shape as
/// `render_mount_details`).
fn render_route_details(
    output: &mut Output<'_>,
    details: &gat_command::RouteDetails,
) -> Result<(), WriteFailure> {
    let fields = vec![
        ("Path".into(), UserLine::gat_path(&details.path)),
        (
            "Remote".into(),
            UserLine::identifier(details.remote.as_str()),
        ),
    ];
    resource_details(
        output,
        "Route",
        UserLine::identifier(details.name.as_str()),
        &fields,
        None,
        Some(details.scope),
    )
}

/// Authors the CLI-facing label/wording for one `gat diff` row's change,
/// keeping "new"/"removed"/"changed" prose in root presentation rather
/// than the command crate that only computes the typed change.
fn diff_row_metadata(change: &gat_engine::RowChange) -> (rows::ListStatus, &'static str) {
    match change {
        gat_engine::RowChange::Added { .. } => (rows::ListStatus::Added, "new"),
        gat_engine::RowChange::Removed => (rows::ListStatus::Deleted, "removed"),
        gat_engine::RowChange::Modified { .. } => (rows::ListStatus::Modified, "changed"),
        gat_engine::RowChange::Unchanged { .. } => {
            unreachable!("`Unchanged::Drop` comparisons never yield unchanged rows")
        }
    }
}

fn diff_target_label(target: &DiffTarget) -> &str {
    match target {
        DiffTarget::Revision(revision) => revision.as_str(),
        DiffTarget::WorkingTree => "working tree",
    }
}

fn with_mount_metadata(metadata: UserLine, mount: Option<&gat_core::name::MountName>) -> UserLine {
    match mount {
        None => metadata,
        Some(name) => {
            let prefix = if metadata.as_str().is_empty() {
                "(mount "
            } else {
                " (mount "
            };
            UserLine::compose([
                metadata,
                UserLine::authored(prefix),
                UserLine::identifier(name.as_str()),
                UserLine::authored(")"),
            ])
        }
    }
}

/// Authors the CLI-facing label/wording for one `gat status` row, keeping
/// "new, cached"/"missing from cache; run `gat fetch`" prose in root
/// presentation rather than the command crate that only computes the typed
/// change and cache presence.
fn status_row_to_list_row(row: &gat_command::StatusRow) -> rows::ListRow {
    let (status, prefix) = match row.change {
        gat_engine::RowChange::Added { .. } => (rows::ListStatus::Added, "new, "),
        gat_engine::RowChange::Modified { .. } => (rows::ListStatus::Modified, ""),
        gat_engine::RowChange::Unchanged { .. } => (rows::ListStatus::Success, ""),
        gat_engine::RowChange::Removed => {
            return match &row.mount {
                Some(mount) => rows::ListRow::with_metadata(
                    rows::ListStatus::Deleted,
                    UserLine::gat_path(&row.path),
                    RowDetail::message(with_mount_metadata(UserLine::authored(""), Some(mount))),
                ),
                None => {
                    rows::ListRow::new(rows::ListStatus::Deleted, UserLine::gat_path(&row.path))
                }
            };
        }
    };
    let cache_note = match row
        .cache_presence
        .expect("a non-removed row has an oid to check the cache for")
    {
        gat_command::CachePresence::Present => "cached",
        gat_command::CachePresence::Missing => "missing from cache; run `gat fetch`",
    };
    rows::ListRow::with_metadata(
        status,
        UserLine::gat_path(&row.path),
        RowDetail::message(with_mount_metadata(
            UserLine::compose([UserLine::authored(prefix), UserLine::authored(cache_note)]),
            row.mount.as_ref(),
        )),
    )
}

/// Separate the hint block from the preceding output on the same stream.
fn render_hints(
    output: &mut Output<'_>,
    hints: &[UserLine],
    stream: Stream,
) -> Result<(), WriteFailure> {
    if !hints.is_empty() {
        match stream {
            Stream::Stdout => output.stdout(format_args!(""))?,
            Stream::Stderr => output.stderr(format_args!(""))?,
        }
    }
    for hint in hints {
        let line = ui::list_hint(hint);
        match stream {
            Stream::Stdout => output.stdout(format_args!("{line}"))?,
            Stream::Stderr => output.stderr(format_args!("{line}"))?,
        }
    }
    Ok(())
}

fn render_rows(
    output: &mut Output<'_>,
    rows: &[rows::ListRow],
    stream: Stream,
) -> Result<(), WriteFailure> {
    let items = rows.iter().map(|row| {
        let status = row.status();
        match row.metadata() {
            Some(detail) => ui::ListItem::with_metadata(status, row.path(), detail),
            None => ui::ListItem::new(status, row.path()),
        }
    });
    for line in ui::list_items(items) {
        match stream {
            Stream::Stdout => output.stdout(format_args!("{line}"))?,
            Stream::Stderr => output.stderr(format_args!("{line}"))?,
        }
    }
    Ok(())
}

fn render_system(
    output: &mut Output<'_>,
    outcome: gat_command::SystemOutcome,
) -> Result<(), WriteFailure> {
    use crate::output::system::SystemStatus;

    let outcome = crate::output::system::render(outcome);
    let status_of = |status: SystemStatus| match status {
        SystemStatus::Success => ui::Status::Success,
        SystemStatus::Warning => ui::Status::Warning,
        SystemStatus::Error => ui::Status::Error,
    };
    output.stdout(format_args!(
        "{}",
        ui::status_heading(status_of(outcome.status), &outcome.title, &outcome.summary)
    ))?;
    for group in &outcome.groups {
        output.stdout(format_args!(""))?;
        output.stdout(format_args!(
            "{}",
            ui::status_group(status_of(group.status), &group.title)
        ))?;
        let items = group.rows.iter().map(|row| match &row.detail {
            Some(detail) => {
                ui::ListItem::with_metadata(status_of(row.status), &row.label, detail.line())
            }
            None => ui::ListItem::new(status_of(row.status), &row.label),
        });
        for line in ui::list_items(items) {
            output.stdout(format_args!("{line}"))?;
        }
        render_hints(output, &group.hints, Stream::Stdout)?;
    }
    if !outcome.footer.is_empty() {
        output.stdout(format_args!(""))?;
        for line in &outcome.footer {
            output.stdout(format_args!("{line}"))?;
        }
    }
    Ok(())
}

fn push_skip_row(skip: &gat_command::PushSkip) -> Option<rows::ListRow> {
    let detail = match &skip.reason {
        gat_command::PushSkipReason::MountOwned { .. } => return None,
        gat_command::PushSkipReason::CacheMissing => RowDetail::authored(
            "not in cache; run `gat add` or `gat fetch` (fetch the selected history first for \
             a historical object)",
        ),
        gat_command::PushSkipReason::CacheCorrupt => {
            RowDetail::authored("cached object corrupt; re-add or re-fetch it before pushing")
        }
    };
    Some(rows::ListRow::with_metadata(
        rows::ListStatus::Skipped,
        UserLine::gat_path(&skip.path),
        detail,
    ))
}

fn missing_rows(outcome: &gat_engine::SyncOutcome) -> Vec<rows::ListRow> {
    outcome
        .missing
        .iter()
        .map(|(path, oid)| {
            rows::ListRow::with_metadata(
                rows::ListStatus::Conflict,
                UserLine::gat_path(path),
                RowDetail::message(UserLine::compose([
                    UserLine::authored("object "),
                    UserLine::oid_value(oid),
                    UserLine::authored(
                        " missing from cache; run `gat fetch`/`gat pull`, or configure a remote",
                    ),
                ])),
            )
        })
        .collect()
}

fn conflict_rows(outcome: &gat_engine::SyncOutcome) -> Vec<rows::ListRow> {
    outcome
        .conflicts
        .iter()
        .map(|path| {
            rows::ListRow::with_metadata(
                rows::ListStatus::Conflict,
                UserLine::gat_path(path),
                RowDetail::authored(
                    "locally modified; left untouched (use `gat sync --force` to overwrite)",
                ),
            )
        })
        .collect()
}

fn corrupted_rows(outcome: &gat_engine::SyncOutcome) -> Vec<rows::ListRow> {
    outcome
        .corrupted
        .iter()
        .map(|(path, oid)| {
            rows::ListRow::with_metadata(
                rows::ListStatus::Conflict,
                UserLine::gat_path(path),
                RowDetail::message(UserLine::compose([
                    UserLine::authored("cache object "),
                    UserLine::oid_value(oid),
                    UserLine::authored(
                        " is corrupted; run `gat sync --repair` to re-fetch it from a remote",
                    ),
                ])),
            )
        })
        .collect()
}

fn repair_failure_rows(repair_failures: &[gat_command::RepairFailure]) -> Vec<rows::ListRow> {
    repair_failures
        .iter()
        .map(|failure| {
            let problem = crate::error::map::problem::repair_problem(failure.error.clone());
            let prefix = UserLine::compose([
                UserLine::authored("repair of object "),
                UserLine::oid_value(&failure.oid),
                UserLine::authored(" failed: "),
            ]);
            rows::ListRow::with_metadata(
                rows::ListStatus::Conflict,
                UserLine::gat_path(&failure.path),
                RowDetail::composed(prefix, problem, ""),
            )
        })
        .collect()
}

fn render_sync(
    output: &mut Output<'_>,
    outcome: &gat_command::SyncOutcome,
) -> Result<(), WriteFailure> {
    let sync = &outcome.outcome;
    let repair_failures = repair_failure_rows(&outcome.repair_failures);
    render_rows(output, &repair_failures, Stream::Stderr)?;

    if let Some(levels) = outcome.reshaped {
        let levels = levels.get();
        let shape = if levels == 0 {
            UserLine::authored("a flat gat.lock file")
        } else {
            UserLine::compose([
                UserLine::authored("gat.lock/ sharded into "),
                UserLine::number(i64::from(levels)),
                UserLine::authored(" level(s)"),
            ])
        };
        ui::success(
            output,
            &UserLine::compose([
                UserLine::authored("gat.lock reshaped to match lock.shard_levels ("),
                shape,
                UserLine::authored(")"),
            ]),
        )?;
    }

    let mut preceding_output = !repair_failures.is_empty() || outcome.reshaped.is_some();
    for (label, rows) in [
        ("Missing objects", missing_rows(sync)),
        ("Conflicts", conflict_rows(sync)),
        ("Corrupted objects", corrupted_rows(sync)),
    ] {
        if rows.is_empty() {
            continue;
        }
        if preceding_output {
            output.stderr(format_args!(""))?;
        }
        output.stderr(format_args!(
            "{}",
            ui::caution_heading(
                &UserLine::authored(label),
                &UserLine::number(rows.len() as i64)
            )
        ))?;
        output.stderr(format_args!(""))?;
        render_rows(output, &rows, Stream::Stderr)?;
        preceding_output = true;
    }
    if !sync.missing.is_empty() || !sync.conflicts.is_empty() || !sync.corrupted.is_empty() {
        output.stderr(format_args!(""))?;
    }

    let mut parts: Vec<UserLine> = [
        (sync.materialized, "materialized"),
        (sync.replaced, "replaced"),
        (sync.rematerialized, "rematerialized"),
        (sync.removed, "removed"),
        (outcome.fetched, "fetched"),
        (outcome.repaired, "repaired"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, label)| {
        UserLine::compose([
            UserLine::number(n as i64),
            UserLine::authored(" "),
            UserLine::authored(label),
        ])
    })
    .collect();
    if sync.excludes_changed {
        parts.push(if sync.dry_run {
            UserLine::compose([
                UserLine::authored("would update excludes ("),
                UserLine::number(sync.excludes_count as i64),
                UserLine::authored(" path(s))"),
            ])
        } else {
            UserLine::compose([
                UserLine::number(sync.excludes_count as i64),
                UserLine::authored(" path(s) excluded"),
            ])
        });
    }
    let summary = if parts.is_empty() {
        UserLine::authored("no changes")
    } else {
        let mut joined = Vec::new();
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                joined.push(UserLine::authored(", "));
            }
            joined.push(part);
        }
        UserLine::compose(joined)
    };
    let (kind, label) = if !outcome.completion.is_clean() {
        (rows::MessageKind::Caution, "Sync incomplete (")
    } else if sync.dry_run {
        (rows::MessageKind::Action, "Sync preview (")
    } else {
        (rows::MessageKind::Success, "Sync complete (")
    };
    render_message(
        output,
        &rows::Message::new(
            kind,
            UserLine::compose([UserLine::authored(label), summary, UserLine::authored(")")]),
        ),
    )?;
    render_hints(
        output,
        selection_scope_note(outcome.scope).as_slice(),
        Stream::Stderr,
    )?;
    render_shallow_caution(output, outcome.shallow)?;
    Ok(())
}

fn selection_scope_note(scope: gat_command::SelectionScope) -> Option<UserLine> {
    match scope {
        gat_command::SelectionScope::Unrestricted => None,
        gat_command::SelectionScope::Configured => Some(UserLine::authored(
            "Configured path selection applied; other paths were not checked. Use --path . to select the whole repository.",
        )),
        gat_command::SelectionScope::Explicit => Some(UserLine::authored(
            "Results cover selected paths only; other paths were not checked.",
        )),
    }
}

fn no_changes_summary(scope: gat_command::SelectionScope) -> UserLine {
    UserLine::authored(match scope {
        gat_command::SelectionScope::Unrestricted => "no changes",
        gat_command::SelectionScope::Explicit | gat_command::SelectionScope::Configured => {
            "no changes in selected paths"
        }
    })
}

/// Render a completed command's results and coverage notices to borrowed streams.
/// Stops at the first write failure. Partial output is not rolled back; callers
/// must never repeat the completed operation to recover from an output failure.
pub fn render(output: &mut Output<'_>, outcome: Outcome) -> Result<(), WriteFailure> {
    if let Outcome::System(system_outcome) = outcome {
        render_system(output, system_outcome)?;
        return Ok(());
    }
    let scope = match &outcome {
        Outcome::Status(
            gat_command::StatusOutcome::WorkingTree { scope, .. }
            | gat_command::StatusOutcome::NoMatchingFiles { scope },
        ) => *scope,
        Outcome::Diff(
            DiffOutcome::Changes { scope, .. } | DiffOutcome::NoChanges { scope, .. },
        ) => *scope,
        Outcome::ListedFiles(result) => result.scope,
        Outcome::Pushed(result) => result.scope,
        Outcome::RemoteStatus(result) => result.scope,
        Outcome::Fetched { scope, .. } => *scope,
        Outcome::Synced(result) | Outcome::Pulled(result) => result.scope,
        _ => gat_command::SelectionScope::Unrestricted,
    };
    match &outcome {
        Outcome::Initialized(outcome) => {
            render_message(
                output,
                &rows::Message::new(
                    rows::MessageKind::Success,
                    UserLine::compose([
                        UserLine::authored("gat initialized (cache at "),
                        UserLine::path(outcome.cache_location.display_path()),
                        UserLine::authored(")."),
                    ]),
                ),
            )?;
            match &outcome.hooks {
                gat_command::InitHooksOutcome::Installed(installed) => {
                    let installed = installed
                        .iter()
                        .map(|hook| hook.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    render_message(
                        output,
                        &rows::Message::new(
                            rows::MessageKind::Success,
                            UserLine::compose([
                                UserLine::authored("Installed Git hooks: "),
                                UserLine::identifier(&installed),
                                UserLine::authored(
                                    ". `gat sync` now runs automatically after checkout, merge/pull, and rebase/amend.",
                                ),
                            ]),
                        ),
                    )?;
                }
                gat_command::InitHooksOutcome::AlreadyInstalled => {
                    render_message(
                        output,
                        &rows::Message::authored(
                            rows::MessageKind::Success,
                            "Git hooks already installed.",
                        ),
                    )?;
                }
                gat_command::InitHooksOutcome::Removed(removed) => {
                    let removed = removed
                        .iter()
                        .map(|hook| hook.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    render_message(
                        output,
                        &rows::Message::new(
                            rows::MessageKind::Success,
                            UserLine::compose([
                                UserLine::authored("Removed Git hooks: "),
                                UserLine::identifier(&removed),
                                UserLine::authored("."),
                            ]),
                        ),
                    )?;
                }
                gat_command::InitHooksOutcome::AlreadyAbsent => {
                    render_message(
                        output,
                        &rows::Message::authored(
                            rows::MessageKind::Success,
                            "Git hooks already absent.",
                        ),
                    )?;
                }
            }
            render_git_integration_step(
                output,
                "gat.lock semantic merge driver",
                outcome.merge_driver,
            )?;
            render_git_integration_step(output, "gat.lock merge attributes", outcome.attributes)?;
            match &outcome.config {
                Some(gat_command::InitConfigOutcome::Created) => {
                    render_message(
                        output,
                        &rows::Message::authored(
                            rows::MessageKind::Success,
                            "Created gat.yaml (all keys commented out; edit as needed).",
                        ),
                    )?;
                }
                Some(gat_command::InitConfigOutcome::AlreadyPresent) => {
                    render_message(
                        output,
                        &rows::Message::authored(
                            rows::MessageKind::Success,
                            "gat.yaml already present.",
                        ),
                    )?;
                }
                None => {}
            }
            render_message(
                output,
                &rows::Message::authored(
                    rows::MessageKind::Action,
                    "Next: `gat add <path>` then `gat remote add origin s3://bucket/prefix`.",
                ),
            )?;
        }
        Outcome::Added(outcome) => {
            output.stdout(format_args!(
                "{}",
                ui::success_heading(
                    &UserLine::authored("Added"),
                    &count_line(outcome.added_count as i64, " file(s)")
                )
            ))?;
            let rows: Vec<rows::ListRow> = outcome
                .rows
                .iter()
                .filter(|row| row.file_count != Some(0))
                .map(|row| {
                    let path = match &row.path {
                        gat_core::path_scope::PathScope::Root => UserLine::authored("."),
                        gat_core::path_scope::PathScope::Path(path) => UserLine::gat_path(path),
                    };
                    match row.file_count {
                        Some(count) => rows::ListRow::with_metadata(
                            rows::ListStatus::Added,
                            path,
                            RowDetail::message(count_line(count as i64, " file(s)")),
                        ),
                        None => rows::ListRow::new(rows::ListStatus::Added, path),
                    }
                })
                .collect();
            if !rows.is_empty() {
                output.stdout(format_args!(""))?;
                render_rows(output, &rows, Stream::Stdout)?;
            }
            let hints = add_hints(outcome);
            render_hints(output, &hints, Stream::Stdout)?;
            output.stdout(format_args!(""))?;
            output.stdout(format_args!(
                "{}",
                ui::list_footer(&count_line(outcome.added_count as i64, " file(s)"))
            ))?;
        }
        Outcome::Removed(outcome) => {
            let rows: Vec<rows::ListRow> = outcome
                .paths
                .iter()
                .map(|path| rows::ListRow::new(rows::ListStatus::Deleted, UserLine::gat_path(path)))
                .collect();
            output.stdout(format_args!(
                "{}",
                ui::success_heading(
                    &UserLine::authored("Removed"),
                    &count_line(rows.len() as i64, " file(s)")
                )
            ))?;
            if !rows.is_empty() {
                output.stdout(format_args!(""))?;
                render_rows(output, &rows, Stream::Stdout)?;
            }
            output.stdout(format_args!(""))?;
            output.stdout(format_args!(
                "{}",
                ui::list_footer(&count_line(rows.len() as i64, " file(s)"))
            ))?;
        }
        Outcome::Selection(outcome) => render_saved_selection(output, outcome)?,
        Outcome::Remote(outcome) => match outcome {
            gat_command::RemoteOutcome::Default {
                name,
                chosen_in,
                defined_in,
            } => {
                resource_details(
                    output,
                    "Default remote",
                    name.as_ref().map_or_else(
                        || UserLine::authored("none"),
                        |n| UserLine::identifier(n.as_str()),
                    ),
                    &[],
                    *chosen_in,
                    *defined_in,
                )?;
            }
            gat_command::RemoteOutcome::Show { record, scope } => {
                let url = crate::redaction::render_remote_template(&record.url);
                let fields = vec![
                    ("URL".into(), UserLine::redacted_url(&url)),
                    (
                        "Default".into(),
                        UserLine::authored(if record.is_default { "yes" } else { "no" }),
                    ),
                ];
                resource_details(
                    output,
                    "Remote",
                    UserLine::identifier(record.name.as_str()),
                    &fields,
                    None,
                    Some(*scope),
                )?;
            }
            gat_command::RemoteOutcome::List(records) => {
                let rows: Vec<rows::ListRow> = records
                    .iter()
                    .map(|record| {
                        let display_url = crate::redaction::render_remote_template(&record.url);
                        let detail = if record.is_default {
                            UserLine::compose([
                                UserLine::redacted_url(&display_url),
                                UserLine::authored("  (default)"),
                            ])
                        } else {
                            UserLine::redacted_url(&display_url)
                        };
                        rows::ListRow::with_metadata(
                            rows::ListStatus::Success,
                            UserLine::identifier(record.name.as_str()),
                            RowDetail::message(detail),
                        )
                    })
                    .collect();
                resource_list(
                    output,
                    "Remotes",
                    records.len(),
                    "Run `gat remote add <name> <url>`",
                    &rows,
                )?;
            }
            gat_command::RemoteOutcome::Added { name, url }
            | gat_command::RemoteOutcome::Updated { name, url } => {
                let display_url = crate::redaction::render_remote_template(url);
                render_message(
                    output,
                    &rows::Message::new(
                        rows::MessageKind::Success,
                        UserLine::compose([
                            UserLine::authored("Remote `"),
                            UserLine::identifier(name.as_str()),
                            UserLine::authored(
                                if matches!(outcome, gat_command::RemoteOutcome::Added { .. }) {
                                    "` added: "
                                } else {
                                    "` updated: "
                                },
                            ),
                            UserLine::redacted_url(&display_url),
                        ]),
                    ),
                )?;
            }
            gat_command::RemoteOutcome::Removed { name, revealed } => {
                resource_confirmation(
                    output,
                    "Removed remote: ",
                    UserLine::identifier(name.as_str()),
                )?;
                if let Some(scope) = revealed {
                    revealed_definition(output, *scope)?;
                }
            }
        },
        Outcome::Configured(outcome) => match outcome {
            gat_command::ConfigOutcome::Value { key, value, source } => {
                render_config_values(output, *key, &[config_scalar_line(value)], *source)?;
            }
            gat_command::ConfigOutcome::Values {
                key,
                values,
                source,
            } => {
                let values: Vec<_> = values
                    .iter()
                    .map(|value| UserLine::identifier(value))
                    .collect();
                render_config_values(output, *key, &values, *source)?;
            }
            gat_command::ConfigOutcome::Set { key, value } => {
                let value = match value {
                    gat_command::ConfigScalarValue::CacheLocation(value) => {
                        value.as_path().display().to_string()
                    }
                    gat_command::ConfigScalarValue::Path(value) => value.display().to_string(),
                    gat_command::ConfigScalarValue::IngestStrategy(value) => value.to_string(),
                    gat_command::ConfigScalarValue::Boolean(value) => value.to_string(),
                    gat_command::ConfigScalarValue::ShardLevels(value) => value.get().to_string(),
                };
                render_message(
                    output,
                    &rows::Message::new(
                        rows::MessageKind::Success,
                        UserLine::compose([
                            UserLine::identifier(key.as_str()),
                            UserLine::authored(" set to "),
                            UserLine::identifier(&value),
                        ]),
                    ),
                )?;
            }
            gat_command::ConfigOutcome::SetList { key, values } => {
                output.stderr(format_args!(
                    "{}",
                    ui::success_heading(
                        &UserLine::authored("Config updated"),
                        &UserLine::authored(key.as_str())
                    )
                ))?;
                output.stderr(format_args!(""))?;
                for value in values {
                    output.stderr(format_args!("  {}", UserLine::identifier(value)))?;
                }
            }
            gat_command::ConfigOutcome::Cleared { key } => render_message(
                output,
                &rows::Message::new(
                    rows::MessageKind::Success,
                    UserLine::compose([
                        UserLine::identifier(key.as_str()),
                        UserLine::authored(" cleared (explicit empty list)"),
                    ]),
                ),
            )?,
            gat_command::ConfigOutcome::Unset { key } => render_message(
                output,
                &rows::Message::new(
                    rows::MessageKind::Success,
                    UserLine::compose([
                        UserLine::identifier(key.as_str()),
                        UserLine::authored(" unset"),
                    ]),
                ),
            )?,
        },
        Outcome::Status(outcome) => match outcome {
            gat_command::StatusOutcome::NoMatchingFiles { .. } => output.stdout(format_args!(
                "{}",
                ui::success_heading(
                    &UserLine::authored("Selected paths"),
                    &UserLine::authored("no tracked files match the current selection")
                )
            ))?,
            gat_command::StatusOutcome::NoTrackedFiles => output.stdout(format_args!(
                "{}",
                ui::success_heading(
                    &UserLine::authored("Gat lock"),
                    &UserLine::authored("no gat-tracked files")
                )
            ))?,
            gat_command::StatusOutcome::WorkingTree { rows, changes, .. } => {
                let heading = if *changes > 0 {
                    ui::action_heading(
                        &UserLine::authored("Gat lock"),
                        &count_line(*changes as i64, " change(s)"),
                    )
                } else {
                    ui::success_heading(&UserLine::authored("Gat lock"), &no_changes_summary(scope))
                };
                output.stdout(format_args!("{heading}"))?;
                if !rows.is_empty() {
                    output.stdout(format_args!(""))?;
                }
                let rendered: Vec<rows::ListRow> =
                    rows.iter().map(status_row_to_list_row).collect();
                render_rows(output, &rendered, Stream::Stdout)?;
                render_hints(
                    output,
                    selection_scope_note(scope).as_slice(),
                    Stream::Stdout,
                )?;
                output.stdout(format_args!(""))?;
                output.stdout(format_args!(
                    "{}",
                    ui::list_footer(&UserLine::compose([
                        UserLine::number(*changes as i64),
                        UserLine::authored(" change(s) across "),
                        UserLine::number(rows.len() as i64),
                        UserLine::authored(" file(s)"),
                    ]))
                ))?;
            }
        },
        Outcome::RemoteStatus(outcome) => {
            if outcome.missing.is_empty() {
                output.stdout(format_args!(
                    "{}",
                    ui::success_heading(
                        &UserLine::authored("Remote status"),
                        &UserLine::compose([
                            UserLine::authored("up to date ("),
                            UserLine::number(outcome.checked as i64),
                            UserLine::authored(" object(s) checked)"),
                        ])
                    )
                ))?;
            } else {
                output.stdout(format_args!(
                    "{}",
                    ui::action_heading(
                        &UserLine::authored("Remote status"),
                        &UserLine::compose([
                            UserLine::number(outcome.missing.len() as i64),
                            UserLine::authored(" of "),
                            UserLine::number(outcome.checked as i64),
                            UserLine::authored(" object(s) missing"),
                        ])
                    )
                ))?;
                output.stdout(format_args!(""))?;
                let rows: Vec<rows::ListRow> = outcome
                    .missing
                    .iter()
                    .map(|obj| {
                        let metadata = match &obj.route {
                            Some(route) => UserLine::compose([
                                UserLine::oid_value(&obj.object.oid),
                                UserLine::authored(" (remote `"),
                                UserLine::identifier(obj.remote_name.as_str()),
                                UserLine::authored("` via route `"),
                                UserLine::identifier(route.as_str()),
                                UserLine::authored("`)"),
                            ]),
                            None => UserLine::compose([
                                UserLine::oid_value(&obj.object.oid),
                                UserLine::authored(" (remote `"),
                                UserLine::identifier(obj.remote_name.as_str()),
                                UserLine::authored("`)"),
                            ]),
                        };
                        rows::ListRow::with_metadata(
                            rows::ListStatus::Skipped,
                            UserLine::gat_path(&obj.object.representative_path),
                            RowDetail::message(metadata),
                        )
                    })
                    .collect();
                render_rows(output, &rows, Stream::Stdout)?;
                render_hints(
                    output,
                    selection_scope_note(scope).as_slice(),
                    Stream::Stdout,
                )?;
                output.stdout(format_args!(""))?;
                output.stdout(format_args!(
                    "{}",
                    ui::list_footer(&count_line(
                        outcome.missing.len() as i64,
                        " object(s) missing"
                    ))
                ))?;
            }
            render_shallow_caution(output, outcome.shallow)?;
        }
        Outcome::Diff(outcome) => match outcome {
            DiffOutcome::NoChanges { from, to, .. } => output.stdout(format_args!(
                "{}",
                ui::success_heading(
                    &UserLine::compose([
                        UserLine::authored("Diff "),
                        UserLine::identifier(from.as_str()),
                        UserLine::authored(".."),
                        UserLine::identifier(diff_target_label(to)),
                    ]),
                    &no_changes_summary(scope)
                )
            ))?,
            DiffOutcome::Changes {
                from,
                to,
                rows,
                changes,
                ..
            } => {
                output.stdout(format_args!(
                    "{}",
                    ui::action_heading(
                        &UserLine::compose([
                            UserLine::authored("Diff "),
                            UserLine::identifier(from.as_str()),
                            UserLine::authored(".."),
                            UserLine::identifier(diff_target_label(to)),
                        ]),
                        &count_line(*changes as i64, " change(s)")
                    )
                ))?;
                output.stdout(format_args!(""))?;
                let rendered: Vec<rows::ListRow> = rows
                    .iter()
                    .map(|row| {
                        let (status, metadata) = diff_row_metadata(&row.change);
                        rows::ListRow::with_metadata(
                            status,
                            UserLine::gat_path(&row.path),
                            RowDetail::message(with_mount_metadata(
                                UserLine::authored(metadata),
                                row.mount.as_ref(),
                            )),
                        )
                    })
                    .collect();
                render_rows(output, &rendered, Stream::Stdout)?;
                render_hints(
                    output,
                    selection_scope_note(scope).as_slice(),
                    Stream::Stdout,
                )?;
                output.stdout(format_args!(""))?;
                output.stdout(format_args!(
                    "{}",
                    ui::list_footer(&UserLine::compose([
                        UserLine::number(*changes as i64),
                        UserLine::authored(" change(s) across "),
                        UserLine::number(rows.len() as i64),
                        UserLine::authored(" file(s)"),
                    ]))
                ))?;
            }
        },
        Outcome::ListedFiles(outcome) => {
            resource_heading(
                output,
                "Tracked files",
                UserLine::number(outcome.paths.len() as i64),
            )?;
            if !outcome.paths.is_empty() {
                output.stdout(format_args!(""))?;
                let rows: Vec<_> = outcome
                    .paths
                    .iter()
                    .map(|path| {
                        rows::ListRow::new(rows::ListStatus::Success, UserLine::gat_path(path))
                    })
                    .collect();
                render_rows(output, &rows, Stream::Stdout)?;
            }
            let mut hints = Vec::new();
            if outcome.paths.is_empty() && scope != gat_command::SelectionScope::Unrestricted {
                hints.push(UserLine::authored(
                    "No tracked files match the current selection.",
                ));
            }
            hints.extend(selection_scope_note(scope));
            render_hints(output, &hints, Stream::Stdout)?;
            output.stdout(format_args!(""))?;
            output.stdout(format_args!(
                "{}",
                ui::list_footer(&count_line(outcome.paths.len() as i64, " file(s)"))
            ))?;
        }
        Outcome::Pushed(outcome) => {
            if !outcome.skipped.is_empty() {
                let rows: Vec<rows::ListRow> =
                    outcome.skipped.iter().filter_map(push_skip_row).collect();
                render_rows(output, &rows, Stream::Stderr)?;
            }
            let completion =
                UserLine::authored(if scope == gat_command::SelectionScope::Unrestricted {
                    "Push complete"
                } else {
                    "Push finished for the selected paths"
                });
            let message = if outcome.total == 0 && outcome.skipped.is_empty() {
                rows::Message::authored(
                    rows::MessageKind::Success,
                    if scope == gat_command::SelectionScope::Unrestricted {
                        "0 items to push."
                    } else {
                        "0 items to push in the selected paths."
                    },
                )
            } else if outcome.skipped.is_empty() {
                rows::Message::new(
                    rows::MessageKind::Success,
                    UserLine::compose([completion, UserLine::authored(".")]),
                )
            } else {
                rows::Message::new(
                    rows::MessageKind::Caution,
                    UserLine::compose([
                        completion,
                        UserLine::authored(" ("),
                        UserLine::number(outcome.skipped.len() as i64),
                        UserLine::authored(" item(s) skipped)."),
                    ]),
                )
            };
            render_message(output, &message)?;
            let mut mounts = std::collections::BTreeMap::new();
            for skip in &outcome.skipped {
                if let gat_command::PushSkipReason::MountOwned {
                    owner_name,
                    owner_target,
                } = &skip.reason
                {
                    *mounts.entry((owner_name, owner_target)).or_insert(0usize) += 1;
                }
            }
            let mut hints: Vec<_> = mounts
                .into_iter()
                .map(|((name, target), count)| {
                    UserLine::compose([
                        count_line(count as i64, " path(s) owned by mount '"),
                        UserLine::identifier(name.as_str()),
                        UserLine::authored("' at target '"),
                        UserLine::gat_path(target),
                        UserLine::authored("'"),
                    ])
                })
                .collect();
            hints.extend(selection_scope_note(scope));
            render_hints(output, &hints, Stream::Stderr)?;
            render_shallow_caution(output, outcome.shallow)?;
        }
        Outcome::Fetched { count, shallow, .. } => {
            let summary = if *count == 0 {
                UserLine::authored("no changes")
            } else {
                count_line(*count as i64, " fetched")
            };
            ui::success(
                output,
                &UserLine::compose([
                    UserLine::authored("Fetch complete ("),
                    summary,
                    UserLine::authored(")"),
                ]),
            )?;
            render_shallow_caution(output, *shallow)?;
        }
        Outcome::Pulled(outcome) | Outcome::Synced(outcome) | Outcome::Hooked(outcome) => {
            render_sync(output, outcome)?;
        }
        Outcome::System(_) => unreachable!("handled above via early return"),
        // Deliberately silent: a clean semantic merge should produce no
        // noise during a Git merge (Git only re-invokes gat as a plain
        // CLI here through the merge-driver protocol, not interactively).
        Outcome::MergeDriverApplied => {}
        Outcome::GarbageCollected(outcome) => {
            let message = if outcome.incomplete_repositories != 0 {
                if outcome.forced_incomplete_keep_set {
                    let verb = if outcome.dry_run {
                        "Would delete"
                    } else {
                        "Deleted"
                    };
                    rows::Message::new(
                        rows::MessageKind::Caution,
                        UserLine::compose([
                            UserLine::authored(verb),
                            UserLine::authored(" "),
                            UserLine::number(outcome.deleted as i64),
                            UserLine::authored(" object(s) despite an incomplete keep set; "),
                            UserLine::number(outcome.incomplete_repositories as i64),
                            UserLine::authored(" additional repository(s) could not be inspected."),
                        ]),
                    )
                } else {
                    rows::Message::new(
                        rows::MessageKind::Caution,
                        UserLine::compose([
                            UserLine::authored(
                                "Keep set incomplete: 0 object(s) are provably collectible, and ",
                            ),
                            UserLine::number(outcome.uncertain as i64),
                            UserLine::authored(" object(s) remain uncertain because "),
                            UserLine::number(outcome.incomplete_repositories as i64),
                            UserLine::authored(" additional repository(s) could not be inspected."),
                        ]),
                    )
                }
            } else if outcome.dry_run {
                rows::Message::new(
                    rows::MessageKind::Action,
                    UserLine::compose([
                        UserLine::authored("Would delete "),
                        UserLine::number(outcome.deleted as i64),
                        UserLine::authored(" unreferenced object(s)."),
                    ]),
                )
            } else {
                rows::Message::new(
                    rows::MessageKind::Success,
                    UserLine::compose([
                        UserLine::authored("Deleted "),
                        UserLine::number(outcome.deleted as i64),
                        UserLine::authored(" unreferenced object(s)."),
                    ]),
                )
            };
            render_message(output, &message)?;
        }
        Outcome::Moved(outcome) => ui::success(
            output,
            &UserLine::compose([
                UserLine::gat_path(&outcome.src),
                UserLine::authored(" → "),
                UserLine::gat_path(&outcome.dst),
            ]),
        )?,
        Outcome::Mount(outcome) => match outcome {
            gat_command::MountOutcome::List(records) => {
                let rows: Vec<rows::ListRow> = records
                    .iter()
                    .map(|record| {
                        let mut parts = vec![
                            UserLine::identifier(record.target.as_str()),
                            UserLine::authored(" ← "),
                            UserLine::redacted_url(&crate::redaction::RedactedUrl::render(
                                record.location.as_location_str(),
                            )),
                        ];
                        if let Some(rev) = &record.revision {
                            parts.push(UserLine::authored(" @ "));
                            parts.push(UserLine::identifier(rev.as_str()));
                        }
                        rows::ListRow::with_metadata(
                            rows::ListStatus::Success,
                            UserLine::identifier(record.name.as_str()),
                            RowDetail::message(UserLine::compose(parts)),
                        )
                    })
                    .collect();
                resource_list(
                    output,
                    "Mounts",
                    records.len(),
                    "Run `gat mount add <name> <url> <target>`",
                    &rows,
                )?;
            }
            gat_command::MountOutcome::Added {
                name,
                target,
                entries,
                route,
            } => {
                render_message(
                    output,
                    &rows::Message::new(
                        rows::MessageKind::Success,
                        UserLine::compose([
                            UserLine::authored("Mount `"),
                            UserLine::identifier(name.as_str()),
                            UserLine::authored("` added, owning `"),
                            UserLine::identifier(target.as_str()),
                            UserLine::authored("` with "),
                            UserLine::number(*entries as i64),
                            UserLine::authored(" tracked file(s)."),
                        ]),
                    ),
                )?;
                if let Some(route) = route {
                    render_mount_route_bootstrap(output, route)?;
                }
            }
            gat_command::MountOutcome::Updated {
                name,
                target,
                removed,
                added,
                route,
            } => {
                render_message(
                    output,
                    &rows::Message::new(
                        rows::MessageKind::Success,
                        UserLine::compose([
                            UserLine::authored("Mount `"),
                            UserLine::identifier(name.as_str()),
                            UserLine::authored("` updated, owning `"),
                            UserLine::identifier(target.as_str()),
                            UserLine::authored("` ("),
                            UserLine::number(*removed as i64),
                            UserLine::authored(" tracked file(s) removed, "),
                            UserLine::number(*added as i64),
                            UserLine::authored(" tracked file(s) written)."),
                        ]),
                    ),
                )?;
                if let Some(route) = route {
                    render_mount_route_bootstrap(output, route)?;
                }
            }
            gat_command::MountOutcome::Show(details) => render_mount_details(output, details)?,
            gat_command::MountOutcome::Removed {
                name,
                target,
                owned_rows,
                detach_only,
                route_remaining,
            } => {
                let message = if *detach_only {
                    UserLine::compose([
                        UserLine::authored("Removed mount `"),
                        UserLine::identifier(name.as_str()),
                        UserLine::authored("`; retained "),
                        UserLine::number(*owned_rows as i64),
                        UserLine::authored(" owned tracked path(s) under `"),
                        UserLine::identifier(target.as_str()),
                        UserLine::authored("` as root-owned where not claimed by another mount."),
                    ])
                } else {
                    UserLine::compose([
                        UserLine::authored("Removed mount `"),
                        UserLine::identifier(name.as_str()),
                        UserLine::authored("` and "),
                        UserLine::number(*owned_rows as i64),
                        UserLine::authored(" owned tracked path(s) under `"),
                        UserLine::identifier(target.as_str()),
                        UserLine::authored("`."),
                    ])
                };
                render_message(
                    output,
                    &rows::Message::new(rows::MessageKind::Success, message),
                )?;
                if let Some(name) = route_remaining {
                    render_message(
                        output,
                        &rows::Message::new(
                            rows::MessageKind::Action,
                            UserLine::compose([
                                UserLine::authored("Route `"),
                                UserLine::identifier(name.as_str()),
                                UserLine::authored("` still configured at `"),
                                UserLine::identifier(target.as_str()),
                                UserLine::authored("`; run `gat route remove "),
                                UserLine::identifier(name.as_str()),
                                UserLine::authored("` to remove it separately."),
                            ]),
                        ),
                    )?;
                }
            }
        },
        Outcome::Route(outcome) => match outcome {
            gat_command::RouteOutcome::List {
                routes,
                default_remote,
            } => {
                let mut rows: Vec<rows::ListRow> = routes
                    .iter()
                    .map(|route| {
                        rows::ListRow::with_metadata(
                            rows::ListStatus::Success,
                            UserLine::identifier(route.name.as_str()),
                            RowDetail::message(UserLine::compose([
                                UserLine::gat_path(&route.path),
                                UserLine::authored(" → "),
                                UserLine::identifier(route.remote.as_str()),
                            ])),
                        )
                    })
                    .collect();
                let default_label = match default_remote {
                    gat_command::DefaultRemoteRoute::Configured { remote } => UserLine::compose([
                        UserLine::identifier(remote.as_str()),
                        UserLine::authored(" (default)"),
                    ]),
                    gat_command::DefaultRemoteRoute::Missing => {
                        UserLine::authored("(no default remote configured)")
                    }
                };
                rows.push(rows::ListRow::with_metadata(
                    if matches!(default_remote, gat_command::DefaultRemoteRoute::Missing) {
                        rows::ListStatus::Skipped
                    } else {
                        rows::ListStatus::Success
                    },
                    UserLine::authored("*"),
                    RowDetail::message(default_label),
                ));
                resource_list(
                    output,
                    "Routes",
                    routes.len(),
                    "Run `gat route add <name> <remote> <path>`",
                    &rows,
                )?;
            }
            gat_command::RouteOutcome::Added { name, path, remote } => {
                render_message(
                    output,
                    &rows::Message::new(
                        rows::MessageKind::Success,
                        UserLine::compose([
                            UserLine::authored("Route `"),
                            UserLine::identifier(name.as_str()),
                            UserLine::authored("` added: `"),
                            UserLine::gat_path(path),
                            UserLine::authored("` → `"),
                            UserLine::identifier(remote.as_str()),
                            UserLine::authored("`."),
                        ]),
                    ),
                )?;
            }
            gat_command::RouteOutcome::Updated { name, path, remote } => {
                render_message(
                    output,
                    &rows::Message::new(
                        rows::MessageKind::Success,
                        UserLine::compose([
                            UserLine::authored("Route `"),
                            UserLine::identifier(name.as_str()),
                            UserLine::authored("` updated: `"),
                            UserLine::gat_path(path),
                            UserLine::authored("` → `"),
                            UserLine::identifier(remote.as_str()),
                            UserLine::authored("`."),
                        ]),
                    ),
                )?;
            }
            gat_command::RouteOutcome::Removed { name, revealed } => {
                resource_confirmation(
                    output,
                    "Removed route: ",
                    UserLine::identifier(name.as_str()),
                )?;
                if let Some(scope) = revealed {
                    revealed_definition(output, *scope)?;
                }
            }
            gat_command::RouteOutcome::Show(details) => render_route_details(output, details)?,
        },
    }
    let scope_rendered = match &outcome {
        Outcome::Synced(_)
        | Outcome::Pushed(_)
        | Outcome::Pulled(_)
        | Outcome::ListedFiles(_)
        | Outcome::Status(gat_command::StatusOutcome::WorkingTree { .. })
        | Outcome::Diff(DiffOutcome::Changes { .. }) => true,
        Outcome::RemoteStatus(result) => !result.missing.is_empty(),
        _ => false,
    };
    if !scope_rendered && let Some(note) = selection_scope_note(scope) {
        let stream = if matches!(&outcome, Outcome::Fetched { .. }) {
            Stream::Stderr
        } else {
            Stream::Stdout
        };
        render_hints(output, &[note], stream)?;
    }
    Ok(())
}

/// Optional explanations of selection behavior, shown before the final count.
fn add_hints(outcome: &gat_command::AddOutcome) -> Vec<UserLine> {
    let mut hints = Vec::new();
    if outcome.added_count == 0 && outcome.exclusions.is_empty() {
        hints.push(UserLine::authored("No eligible files found."));
    }
    let mut ignored = false;
    for exclusion in &outcome.exclusions {
        let reason = match exclusion.reason {
            gat_engine::AddExclusionReason::GitIgnore => "ignored by Git",
            gat_engine::AddExclusionReason::GatIgnore => "ignored by .gatignore",
            gat_engine::AddExclusionReason::GitTracked => "tracked by Git",
            gat_engine::AddExclusionReason::Infrastructure => "reserved for Gat/Git infrastructure",
        };
        if exclusion.files != 0 {
            hints.push(UserLine::compose([
                UserLine::authored("Skipped "),
                count_line(exclusion.files as i64, " file(s) "),
                UserLine::authored(reason),
                UserLine::authored("."),
            ]));
        }
        if exclusion.directories != 0 {
            let git_ignored = exclusion.reason == gat_engine::AddExclusionReason::GitIgnore;
            hints.push(UserLine::compose([
                UserLine::authored(if git_ignored {
                    "Did not search "
                } else {
                    "Skipped "
                }),
                count_line(
                    exclusion.directories as i64,
                    if exclusion.directories == 1 {
                        " directory "
                    } else {
                        " directories "
                    },
                ),
                UserLine::authored(reason),
                UserLine::authored(if git_ignored {
                    " for additional files (may contain matches)."
                } else {
                    "; contents were not scanned (may contain glob matches)."
                }),
            ]));
            if git_ignored {
                hints.push(UserLine::authored(
                    "Eligible Gat-tracked files were considered separately.",
                ));
            }
        }
        ignored |= matches!(
            exclusion.reason,
            gat_engine::AddExclusionReason::GitIgnore | gat_engine::AddExclusionReason::GatIgnore
        );
    }
    if ignored {
        hints.push(UserLine::authored("Use --force to include ignored files."));
    }
    hints
}

#[cfg(test)]
mod resource_hint_tests {
    use super::*;

    #[test]
    fn config_source_is_dimmed_below_full_contrast_escaped_values() {
        let mut stdout = Vec::new();
        render_config_values(
            &mut Output::new(&mut stdout, &mut Vec::new()),
            gat_core::config_keys::ConfigKey::GitIgnorePatterns,
            &[
                UserLine::identifier("a,b"),
                UserLine::identifier("data\nfiles"),
            ],
            gat_command::ConfigSource::Scope(gat_core::config::ConfigScope::Global),
        )
        .unwrap();
        let styled = String::from_utf8(stdout).unwrap();
        let lines: Vec<_> = styled.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[2], "  a,b");
        assert!(!lines[3].contains("\x1b[2m"));
        assert_eq!(lines[4], format!("  {}", ui::dim("Source: global")));
    }

    #[test]
    fn scope_metadata_is_one_dim_line_and_omits_absent_scopes() {
        use gat_core::config::ConfigScope::{Local, Project};
        for (chosen, defined, expected) in [
            (
                Some(Local),
                Some(Project),
                "Chosen in: local  ·  Defined in: project",
            ),
            (None, Some(Project), "Defined in: project"),
            (Some(Local), None, "Chosen in: local"),
            (None, None, ""),
        ] {
            let mut stdout = Vec::new();
            resource_details(
                &mut Output::new(&mut stdout, &mut Vec::new()),
                "Default selection",
                UserLine::authored("runtime"),
                &[],
                chosen,
                defined,
            )
            .unwrap();
            let styled = String::from_utf8(stdout).unwrap();
            let lines: Vec<_> = styled.lines().collect();
            if expected.is_empty() {
                assert_eq!(lines.len(), 1);
            } else {
                assert_eq!(lines.len(), 2);
                assert_eq!(lines[1], format!("  {}", ui::dim(expected)));
            }
        }
    }

    #[test]
    fn empty_resource_hint_follows_fallback_rows_and_configured_lists_omit_it() {
        let rows = [rows::ListRow::with_metadata(
            rows::ListStatus::Skipped,
            UserLine::authored("*"),
            RowDetail::authored("no default remote"),
        )];
        for configured in [0, 1] {
            let mut stdout = Vec::new();
            resource_list(
                &mut Output::new(&mut stdout, &mut Vec::new()),
                "Routes",
                configured,
                "Run `gat route add <name> <remote> <path>`",
                &rows,
            )
            .unwrap();
            let styled = String::from_utf8(stdout).unwrap();
            if configured == 0 {
                assert!(styled.contains("\x1b[2mhint: Run"));
                assert_eq!(
                    crate::output::strip_ansi(&styled),
                    "✓ Routes: none configured\n\n.  *  no default remote\n\nhint: Run `gat route add <name> <remote> <path>`\n"
                );
            } else {
                assert!(!styled.contains("hint:"));
            }
        }
    }
}

#[cfg(test)]
mod add_output_tests {
    use super::*;

    #[test]
    fn ignored_selection_has_dim_hints_before_the_count_without_zero_or_sample_rows() {
        let outcome = gat_command::AddOutcome {
            rows: vec![gat_command::AddedRow {
                path: gat_core::path_scope::PathScope::Path(
                    gat_core::lexical_path::GatPath::parse_canonical("small-files").unwrap(),
                ),
                file_count: Some(0),
            }],
            added_count: 0,
            exclusions: vec![gat_engine::AddExclusion {
                reason: gat_engine::AddExclusionReason::GitIgnore,
                files: 1000,
                directories: 0,
                samples: vec![
                    gat_core::lexical_path::GatPath::parse_canonical("small-files/file-000000.bin")
                        .unwrap(),
                ],
            }],
        };
        let mut stdout = Vec::new();
        render(
            &mut Output::new(&mut stdout, &mut Vec::new()),
            Outcome::Added(outcome),
        )
        .unwrap();
        let styled = String::from_utf8(stdout).unwrap();
        let hints: Vec<_> = styled
            .lines()
            .filter(|line| line.contains("hint:"))
            .collect();
        assert_eq!(hints.len(), 2);
        assert!(hints.iter().all(|line| line.starts_with("\x1b[2mhint:")));
        assert_eq!(
            crate::output::strip_ansi(&styled),
            "✓ Added: 0 file(s)\n\nhint: Skipped 1000 file(s) ignored by Git.\nhint: Use --force to include ignored files.\n\n0 file(s)\n"
        );
    }

    #[test]
    fn force_hint_is_shared_across_ignore_sources() {
        let outcome = gat_command::AddOutcome {
            rows: Vec::new(),
            added_count: 0,
            exclusions: [
                gat_engine::AddExclusionReason::GitIgnore,
                gat_engine::AddExclusionReason::GatIgnore,
            ]
            .into_iter()
            .map(|reason| gat_engine::AddExclusion {
                reason,
                files: 1,
                directories: 0,
                samples: Vec::new(),
            })
            .collect(),
        };
        let hints = add_hints(&outcome);
        assert_eq!(hints.len(), 3);
        assert_eq!(
            hints
                .iter()
                .filter(|line| line.as_str().contains("--force"))
                .count(),
            1
        );
    }

    #[test]
    fn hints_summarize_exclusions_without_sample_paths() {
        let outcome = gat_command::AddOutcome {
            rows: Vec::new(),
            added_count: 0,
            exclusions: vec![gat_engine::AddExclusion {
                reason: gat_engine::AddExclusionReason::GitIgnore,
                files: 9,
                directories: 2,
                samples: vec![
                    gat_core::lexical_path::GatPath::parse_canonical("unsafe\x1b[31m.bin").unwrap(),
                ],
            }],
        };
        let lines: Vec<_> = add_hints(&outcome)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(lines.iter().any(|line| {
            line.contains("Did not search 2 directories ignored by Git for additional files")
        }));
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("samples") && !line.contains("unsafe"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("may contain matches"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "Eligible Gat-tracked files were considered separately.")
        );
        assert!(lines.iter().any(|line| line.contains("--force")));
        assert!(lines.iter().all(|line| !line.contains('\x1b')));
    }

    #[test]
    fn empty_selection_is_distinct_from_infrastructure_exclusion() {
        let mut outcome = gat_command::AddOutcome {
            rows: Vec::new(),
            added_count: 0,
            exclusions: Vec::new(),
        };
        let lines: Vec<_> = add_hints(&outcome)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(lines, ["No eligible files found."]);
        outcome.exclusions.push(gat_engine::AddExclusion {
            reason: gat_engine::AddExclusionReason::Infrastructure,
            files: 1,
            directories: 0,
            samples: Vec::new(),
        });
        let lines: Vec<_> = add_hints(&outcome)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("--force") && !line.contains("No eligible"))
        );
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;
    use gat_core::config::ConfigScope;
    use gat_core::lexical_path::{GatPath, GatSubpath};

    #[test]
    fn config_list_confirmation_preserves_individual_values() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render(
            &mut Output::new(&mut stdout, &mut stderr),
            Outcome::Configured(gat_command::ConfigOutcome::SetList {
                key: gat_core::config_keys::ConfigKey::GitIgnorePatterns,
                values: vec!["a,b".into(), "data files".into()],
            }),
        )
        .unwrap();
        assert!(stdout.is_empty());
        assert_eq!(
            crate::output::strip_ansi(&String::from_utf8(stderr).unwrap()),
            "✓ Config updated: git.ignore_patterns\n\n  a,b\n  data files\n"
        );
    }

    #[test]
    fn system_cleanup_advice_is_dim_but_required_recovery_remains_plain() {
        use gat_command::{DomainFact, LockClean, LockFact, LockRepair, SystemOutcome, SystemVerb};
        for (fact, optional) in [
            (
                LockFact::Repair(LockRepair::CleanScratch { count: 2 }),
                true,
            ),
            (
                LockFact::Repair(LockRepair::Recovered {
                    txn_id: "txn".into(),
                    choice: gat_command::RecoveryChoice::RestoreBackup,
                }),
                true,
            ),
            (
                LockFact::Clean(LockClean {
                    removed: 0,
                    unresolved: true,
                }),
                false,
            ),
        ] {
            let mut stdout = Vec::new();
            render_system(
                &mut Output::new(&mut stdout, &mut Vec::new()),
                SystemOutcome {
                    verb: if optional {
                        SystemVerb::Repair
                    } else {
                        SystemVerb::Clean
                    },
                    facts: vec![DomainFact::Lock(fact)],
                },
            )
            .unwrap();
            let styled = String::from_utf8(stdout).unwrap();
            let advice = styled
                .lines()
                .find(|line| line.contains("Run `gat system"))
                .unwrap();
            assert_eq!(advice.starts_with("\x1b[2mhint: "), optional);
            if !optional {
                assert!(!advice.contains("\x1b[2m"));
            }
        }
    }

    #[test]
    fn sync_problem_sections_have_no_leading_blank_and_separate_headings_from_rows() {
        let path = GatPath::parse_canonical("data.bin").unwrap();
        let oid = gat_core::oid::Oid::from_bytes([0xaa; 32]);
        let outcome = gat_command::SyncOutcome {
            scope: gat_command::SelectionScope::Unrestricted,
            outcome: gat_engine::SyncOutcome {
                missing: vec![(path.clone(), oid)],
                conflicts: vec![path.clone()],
                corrupted: vec![(path, oid)],
                ..Default::default()
            },
            fetched: 0,
            repaired: 0,
            repair_failures: Vec::new(),
            reshaped: None,
            shallow: false,
            completion: gat_command::SyncCompletionStatus::Incomplete {
                conflicts: 1,
                missing: 1,
                corrupted: 1,
            },
        };
        let mut stderr = Vec::new();
        render_sync(&mut Output::new(&mut Vec::new(), &mut stderr), &outcome).unwrap();
        let plain = crate::output::strip_ansi(&String::from_utf8(stderr).unwrap());
        assert!(plain.starts_with("! Missing objects: 1\n\n!  data.bin"));
        for heading in ["Conflicts", "Corrupted objects"] {
            assert!(plain.contains(&format!("\n\n! {heading}: 1\n\n!  data.bin")));
        }
        assert!(plain.ends_with("\n\n! Sync incomplete (no changes)\n"));
    }

    #[test]
    fn mount_metadata_follows_existing_status_details_and_is_dimmed() {
        let path = GatPath::parse_canonical("vendor/data.bin").unwrap();
        let oid = gat_core::oid::Oid::from_bytes([0xaa; 32]);
        for (change, cache_presence, expected) in [
            (
                gat_engine::RowChange::Added { oid },
                Some(gat_command::CachePresence::Present),
                "new, cached (mount models)",
            ),
            (
                gat_engine::RowChange::Unchanged { oid },
                Some(gat_command::CachePresence::Missing),
                "missing from cache; run `gat fetch` (mount models)",
            ),
            (gat_engine::RowChange::Removed, None, "(mount models)"),
        ] {
            let row = status_row_to_list_row(&gat_command::StatusRow {
                path: path.clone(),
                change,
                cache_presence,
                mount: Some("models".into()),
            });
            let mut stdout = Vec::new();
            render_rows(
                &mut Output::new(&mut stdout, &mut Vec::new()),
                &[row],
                Stream::Stdout,
            )
            .unwrap();
            let styled = String::from_utf8(stdout).unwrap();
            assert!(styled.ends_with(&format!("{}\n", ui::dim(expected))));
        }
        assert_eq!(
            with_mount_metadata(UserLine::authored("new"), Some(&"models".into())).as_str(),
            "new (mount models)"
        );
        assert_eq!(
            with_mount_metadata(UserLine::authored("new"), None).as_str(),
            "new"
        );
    }

    #[test]
    fn push_summarizes_mount_owned_skips_without_hiding_cache_problems() {
        use gat_command::{PushOutcome, PushSkip, PushSkipReason};
        for (include_cache_problem, total) in [(false, 0), (false, 1), (true, 0), (true, 1)] {
            let mut skipped: Vec<_> = ["vendor/a.bin", "vendor/b.bin"]
                .into_iter()
                .map(|path| PushSkip {
                    path: GatPath::parse_canonical(path).unwrap(),
                    reason: PushSkipReason::MountOwned {
                        owner_name: "vendor".into(),
                        owner_target: GatPath::parse_canonical("vendor").unwrap(),
                    },
                })
                .collect();
            skipped.push(PushSkip {
                path: GatPath::parse_canonical("data assets/image.bin").unwrap(),
                reason: PushSkipReason::MountOwned {
                    owner_name: "assets".into(),
                    owner_target: GatPath::parse_canonical("data assets").unwrap(),
                },
            });
            if include_cache_problem {
                skipped.push(PushSkip {
                    path: GatPath::parse_canonical("data.bin").unwrap(),
                    reason: PushSkipReason::CacheMissing,
                });
            }
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            render(
                &mut Output::new(&mut stdout, &mut stderr),
                Outcome::Pushed(PushOutcome {
                    scope: gat_command::SelectionScope::Unrestricted,
                    total,
                    skipped,
                    shallow: false,
                }),
            )
            .unwrap();
            assert!(stdout.is_empty());
            let styled = String::from_utf8(stderr).unwrap();
            let plain = crate::output::strip_ansi(&styled);
            assert!(!plain.contains("vendor/a.bin"));
            assert!(!plain.contains("vendor/b.bin"));
            assert_eq!(plain.matches("hint:").count(), 2);
            assert!(styled.contains(&ui::list_hint(&UserLine::authored(
                "2 path(s) owned by mount 'vendor' at target 'vendor'"
            ))));
            assert!(plain.ends_with(&format!(
                "! Push complete ({} item(s) skipped).\n\nhint: 1 path(s) owned by mount 'assets' at target 'data assets'\nhint: 2 path(s) owned by mount 'vendor' at target 'vendor'\n",
                if include_cache_problem { 4 } else { 3 }
            )));
            assert_eq!(plain.contains("data.bin"), include_cache_problem);
            assert_eq!(
                plain.contains("not in cache; run `gat add`"),
                include_cache_problem
            );
        }
    }

    #[test]
    fn mutation_reports_keep_counts_last_and_omit_empty_row_sections() {
        let path = GatPath::parse_canonical("data.bin").unwrap();
        for (outcome, expected) in [
            (
                Outcome::Added(gat_command::AddOutcome {
                    rows: vec![gat_command::AddedRow {
                        path: gat_core::path_scope::PathScope::Path(path.clone()),
                        file_count: None,
                    }],
                    added_count: 1,
                    exclusions: Vec::new(),
                }),
                "✓ Added: 1 file(s)\n\nA  data.bin\n\n1 file(s)\n",
            ),
            (
                Outcome::Removed(gat_command::RemoveOutcome { paths: vec![path] }),
                "✓ Removed: 1 file(s)\n\nD  data.bin\n\n1 file(s)\n",
            ),
            (
                Outcome::Added(gat_command::AddOutcome {
                    rows: Vec::new(),
                    added_count: 0,
                    exclusions: Vec::new(),
                }),
                "✓ Added: 0 file(s)\n\nhint: No eligible files found.\n\n0 file(s)\n",
            ),
            (
                Outcome::Removed(gat_command::RemoveOutcome { paths: Vec::new() }),
                "✓ Removed: 0 file(s)\n\n0 file(s)\n",
            ),
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            render(&mut Output::new(&mut stdout, &mut stderr), outcome).unwrap();
            assert_eq!(
                crate::output::strip_ansi(&String::from_utf8(stdout).unwrap()),
                expected
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn cache_metadata_is_dimmed_on_unchanged_files() {
        let outcome = Outcome::Status(gat_command::StatusOutcome::WorkingTree {
            scope: gat_command::SelectionScope::Unrestricted,
            rows: vec![gat_command::StatusRow {
                path: GatPath::parse_canonical("data.bin").unwrap(),
                change: gat_engine::RowChange::Unchanged {
                    oid: gat_core::oid::Oid::from_bytes([0xaa; 32]),
                },
                cache_presence: Some(gat_command::CachePresence::Missing),
                mount: None,
            }],
            changes: 0,
        });
        let mut stdout = Vec::new();
        render(&mut Output::new(&mut stdout, &mut Vec::new()), outcome).unwrap();
        let styled = String::from_utf8(stdout).unwrap();
        let row = styled
            .lines()
            .find(|line| line.contains("data.bin"))
            .unwrap();
        assert!(row.contains("\x1b[2mmissing from cache; run `gat fetch`"));
        assert_eq!(
            crate::output::strip_ansi(row),
            "✓  data.bin  missing from cache; run `gat fetch`"
        );
    }

    #[test]
    fn scope_hints_are_separated_from_results_and_precede_count_footers() {
        use gat_command::SelectionScope;
        for scope in [
            SelectionScope::Unrestricted,
            SelectionScope::Explicit,
            SelectionScope::Configured,
        ] {
            for (outcome, stream) in [
                (
                    Outcome::Status(gat_command::StatusOutcome::WorkingTree {
                        scope,
                        rows: Vec::new(),
                        changes: 0,
                    }),
                    Stream::Stdout,
                ),
                (
                    Outcome::Diff(DiffOutcome::NoChanges {
                        scope,
                        from: gat_core::git::GitRevisionSpec::from_string("HEAD".into()),
                        to: DiffTarget::WorkingTree,
                    }),
                    Stream::Stdout,
                ),
                (
                    Outcome::Diff(DiffOutcome::Changes {
                        scope,
                        from: gat_core::git::GitRevisionSpec::from_string("HEAD".into()),
                        to: DiffTarget::WorkingTree,
                        rows: vec![gat_command::DiffRow {
                            path: GatPath::parse_canonical("data.bin").unwrap(),
                            change: gat_engine::RowChange::Removed,
                            mount: None,
                        }],
                        changes: 1,
                    }),
                    Stream::Stdout,
                ),
                (
                    Outcome::ListedFiles(gat_command::LsFilesOutcome {
                        scope,
                        paths: vec![GatPath::parse_canonical("data.bin").unwrap()],
                    }),
                    Stream::Stdout,
                ),
                (
                    Outcome::RemoteStatus(gat_command::RemoteStatusOutcome {
                        scope,
                        checked: 1,
                        missing: Vec::new(),
                        shallow: false,
                    }),
                    Stream::Stdout,
                ),
                (
                    Outcome::Pushed(gat_command::PushOutcome {
                        scope,
                        total: 1,
                        skipped: Vec::new(),
                        shallow: false,
                    }),
                    Stream::Stderr,
                ),
                (
                    Outcome::Fetched {
                        scope,
                        count: 1,
                        shallow: false,
                    },
                    Stream::Stderr,
                ),
            ] {
                let has_footer = matches!(
                    &outcome,
                    Outcome::Status(gat_command::StatusOutcome::WorkingTree { .. })
                        | Outcome::ListedFiles(_)
                        | Outcome::Diff(DiffOutcome::Changes { .. })
                );
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                render(&mut Output::new(&mut stdout, &mut stderr), outcome).unwrap();
                let (result, other) = match stream {
                    Stream::Stdout => (stdout, stderr),
                    Stream::Stderr => (stderr, stdout),
                };
                assert!(other.is_empty());
                let styled = String::from_utf8(result).unwrap();
                if let Some(note) = selection_scope_note(scope) {
                    assert_eq!(styled.matches("hint:").count(), 1);
                    let lines: Vec<_> = styled.lines().collect();
                    let index = lines
                        .iter()
                        .position(|line| line.contains("hint:"))
                        .unwrap();
                    let hint = ui::list_hint(&note);
                    let hint_lines: Vec<_> = hint.lines().collect();
                    let end = index + hint_lines.len();
                    assert_eq!(&lines[index..end], hint_lines.as_slice());
                    if has_footer {
                        assert_eq!(lines[index - 1], "");
                        assert_eq!(lines[end], "");
                        assert_eq!(end + 2, lines.len());
                    } else {
                        assert_eq!(index, 2);
                        assert_eq!(lines[1], "");
                        assert_eq!(lines.len(), end);
                    }
                } else {
                    assert!(!styled.contains("hint:"));
                }
            }
        }
    }

    #[test]
    fn sync_scope_is_a_single_dim_hint_after_the_result() {
        for scope in [
            gat_command::SelectionScope::Explicit,
            gat_command::SelectionScope::Configured,
        ] {
            for pull in [false, true] {
                let result = gat_command::SyncOutcome {
                    scope,
                    outcome: gat_engine::SyncOutcome::default(),
                    fetched: 0,
                    repaired: 0,
                    repair_failures: Vec::new(),
                    reshaped: None,
                    shallow: false,
                    completion: gat_command::SyncCompletionStatus::Clean,
                };
                let outcome = if pull {
                    Outcome::Pulled(result)
                } else {
                    Outcome::Synced(result)
                };
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                render(&mut Output::new(&mut stdout, &mut stderr), outcome).unwrap();
                assert!(stdout.is_empty());
                let styled = String::from_utf8(stderr).unwrap();
                let lines: Vec<_> = styled.lines().collect();
                assert_eq!(lines[1], "");
                assert_eq!(
                    crate::output::strip_ansi(lines[0]),
                    "✓ Sync complete (no changes)"
                );
                assert_eq!(
                    lines[2..].join("\n"),
                    ui::list_hint(&selection_scope_note(scope).unwrap())
                );
                assert!(lines[2].starts_with("\x1b[2mhint: "));
            }
        }
    }

    #[test]
    fn sync_summaries_distinguish_completion_preview_and_incomplete_outcomes() {
        use gat_command::SyncCompletionStatus;
        for (dry_run, completion, expected) in [
            (
                false,
                SyncCompletionStatus::Clean,
                "✓ Sync complete (no changes)\n",
            ),
            (
                true,
                SyncCompletionStatus::Clean,
                "→ Sync preview (no changes)\n",
            ),
            (
                false,
                SyncCompletionStatus::Incomplete {
                    conflicts: 1,
                    missing: 0,
                    corrupted: 0,
                },
                "! Sync incomplete (no changes)\n",
            ),
        ] {
            let outcome = gat_command::SyncOutcome {
                scope: gat_command::SelectionScope::Unrestricted,
                outcome: gat_engine::SyncOutcome {
                    dry_run,
                    ..Default::default()
                },
                fetched: 0,
                repaired: 0,
                repair_failures: Vec::new(),
                reshaped: None,
                shallow: false,
                completion,
            };
            let mut stderr = Vec::new();
            render_sync(&mut Output::new(&mut Vec::new(), &mut stderr), &outcome).unwrap();
            let styled = String::from_utf8(stderr).unwrap();
            assert!(!styled.contains("\x1b[2m"));
            assert_eq!(crate::output::strip_ansi(&styled), expected);
        }
    }

    #[test]
    fn mount_and_route_details_share_fields_and_preserve_individual_patterns() {
        let mount = gat_command::MountDetails {
            name: "assets".into(),
            location: gat_core::git_location::GitLocationSpec::from_string("source repo".into()),
            path: GatSubpath::Root,
            target: GatPath::parse_canonical("data files").unwrap(),
            revision: None,
            revision_lock: None,
            include: vec![
                gat_core::globs::GatGlobPattern::parse("**/*, final.bin").unwrap(),
                gat_core::globs::GatGlobPattern::parse("**/*.onnx").unwrap(),
            ],
            exclude: Vec::new(),
            tracked_rows: 0,
            route: None,
            default_remote: None,
            scope: ConfigScope::Project,
        };
        let route = gat_command::RouteDetails {
            name: "assets".into(),
            path: mount.target.clone(),
            remote: "origin".into(),
            scope: ConfigScope::Project,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            let mut output = Output::new(&mut stdout, &mut stderr);
            render_mount_details(&mut output, &mount).unwrap();
            render_route_details(&mut output, &route).unwrap();
        }
        let styled = String::from_utf8(stdout).unwrap();
        assert_eq!(
            styled
                .lines()
                .filter(|line| line.contains("\x1b[2m"))
                .count(),
            2
        );
        assert!(stderr.is_empty());
        let plain = crate::output::strip_ansi(&styled);
        assert!(plain.starts_with("✓ Mount: assets\n  Defined in: project\n\n  URL:"));
        assert!(plain.contains("✓ Route: assets\n  Defined in: project\n\n  Path:   data files\n"));
        let includes: Vec<_> = plain
            .lines()
            .filter(|line| line.trim_start().starts_with("Include:"))
            .collect();
        assert_eq!(includes.len(), 2);
        assert!(includes[0].ends_with("**/*, final.bin"));
        assert!(includes[1].ends_with("**/*.onnx"));
    }
}
