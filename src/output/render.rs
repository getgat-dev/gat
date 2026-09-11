//! Durable rendering of completed command outcomes to borrowed writers.
//! This module authors command wording, report structure, and stream ordering.
//! Shared list mechanics live in `list`; styling lives in `terminal`. The process
//! adapter owns destinations, color policy, and write-failure exit codes.

use super::layout::DetailMode;
use super::list::{
    begin_row_section, render_projected_rows, render_rows, render_selected_items,
    render_selected_rows,
};
use crate::app::Outcome;
use crate::output::rows;
use crate::output::rows::RowDetail;
use crate::output::terminal as ui;
use crate::output::{Output, Stream, WriteFailure};
use crate::presentation::UserLine;
use gat_command::{DiffOutcome, DiffTarget};

/// Final interruption notice, emitted only after owned command work has drained.
pub fn interrupted(output: &mut Output<'_>) -> Result<(), WriteFailure> {
    ui::caution(output, &UserLine::authored("Interrupted"))
}

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
    label: &'static str,
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
            UserLine::authored(label),
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
        gat_command::ConfigScalarValue::Unsigned(value) => UserLine::number(i64::from(*value)),
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
    key: gat_core::config_keys::SettingKey,
    values: &[UserLine],
    source: gat_command::ConfigSource,
) -> Result<(), WriteFailure> {
    resource_heading(output, "Config", UserLine::authored(key.as_str()))?;
    ui::section(output, Stream::Stdout)?;
    if values.is_empty() {
        ui::paragraph(
            output,
            Stream::Stdout,
            &UserLine::authored("(empty)"),
            2,
            ui::Emphasis::Normal,
        )?;
    } else {
        for value in values {
            ui::paragraph(output, Stream::Stdout, value, 2, ui::Emphasis::Normal)?;
        }
    }
    let source = match source {
        gat_command::ConfigSource::Default => UserLine::authored("built-in default"),
        gat_command::ConfigSource::Scope(scope) => scope_line(scope),
        gat_command::ConfigSource::Environment(key) => UserLine::compose([
            UserLine::identifier(&key.environment_name()),
            UserLine::authored(" environment variable"),
        ]),
    };
    let metadata = UserLine::compose([UserLine::authored("Source: "), source]);
    ui::paragraph(output, Stream::Stdout, &metadata, 2, ui::Emphasis::Muted)
}

fn resource_heading(
    output: &mut Output<'_>,
    label: &'static str,
    value: UserLine,
) -> Result<(), WriteFailure> {
    ui::success_heading(output, Stream::Stdout, &UserLine::authored(label), &value)
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
                scopes.push(UserLine::authored(" · "));
            }
            scopes.extend([UserLine::authored(label), scope_line(scope)]);
        }
    }
    if !scopes.is_empty() {
        ui::paragraph(
            output,
            Stream::Stdout,
            &UserLine::compose(scopes),
            2,
            ui::Emphasis::Muted,
        )?;
    }
    if !fields.is_empty() {
        ui::section(output, Stream::Stdout)?;
        ui::fields(output, Stream::Stdout, fields)?;
    }
    Ok(())
}

fn resource_list(
    output: &mut Output<'_>,
    label: &'static str,
    configured: usize,
    empty_command: &'static str,
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
        ui::section(output, Stream::Stdout)?;
        render_rows(output, rows, Stream::Stdout)?;
    }
    if configured == 0 {
        ui::section(output, Stream::Stdout)?;
        ui::list_hint(
            output,
            Stream::Stdout,
            &UserLine::compose([
                UserLine::authored("Run `"),
                UserLine::authored(empty_command).unbroken(),
                UserLine::authored("`"),
            ]),
        )?;
    }
    Ok(())
}

fn resource_label(name: &str, is_default: bool) -> UserLine {
    UserLine::compose([
        UserLine::identifier(name),
        UserLine::authored(if is_default { " (default)" } else { "" }),
    ])
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
    if record.definition.is_unrestricted() {
        fields.push((
            UserLine::authored("Matches"),
            UserLine::authored("All tracked paths"),
        ));
    }
    for (label, patterns) in [
        ("Include", &record.definition.include),
        ("Exclude", &record.definition.exclude),
    ] {
        for pattern in patterns {
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
                        resource_label(record.name.as_str(), record.is_default),
                        RowDetail::message(if record.definition.is_unrestricted() {
                            UserLine::authored("All tracked paths")
                        } else {
                            UserLine::gat_subpath(&record.definition.path)
                        }),
                    )
                })
                .collect();
            resource_list(
                output,
                "Selections",
                records.len(),
                "gat selection add <name> --path <path>",
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
        O::Saved { name, unrestricted } => resource_confirmation(
            output,
            "Saved selection: ",
            UserLine::compose([
                UserLine::identifier(name.as_str()),
                UserLine::authored(if *unrestricted {
                    " (all tracked paths)"
                } else {
                    ""
                }),
            ]),
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
        O::Default(default) => {
            let record = default.as_ref().map(|default| &default.record);
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
                default.as_ref().map(|default| default.chosen_in),
                record.map(|record| record.scope),
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
/// "new, cached"/"uncached" wording in root
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
        gat_command::CachePresence::Missing => "uncached",
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
    ui::hints(output, stream, hints)
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
    ui::status_heading(
        output,
        Stream::Stdout,
        status_of(outcome.status),
        &outcome.title,
        &outcome.summary,
    )?;
    let layout = output.layout(Stream::Stdout);
    let mut budget = layout.budget(outcome.groups.iter().map(|group| group.rows.len()).sum());
    for group in &outcome.groups {
        ui::section(output, Stream::Stdout)?;
        ui::status_group(
            output,
            Stream::Stdout,
            status_of(group.status()),
            &group.title,
        )?;
        let items = group.rows.iter().map(|row| {
            ui::ListItem::with_metadata(
                status_of(row.status),
                &row.label,
                row.metadata(layout.detail_mode()),
            )
        });
        let selection = budget.take(group.rows.len());
        begin_row_section(output, Stream::Stdout, &selection)?;
        render_selected_items(output, items, Stream::Stdout, selection)?;
        let mut explanations = group.explanations().peekable();
        if explanations.peek().is_some() {
            ui::section(output, Stream::Stdout)?;
        }
        for explanation in explanations {
            ui::paragraph(
                output,
                Stream::Stdout,
                &explanation,
                2,
                ui::Emphasis::Normal,
            )?;
        }
        render_hints(output, &group.hints, Stream::Stdout)?;
    }
    if !outcome.footer.is_empty() {
        ui::section(output, Stream::Stdout)?;
        for line in &outcome.footer {
            ui::paragraph(output, Stream::Stdout, line, 0, ui::Emphasis::Normal)?;
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
        )
        .with_annotation("uncached"),
        gat_command::PushSkipReason::CacheCorrupt => {
            RowDetail::authored("cached object corrupt; re-add or re-fetch it before pushing")
                .with_annotation("corrupt")
        }
    };
    Some(rows::ListRow::with_metadata(
        rows::ListStatus::Skipped,
        UserLine::gat_path(&skip.path),
        detail,
    ))
}

fn missing_rows(
    outcome: &gat_engine::SyncOutcome,
) -> impl ExactSizeIterator<Item = rows::ListRow> + '_ {
    outcome.missing.iter().map(|(path, oid)| {
        rows::ListRow::with_metadata(
            rows::ListStatus::Conflict,
            UserLine::gat_path(path),
            RowDetail::message(UserLine::compose([
                UserLine::authored("object "),
                UserLine::oid_value(oid),
                UserLine::authored(" missing from cache; run "),
                UserLine::authored("`gat fetch`").unbroken(),
                UserLine::authored(" or "),
                UserLine::authored("`gat pull`").unbroken(),
                UserLine::authored(", or configure a remote"),
            ]))
            .with_annotation("uncached"),
        )
    })
}

fn conflict_rows(
    outcome: &gat_engine::SyncOutcome,
) -> impl ExactSizeIterator<Item = rows::ListRow> + '_ {
    outcome.conflicts.iter().map(|path| {
        rows::ListRow::with_metadata(
            rows::ListStatus::Conflict,
            UserLine::gat_path(path),
            RowDetail::authored(
                "locally modified; left untouched (use `gat sync --force` to overwrite)",
            )
            .with_annotation("conflict"),
        )
    })
}

fn corrupted_rows(
    outcome: &gat_engine::SyncOutcome,
) -> impl ExactSizeIterator<Item = rows::ListRow> + '_ {
    outcome.corrupted.iter().map(|(path, oid)| {
        rows::ListRow::with_metadata(
            rows::ListStatus::Conflict,
            UserLine::gat_path(path),
            RowDetail::message(UserLine::compose([
                UserLine::authored("cache object "),
                UserLine::oid_value(oid),
                UserLine::authored(" is corrupted; run "),
                UserLine::authored("`gat sync --repair`").unbroken(),
                UserLine::authored(" to re-fetch it from a remote"),
            ]))
            .with_annotation("corrupt"),
        )
    })
}

fn repair_failure_rows(
    repair_failures: &[gat_command::RepairFailure],
) -> impl ExactSizeIterator<Item = rows::ListRow> + '_ {
    repair_failures.iter().map(|failure| {
        let problem = crate::error::map::problem::repair_problem(failure.error.clone());
        let prefix = UserLine::compose([
            UserLine::authored("repair of object "),
            UserLine::oid_value(&failure.oid),
            UserLine::authored(" failed: "),
        ]);
        rows::ListRow::with_metadata(
            rows::ListStatus::Conflict,
            UserLine::gat_path(&failure.path),
            RowDetail::composed(prefix, problem, "").with_annotation("repair failed"),
        )
    })
}

/// Retain headings for omitted groups, but only separate a row block when it
/// contains visible rows or the report's omission marker.
fn render_sync_group(
    output: &mut Output<'_>,
    label: &'static str,
    rows: impl ExactSizeIterator<Item = rows::ListRow>,
    budget: &mut super::layout::RowBudget,
    preceding_output: &mut bool,
) -> Result<(), WriteFailure> {
    let total = rows.len();
    if total == 0 {
        return Ok(());
    }
    if *preceding_output {
        ui::section(output, Stream::Stderr)?;
    }
    ui::caution_heading(
        output,
        Stream::Stderr,
        &UserLine::authored(label),
        &UserLine::number(total as i64),
    )?;
    let selection = budget.take(total);
    begin_row_section(output, Stream::Stderr, &selection)?;
    render_selected_rows(output, rows, Stream::Stderr, selection)?;
    *preceding_output = true;
    Ok(())
}

fn render_sync(
    output: &mut Output<'_>,
    outcome: &gat_command::SyncOutcome,
    command: &'static str,
) -> Result<(), WriteFailure> {
    let sync = &outcome.outcome;
    let repair_failures = repair_failure_rows(&outcome.repair_failures);
    let mut budget = output.layout(Stream::Stderr).budget(
        repair_failures.len() + sync.missing.len() + sync.conflicts.len() + sync.corrupted.len(),
    );
    let repair_count = repair_failures.len();
    render_selected_rows(
        output,
        repair_failures,
        Stream::Stderr,
        budget.take(repair_count),
    )?;

    if let Some(levels) = outcome.reshaped {
        if repair_count > 0 {
            ui::section(output, Stream::Stderr)?;
        }
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

    let mut preceding_output = repair_count > 0 || outcome.reshaped.is_some();
    render_sync_group(
        output,
        "Missing objects",
        missing_rows(sync),
        &mut budget,
        &mut preceding_output,
    )?;
    render_sync_group(
        output,
        "Conflicts",
        conflict_rows(sync),
        &mut budget,
        &mut preceding_output,
    )?;
    render_sync_group(
        output,
        "Corrupted objects",
        corrupted_rows(sync),
        &mut budget,
        &mut preceding_output,
    )?;
    let mut hints = Vec::new();
    if !sync.missing.is_empty() {
        hints.push(UserLine::compose([
            UserLine::authored("Run "),
            UserLine::authored("`gat fetch`").unbroken(),
            UserLine::authored(" or "),
            UserLine::authored("`gat pull`").unbroken(),
            UserLine::authored(" to retrieve missing objects, or configure a remote."),
        ]));
    }
    if !sync.conflicts.is_empty() {
        hints.push(UserLine::compose([
            UserLine::authored("Locally modified files were left untouched. Use "),
            UserLine::authored("`gat sync --force`").unbroken(),
            UserLine::authored(" to overwrite them."),
        ]));
    }
    if !sync.corrupted.is_empty() {
        hints.push(UserLine::compose([
            UserLine::authored("Run "),
            UserLine::authored("`gat sync --repair`").unbroken(),
            UserLine::authored(" to re-fetch corrupted objects from a remote."),
        ]));
    }
    if repair_count > 0 && output.layout(Stream::Stderr).detail_mode() == DetailMode::Compact {
        hints.push(UserLine::authored(
            "Use --full-output to inspect individual repair failures.",
        ));
    }
    render_hints(output, &hints, Stream::Stderr)?;
    if preceding_output {
        ui::section(output, Stream::Stderr)?;
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
        (rows::MessageKind::Caution, " incomplete (")
    } else if sync.dry_run {
        (rows::MessageKind::Action, " preview (")
    } else {
        (rows::MessageKind::Success, " complete (")
    };
    render_message(
        output,
        &rows::Message::new(
            kind,
            UserLine::compose([
                UserLine::authored(command),
                UserLine::authored(label),
                summary,
                UserLine::authored(")"),
            ]),
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
        gat_command::SelectionScope::Configured => Some(UserLine::compose([
            UserLine::authored(
                "Configured path selection applied; other paths were not checked. Use ",
            ),
            UserLine::authored("--path .").unbroken(),
            UserLine::authored(" to select the whole repository."),
        ])),
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
                    let installed = UserLine::join(
                        installed
                            .iter()
                            .map(|hook| UserLine::identifier(hook.as_str())),
                        ", ",
                    );
                    render_message(
                        output,
                        &rows::Message::new(
                            rows::MessageKind::Success,
                            UserLine::compose([
                                UserLine::authored("Installed Git hooks: "),
                                installed,
                                UserLine::authored(". "),
                                UserLine::authored("`gat sync`").unbroken(),
                                UserLine::authored(
                                    " now runs automatically after checkout, merge/pull, and rebase/amend.",
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
                    let removed = UserLine::join(
                        removed
                            .iter()
                            .map(|hook| UserLine::identifier(hook.as_str())),
                        ", ",
                    );
                    render_message(
                        output,
                        &rows::Message::new(
                            rows::MessageKind::Success,
                            UserLine::compose([
                                UserLine::authored("Removed Git hooks: "),
                                removed,
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
                &rows::Message::new(rows::MessageKind::Action, initialization_guidance()),
            )?;
        }
        Outcome::Added(outcome) => {
            ui::success_heading(
                output,
                Stream::Stdout,
                &UserLine::authored("Added"),
                &count_line(outcome.added_count as i64, " file(s)"),
            )?;
            let mut eligible = outcome
                .rows
                .iter()
                .filter(|row| row.file_count != Some(0))
                .peekable();
            if eligible.peek().is_some() {
                ui::section(output, Stream::Stdout)?;
            }
            render_projected_rows(output, eligible, Stream::Stdout, |row| {
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
            })?;
            let hints = add_hints(outcome);
            render_hints(output, &hints, Stream::Stdout)?;
            ui::section(output, Stream::Stdout)?;
            ui::list_footer(
                output,
                Stream::Stdout,
                &count_line(outcome.added_count as i64, " file(s)"),
            )?;
        }
        Outcome::Removed(outcome) => {
            ui::success_heading(
                output,
                Stream::Stdout,
                &UserLine::authored("Removed"),
                &count_line(outcome.paths.len() as i64, " file(s)"),
            )?;
            if !outcome.paths.is_empty() {
                ui::section(output, Stream::Stdout)?;
                render_projected_rows(output, outcome.paths.iter(), Stream::Stdout, |path| {
                    rows::ListRow::new(rows::ListStatus::Deleted, UserLine::gat_path(path))
                })?;
            }
            ui::section(output, Stream::Stdout)?;
            ui::list_footer(
                output,
                Stream::Stdout,
                &count_line(outcome.paths.len() as i64, " file(s)"),
            )?;
        }
        Outcome::Selection(outcome) => render_saved_selection(output, outcome)?,
        Outcome::Remote(outcome) => match outcome {
            gat_command::RemoteOutcome::Default(default) => {
                resource_details(
                    output,
                    "Default remote",
                    default.as_ref().map_or_else(
                        || UserLine::authored("none"),
                        |default| UserLine::identifier(default.name.as_str()),
                    ),
                    &[],
                    default.as_ref().map(|default| default.chosen_in),
                    default.as_ref().and_then(|default| default.defined_in),
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
                        rows::ListRow::with_metadata(
                            rows::ListStatus::Success,
                            resource_label(record.name.as_str(), record.is_default),
                            RowDetail::message(UserLine::redacted_url(&display_url)),
                        )
                    })
                    .collect();
                resource_list(
                    output,
                    "Remotes",
                    records.len(),
                    "gat remote add <name> <url>",
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
            gat_command::ConfigOutcome::Masked { change, source } => {
                render(output, Outcome::Configured((**change).clone()))?;
                let source = match source {
                    gat_command::ConfigSource::Environment(key) => {
                        UserLine::identifier(&key.environment_name())
                    }
                    gat_command::ConfigSource::Scope(scope) => scope_line(*scope),
                    gat_command::ConfigSource::Default => UserLine::authored("built-in default"),
                };
                ui::paragraph(
                    output,
                    Stream::Stderr,
                    &UserLine::compose([
                        UserLine::authored("Saved configuration is currently overridden by "),
                        source,
                    ]),
                    0,
                    ui::Emphasis::Normal,
                )?;
            }

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
                    gat_command::ConfigScalarValue::Unsigned(value) => value.to_string(),
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
                ui::success_heading(
                    output,
                    Stream::Stderr,
                    &UserLine::authored("Config updated"),
                    &UserLine::authored(key.as_str()),
                )?;
                ui::section(output, Stream::Stderr)?;
                for value in values {
                    ui::paragraph(
                        output,
                        Stream::Stderr,
                        &UserLine::identifier(value),
                        2,
                        ui::Emphasis::Normal,
                    )?;
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
            gat_command::StatusOutcome::NoMatchingFiles { .. } => ui::success_heading(
                output,
                Stream::Stdout,
                &UserLine::authored("Selected paths"),
                &UserLine::authored("no tracked files match the current selection"),
            )?,
            gat_command::StatusOutcome::NoTrackedFiles => ui::success_heading(
                output,
                Stream::Stdout,
                &UserLine::authored("Gat lock"),
                &UserLine::authored("no gat-tracked files"),
            )?,
            gat_command::StatusOutcome::WorkingTree { rows, changes, .. } => {
                if *changes > 0 {
                    ui::action_heading(
                        output,
                        Stream::Stdout,
                        &UserLine::authored("Gat lock"),
                        &count_line(*changes as i64, " change(s)"),
                    )
                } else {
                    ui::success_heading(
                        output,
                        Stream::Stdout,
                        &UserLine::authored("Gat lock"),
                        &no_changes_summary(scope),
                    )
                }?;
                if !rows.is_empty() {
                    ui::section(output, Stream::Stdout)?;
                }
                render_projected_rows(output, rows.iter(), Stream::Stdout, status_row_to_list_row)?;
                let mut hints: Vec<_> = selection_scope_note(scope).into_iter().collect();
                if rows
                    .iter()
                    .any(|row| row.cache_presence == Some(gat_command::CachePresence::Missing))
                {
                    hints.push(UserLine::compose([
                        UserLine::authored("Run "),
                        UserLine::authored("`gat fetch`").unbroken(),
                        UserLine::authored(" to retrieve missing cache objects."),
                    ]));
                }
                render_hints(output, &hints, Stream::Stdout)?;
                ui::section(output, Stream::Stdout)?;
                ui::list_footer(
                    output,
                    Stream::Stdout,
                    &UserLine::compose([
                        UserLine::number(*changes as i64),
                        UserLine::authored(" change(s) across "),
                        UserLine::number(rows.len() as i64),
                        UserLine::authored(" file(s)"),
                    ]),
                )?;
            }
        },
        Outcome::RemoteStatus(outcome) => {
            if outcome.missing.is_empty() {
                ui::success_heading(
                    output,
                    Stream::Stdout,
                    &UserLine::authored("Remote status"),
                    &UserLine::compose([
                        UserLine::authored("up to date ("),
                        UserLine::number(outcome.checked as i64),
                        UserLine::authored(" object(s) checked)"),
                    ]),
                )?;
            } else {
                ui::action_heading(
                    output,
                    Stream::Stdout,
                    &UserLine::authored("Remote status"),
                    &UserLine::compose([
                        UserLine::number(outcome.missing.len() as i64),
                        UserLine::authored(" of "),
                        UserLine::number(outcome.checked as i64),
                        UserLine::authored(" object(s) missing"),
                    ]),
                )?;
                ui::section(output, Stream::Stdout)?;
                render_projected_rows(output, outcome.missing.iter(), Stream::Stdout, |obj| {
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
                })?;
                render_hints(
                    output,
                    selection_scope_note(scope).as_slice(),
                    Stream::Stdout,
                )?;
                ui::section(output, Stream::Stdout)?;
                ui::list_footer(
                    output,
                    Stream::Stdout,
                    &count_line(outcome.missing.len() as i64, " object(s) missing"),
                )?;
            }
            render_shallow_caution(output, outcome.shallow)?;
        }
        Outcome::Diff(outcome) => match outcome {
            DiffOutcome::NoChanges { from, to, .. } => ui::success_heading(
                output,
                Stream::Stdout,
                &UserLine::compose([
                    UserLine::authored("Diff "),
                    UserLine::identifier(from.as_str()),
                    UserLine::authored(".."),
                    UserLine::identifier(diff_target_label(to)),
                ]),
                &no_changes_summary(scope),
            )?,
            DiffOutcome::Changes {
                from,
                to,
                rows,
                changes,
                ..
            } => {
                ui::action_heading(
                    output,
                    Stream::Stdout,
                    &UserLine::compose([
                        UserLine::authored("Diff "),
                        UserLine::identifier(from.as_str()),
                        UserLine::authored(".."),
                        UserLine::identifier(diff_target_label(to)),
                    ]),
                    &count_line(*changes as i64, " change(s)"),
                )?;
                ui::section(output, Stream::Stdout)?;
                render_projected_rows(output, rows.iter(), Stream::Stdout, |row| {
                    let (status, metadata) = diff_row_metadata(&row.change);
                    rows::ListRow::with_metadata(
                        status,
                        UserLine::gat_path(&row.path),
                        RowDetail::message(with_mount_metadata(
                            UserLine::authored(metadata),
                            row.mount.as_ref(),
                        )),
                    )
                })?;
                render_hints(
                    output,
                    selection_scope_note(scope).as_slice(),
                    Stream::Stdout,
                )?;
                ui::section(output, Stream::Stdout)?;
                ui::list_footer(
                    output,
                    Stream::Stdout,
                    &UserLine::compose([
                        UserLine::number(*changes as i64),
                        UserLine::authored(" change(s) across "),
                        UserLine::number(rows.len() as i64),
                        UserLine::authored(" file(s)"),
                    ]),
                )?;
            }
        },
        Outcome::ListedFiles(outcome) => {
            resource_heading(
                output,
                "Tracked files",
                UserLine::number(outcome.paths.len() as i64),
            )?;
            if !outcome.paths.is_empty() {
                ui::section(output, Stream::Stdout)?;
                render_projected_rows(output, outcome.paths.iter(), Stream::Stdout, |path| {
                    rows::ListRow::new(rows::ListStatus::Success, UserLine::gat_path(path))
                })?;
            }
            let mut hints = Vec::new();
            if outcome.paths.is_empty() && scope != gat_command::SelectionScope::Unrestricted {
                hints.push(UserLine::authored(
                    "No tracked files match the current selection.",
                ));
            }
            hints.extend(selection_scope_note(scope));
            render_hints(output, &hints, Stream::Stdout)?;
            ui::section(output, Stream::Stdout)?;
            ui::list_footer(
                output,
                Stream::Stdout,
                &count_line(outcome.paths.len() as i64, " file(s)"),
            )?;
        }
        Outcome::Pushed(outcome) => {
            if !outcome.skipped.is_empty() {
                let rows: Vec<rows::ListRow> =
                    outcome.skipped.iter().filter_map(push_skip_row).collect();
                render_rows(output, &rows, Stream::Stderr)?;
                if !rows.is_empty() {
                    ui::section(output, Stream::Stderr)?;
                }
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
            if outcome
                .skipped
                .iter()
                .any(|skip| matches!(skip.reason, gat_command::PushSkipReason::CacheMissing))
            {
                hints.push(UserLine::compose([
                    UserLine::authored("Objects not in cache; run "),
                    UserLine::authored("`gat add`").unbroken(),
                    UserLine::authored(" or "),
                    UserLine::authored("`gat fetch`").unbroken(),
                    UserLine::authored(
                        " (fetch the selected history first for a historical object).",
                    ),
                ]));
            }
            if outcome
                .skipped
                .iter()
                .any(|skip| matches!(skip.reason, gat_command::PushSkipReason::CacheCorrupt))
            {
                hints.push(UserLine::authored(
                    "Cached objects are corrupt; re-add or re-fetch them before pushing.",
                ));
            }
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
        Outcome::Pulled(outcome) => {
            render_sync(output, outcome, "Pull")?;
        }
        Outcome::Synced(outcome) | Outcome::Hooked(outcome) => {
            render_sync(output, outcome, "Sync")?;
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
                    "gat mount add <name> <url> <target>",
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
                                UserLine::authored("`; run `"),
                                UserLine::compose([
                                    UserLine::authored("gat route remove "),
                                    UserLine::identifier(name.as_str()),
                                ])
                                .unbroken(),
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
                    "gat route add <name> <remote> <path>",
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

fn initialization_guidance() -> UserLine {
    UserLine::compose([
        UserLine::authored("Next: "),
        UserLine::authored("`gat add <path>`").unbroken(),
        UserLine::authored(" then "),
        UserLine::authored("`gat remote add origin s3://bucket/prefix`").unbroken(),
        UserLine::authored("."),
    ])
}

#[cfg(test)]
mod resource_hint_tests {
    use super::*;

    #[test]
    fn remote_list_uses_stdout_width_and_keeps_redaction_in_full_output() {
        use gat_command::{RemoteOutcome, RemoteRecord};
        use gat_core::endpoint::RemoteUrlTemplate;
        use unicode_width::UnicodeWidthStr;

        let url = format!(
            "s3://bucket/{}?secret_access_key=NEVER_DISPLAY",
            "segment/".repeat(16)
        );
        for width in [20, 39, 40, 80, 120, 200] {
            for full in [false, true] {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                let mut output = Output::new(&mut stdout, &mut stderr);
                output.set_layouts(
                    crate::output::OutputLayout::bounded(width).with_full_output(full),
                    crate::output::OutputLayout::bounded(20),
                );
                render(
                    &mut output,
                    Outcome::Remote(RemoteOutcome::List(vec![RemoteRecord {
                        name: "origin".into(),
                        url: RemoteUrlTemplate::from_string(url.clone()),
                        is_default: true,
                    }])),
                )
                .unwrap();
                let styled = String::from_utf8(stdout).unwrap();
                let text = crate::output::strip_ansi(&styled);
                assert!(!text.contains("NEVER_DISPLAY"));
                let row = text
                    .lines()
                    .find(|line| line.starts_with("✓  origin"))
                    .unwrap();
                assert!(row.starts_with("✓  origin (default)"));
                if full || width == 200 {
                    assert!(row.contains(&"segment/".repeat(16)));
                } else if width < 40 {
                    assert_eq!(row, "✓  origin (default)");
                } else {
                    assert_eq!(row.width(), width);
                    assert!(row.ends_with("..."));
                }
                assert!(stderr.is_empty());
            }
        }
    }

    #[test]
    fn selection_default_marker_is_visible_when_details_are_hidden() {
        for width in [20, 39, 40, 100] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr);
            output.set_layouts(
                crate::output::OutputLayout::bounded(width),
                crate::output::OutputLayout::default(),
            );
            render(
                &mut output,
                Outcome::Selection(gat_command::SelectionOutcome::List(vec![
                    gat_command::SelectionRecord {
                        name: "all".into(),
                        definition: Default::default(),
                        scope: gat_core::config::ConfigScope::Project,
                        is_default: true,
                    },
                ])),
            )
            .unwrap();
            let styled = String::from_utf8(stdout).unwrap();
            let text = crate::output::strip_ansi(&styled);
            let row = text
                .lines()
                .find(|line| line.starts_with("✓  all"))
                .unwrap();
            assert!(row.starts_with("✓  all (default)"));
            assert_eq!(row.contains("All tracked paths"), width >= 40);
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn initialization_commands_remain_intact_at_narrow_widths() {
        for width in [20, 40, 60] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr);
            output.set_layouts(
                crate::output::OutputLayout::default(),
                crate::output::OutputLayout::bounded(width),
            );
            render_message(
                &mut output,
                &rows::Message::new(rows::MessageKind::Action, initialization_guidance()),
            )
            .unwrap();
            assert!(stdout.is_empty());
            let plain = crate::output::strip_ansi(&String::from_utf8(stderr).unwrap());
            assert!(plain.contains("`gat add <path>`"));
            assert!(plain.contains("`gat remote add origin s3://bucket/prefix`"));
        }
    }

    #[test]
    fn empty_resource_guidance_keeps_the_command_together_at_narrow_widths() {
        let mut bytes = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut bytes, &mut stderr);
        output.set_layouts(
            crate::output::OutputLayout::bounded(40),
            crate::output::OutputLayout::default(),
        );
        resource_list(
            &mut output,
            "Routes",
            0,
            "gat route add <name> <remote> <path>",
            &[],
        )
        .unwrap();
        let rendered = crate::output::strip_ansi(&String::from_utf8(bytes).unwrap());
        assert!(rendered.contains("`gat route add <name> <remote> <path>`"));
    }

    #[test]
    fn config_source_is_dimmed_below_full_contrast_escaped_values() {
        let mut stdout = Vec::new();
        render_config_values(
            &mut Output::new(&mut stdout, &mut Vec::new()),
            gat_core::config_keys::SettingKey::GitIgnorePatterns,
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
        assert_eq!(lines[4], ui::dim("  Source: global"));
    }

    #[test]
    fn scope_metadata_is_one_dim_line_and_omits_absent_scopes() {
        use gat_core::config::ConfigScope::{Local, Project};
        for (chosen, defined, expected) in [
            (
                Some(Local),
                Some(Project),
                "Chosen in: local · Defined in: project",
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
                assert_eq!(lines[1], ui::dim(&format!("  {expected}")));
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
                "gat route add <name> <remote> <path>",
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
    fn sync_projects_only_visible_rows_and_omitted_groups_do_not_add_empty_blocks() {
        for full in [false, true] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr);
            output.set_full_output(full);
            let mut budget = output.layout(Stream::Stderr).budget(2500);
            let mut preceding = false;
            let mut projected = 0;
            for (label, total) in [
                ("Missing objects", 1000_i32),
                ("Conflicts", 1000),
                ("Corrupted objects", 500),
            ] {
                let rows = (0..total).map(|index| {
                    projected += 1;
                    rows::ListRow::new(
                        rows::ListStatus::Conflict,
                        UserLine::number(i64::from(index)),
                    )
                });
                render_sync_group(&mut output, label, rows, &mut budget, &mut preceding).unwrap();
            }
            let text = crate::output::strip_ansi(&String::from_utf8(stderr).unwrap());
            assert_eq!(projected, if full { 2500 } else { 19 });
            assert!(!text.contains("\n\n\n"));
            assert_eq!(text.contains("(... 2481 more rows)"), !full);
            assert!(text.contains("! Conflicts: 1000"));
            assert!(text.contains("! Corrupted objects: 500"));
            assert!(stdout.is_empty());
        }
    }

    #[test]
    fn sync_heading_failure_prevents_row_projection() {
        struct RejectWrites;
        impl std::io::Write for RejectWrites {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut stdout = Vec::new();
        let mut writer = RejectWrites;
        let mut output = Output::new(&mut stdout, &mut writer);
        let mut budget = output.layout(Stream::Stderr).budget(1000);
        let rows = (0..1000)
            .map(|_| -> rows::ListRow { panic!("a failed heading must not project rows") });
        assert!(
            render_sync_group(
                &mut output,
                "Missing objects",
                rows,
                &mut budget,
                &mut false
            )
            .is_err()
        );
    }

    #[test]
    fn repair_only_reports_separate_the_summary_in_both_detail_modes() {
        let outcome = gat_command::SyncOutcome {
            scope: gat_command::SelectionScope::Unrestricted,
            outcome: gat_engine::SyncOutcome::default(),
            fetched: 0,
            repaired: 0,
            reshaped: None,
            shallow: false,
            completion: gat_command::SyncCompletionStatus::Clean,
            repair_failures: vec![gat_command::RepairFailure {
                path: GatPath::parse_canonical("data.bin").unwrap(),
                oid: gat_core::oid::Oid::from_bytes([0xaa; 32]),
                error: std::sync::Arc::new(gat_command::RepairError::UnknownRemoteOverride(
                    gat_engine::UnknownRemoteOverrideError {
                        name: "missing".into(),
                    },
                )),
            }],
        };
        for full in [false, true] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr);
            output.set_full_output(full);
            render_sync(&mut output, &outcome, "Sync").unwrap();
            let text = crate::output::strip_ansi(&String::from_utf8(stderr).unwrap());
            assert!(text.ends_with("\n\n✓ Sync complete (no changes)\n"));
            assert!(!text.contains("\n\n\n"));
            assert_eq!(text.contains("hint: Use --full-output"), !full);
        }
    }

    #[test]
    fn config_list_confirmation_preserves_individual_values() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render(
            &mut Output::new(&mut stdout, &mut stderr),
            Outcome::Configured(gat_command::ConfigOutcome::SetList {
                key: gat_core::config_keys::SettingKey::GitIgnorePatterns,
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
    fn temporary_cleanup_guidance_survives_narrow_and_omitted_rows() {
        use gat_command::{
            CacheClean, CacheDbState, CacheFact, CacheInspect, DomainFact, LiveLockState, LockFact,
            LockState, SystemOutcome, SystemVerb, TemporaryCleanOutcome, TransactionKind,
            TransactionState,
        };
        let command = "`gat system clean cache --purge-temporary`";
        for width in [20, 40, 100] {
            for full in [false, true] {
                for inspect in [false, true] {
                    let mut facts = Vec::new();
                    let (verb, cache) = if inspect {
                        // Exhaust the shared row budget before the cache group.
                        facts.push(DomainFact::Lock(LockFact::Inspect(LockState {
                            live: LiveLockState::Missing,
                            transactions: (0..21)
                                .map(|id| TransactionState {
                                    id: format!("txn-{id}"),
                                    kind: TransactionKind::ScratchOnly,
                                })
                                .collect(),
                        })));
                        (
                            SystemVerb::Inspect,
                            CacheFact::Inspect(CacheInspect {
                                db: CacheDbState::Healthy,
                                temporary: 2,
                            }),
                        )
                    } else {
                        (
                            SystemVerb::Clean,
                            CacheFact::Clean(CacheClean {
                                temporary: TemporaryCleanOutcome::Preserved { count: 2 },
                                objects_purged: None,
                            }),
                        )
                    };
                    facts.push(DomainFact::Cache(cache));
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    let mut output = Output::new(&mut stdout, &mut stderr);
                    output.set_layouts(
                        crate::output::OutputLayout::bounded(width).with_full_output(full),
                        crate::output::OutputLayout::default(),
                    );
                    render_system(&mut output, SystemOutcome { verb, facts }).unwrap();
                    assert!(stderr.is_empty());
                    let styled = String::from_utf8(stdout).unwrap();
                    let plain = crate::output::strip_ansi(&styled);
                    assert_eq!(plain.matches(command).count(), 1);
                    assert_eq!(plain.contains("more rows"), inspect && !full);
                    assert!(
                        !styled
                            .lines()
                            .find(|line| line.contains(command))
                            .unwrap()
                            .contains("\x1b[2m")
                    );
                }
            }
        }
    }

    #[test]
    fn schema_recovery_prose_wraps_and_is_present_after_repair() {
        use gat_command::{
            CacheFact, CacheRepair, DomainFact, StateFact, StateRepair, SystemOutcome, SystemVerb,
        };
        for fact in [
            DomainFact::Cache(CacheFact::Repair(CacheRepair::UnsupportedVersion {
                version: 99,
            })),
            DomainFact::State(StateFact::Repair(StateRepair::NewerVersion { version: 99 })),
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr);
            output.set_layouts(
                crate::output::OutputLayout::bounded(30),
                crate::output::OutputLayout::default(),
            );
            render_system(
                &mut output,
                SystemOutcome {
                    verb: SystemVerb::Repair,
                    facts: vec![fact],
                },
            )
            .unwrap();
            let plain = crate::output::strip_ansi(&String::from_utf8(stdout).unwrap());
            assert!(plain.contains("`gat`"));
            assert!(
                plain
                    .lines()
                    .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 30)
            );
            assert!(plain.contains("repair"));
        }
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
                    state: gat_command::LockState {
                        live: gat_command::LiveLockState::Valid {
                            shard_levels: gat_core::lock::LockShardLevels::FLAT,
                            entries: 0,
                        },
                        transactions: Vec::new(),
                    },
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
        render_sync(
            &mut Output::new(&mut Vec::new(), &mut stderr),
            &outcome,
            "Sync",
        )
        .unwrap();
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
                "uncached (mount models)",
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
            assert_eq!(
                plain.matches("hint:").count(),
                2 + usize::from(include_cache_problem)
            );
            assert!(styled.contains(&ui::list_hint_text(
                &UserLine::authored("2 path(s) owned by mount 'vendor' at target 'vendor'"),
                100
            )));
            assert!(plain.contains(&format!(
                "! Push complete ({} item(s) skipped).\n\nhint: 1 path(s) owned by mount 'assets' at target 'data assets'\nhint: 2 path(s) owned by mount 'vendor' at target 'vendor'\n",
                if include_cache_problem { 4 } else { 3 }
            )));
            if include_cache_problem {
                assert!(plain.contains("\n\n! Push complete"));
            } else {
                assert!(plain.starts_with("! Push complete"));
            }
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
    fn sync_groups_share_one_limit_and_retain_hidden_group_guidance() {
        let oid = gat_core::oid::Oid::from_bytes([0xaa; 32]);
        let outcome = gat_command::SyncOutcome {
            scope: gat_command::SelectionScope::Unrestricted,
            outcome: gat_engine::SyncOutcome {
                missing: (0..25)
                    .map(|index| {
                        (
                            GatPath::parse_canonical(&format!("missing-{index}.bin")).unwrap(),
                            oid,
                        )
                    })
                    .collect(),
                conflicts: vec![GatPath::parse_canonical("hidden-conflict.bin").unwrap()],
                ..Default::default()
            },
            fetched: 0,
            repaired: 0,
            repair_failures: Vec::new(),
            reshaped: None,
            shallow: false,
            completion: gat_command::SyncCompletionStatus::Incomplete {
                conflicts: 1,
                missing: 25,
                corrupted: 0,
            },
        };
        let mut stderr = Vec::new();
        render_sync(
            &mut Output::new(&mut Vec::new(), &mut stderr),
            &outcome,
            "Sync",
        )
        .unwrap();
        let text = crate::output::strip_ansi(&String::from_utf8(stderr).unwrap());
        assert_eq!(
            text.lines().filter(|line| line.starts_with("!  ")).count(),
            19
        );
        assert_eq!(text.matches("(... 7 more rows)").count(), 1);
        assert!(!text.contains("hidden-conflict.bin"));
        assert!(text.contains("! Conflicts: 1"));
        assert!(text.contains("Use `gat sync --force`"));
    }

    #[test]
    fn hidden_status_rows_still_contribute_recovery_hints_and_totals() {
        let oid = gat_core::oid::Oid::from_bytes([0xaa; 32]);
        let rows = (0..25)
            .map(|index| gat_command::StatusRow {
                path: GatPath::parse_canonical(&format!("file-{index:02}.bin")).unwrap(),
                change: gat_engine::RowChange::Unchanged { oid },
                cache_presence: Some(if index == 24 {
                    gat_command::CachePresence::Missing
                } else {
                    gat_command::CachePresence::Present
                }),
                mount: None,
            })
            .collect();
        let mut stdout = Vec::new();
        render(
            &mut Output::new(&mut stdout, &mut Vec::new()),
            Outcome::Status(gat_command::StatusOutcome::WorkingTree {
                scope: gat_command::SelectionScope::Unrestricted,
                rows,
                changes: 0,
            }),
        )
        .unwrap();
        let text = crate::output::strip_ansi(&String::from_utf8(stdout).unwrap());
        assert!(text.contains("(... 6 more rows)"));
        assert!(!text.contains("file-24.bin"));
        assert_eq!(text.matches("Run `gat fetch`").count(), 1);
        assert!(text.ends_with("0 change(s) across 25 file(s)\n"));
    }

    #[test]
    fn full_output_preserves_problem_details() {
        let row = rows::ListRow::with_metadata(
            rows::ListStatus::Conflict,
            UserLine::authored("data.bin"),
            RowDetail::authored("a complete explanation for this particular failure")
                .with_annotation("failed"),
        );
        for full in [false, true] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr);
            if full {
                output.set_layouts(
                    super::super::OutputLayout::full(),
                    super::super::OutputLayout::full(),
                );
            }
            render_rows(&mut output, std::slice::from_ref(&row), Stream::Stdout).unwrap();
            let text = crate::output::strip_ansi(&String::from_utf8(stdout).unwrap());
            assert_eq!(text.contains("complete explanation"), full);
            assert_eq!(text.contains("failed"), !full);
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
        assert!(row.contains("\x1b[2muncached"));
        assert_eq!(crate::output::strip_ansi(row), "✓  data.bin  uncached");
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
                    let hint = ui::list_hint_text(&note, 100);
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
                    if pull {
                        "✓ Pull complete (no changes)"
                    } else {
                        "✓ Sync complete (no changes)"
                    }
                );
                assert_eq!(
                    lines[2..].join("\n"),
                    ui::list_hint_text(&selection_scope_note(scope).unwrap(), 100)
                );
                assert!(lines[2].starts_with("\x1b[2mhint: "));
            }
        }
    }

    #[test]
    fn sync_summaries_distinguish_completion_preview_and_incomplete_outcomes() {
        use gat_command::SyncCompletionStatus;
        for pull in [false, true] {
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
                let outcome = if pull {
                    Outcome::Pulled(outcome)
                } else {
                    Outcome::Synced(outcome)
                };
                render(&mut Output::new(&mut Vec::new(), &mut stderr), outcome).unwrap();
                let styled = String::from_utf8(stderr).unwrap();
                assert!(!styled.contains("\x1b[2m"));
                assert_eq!(
                    crate::output::strip_ansi(&styled),
                    if pull {
                        expected.replace("Sync", "Pull")
                    } else {
                        expected.to_owned()
                    }
                );
            }
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

#[cfg(test)]
mod complete_report_tests {
    use super::*;
    use gat_command::{
        DomainFact, GitFact, GitInspect, LiveLockInvalidReason, LiveLockState, LockFact, LockState,
        SystemOutcome, SystemVerb, TransactionKind, TransactionState,
    };
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn system_explanations_survive_hidden_rows_and_full_mode_without_duplication() {
        for width in [20, 39, 40, 60, 100, 120] {
            for full in [false, true] {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                let mut output = Output::new(&mut stdout, &mut stderr);
                output.set_layouts(
                    super::super::OutputLayout::bounded(width).with_full_output(full),
                    super::super::OutputLayout::bounded(1),
                );
                render_system(
                    &mut output,
                    SystemOutcome {
                        verb: SystemVerb::Inspect,
                        facts: vec![
                            DomainFact::Lock(LockFact::Inspect(LockState {
                                live: LiveLockState::Invalid {
                                    reason: LiveLockInvalidReason::NeitherFileNorShardTree,
                                },
                                transactions: (0..25)
                                    .map(|index| TransactionState {
                                        id: format!("txn-{index}"),
                                        kind: TransactionKind::ScratchOnly,
                                    })
                                    .collect(),
                            })),
                            DomainFact::Git(GitFact::Inspect(GitInspect::PresentButUnvalidated)),
                        ],
                    },
                )
                .unwrap();
                assert!(stderr.is_empty());
                let styled = String::from_utf8(stdout).unwrap();
                let text = crate::output::strip_ansi(&styled);
                let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(
                    words.contains(
                        "gat.lock: live gat.lock path is neither a file nor a shard tree"
                    )
                );
                assert_eq!(
                    words
                        .matches("excludes: present but could not be validated")
                        .count(),
                    1
                );
                assert!(words.contains("25 findings: incomplete transaction scratch remains"));
                assert_eq!(
                    words
                        .matches("incomplete transaction scratch remains")
                        .count(),
                    1
                );
                assert_eq!(text.contains("more rows"), !full);
                assert!(text.contains("\n\n✗ Lock\n\n"));
                assert!(!text.contains("\n\n\n"));
                assert!(
                    text.lines().all(|line| {
                        // Full list rows are deliberately unbounded; prose is not.
                        (full && (line.starts_with("✗  ") || line.starts_with("!  ")))
                            || line.width() <= width.min(100)
                    }),
                    "{text}"
                );
                for line in styled
                    .lines()
                    .filter(|line| line.contains("excludes:") || line.contains("gat.lock:"))
                {
                    assert!(!line.contains("\x1b[2m"));
                }
            }
        }
    }

    #[test]
    fn integration_step_prose_wraps_instead_of_becoming_a_protected_phrase() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr);
        output.set_layouts(
            super::super::OutputLayout::default(),
            super::super::OutputLayout::bounded(20),
        );
        render_git_integration_step(
            &mut output,
            "gat.lock semantic merge driver",
            gat_command::InitGitIntegrationOutcome::Installed,
        )
        .unwrap();
        let text = crate::output::strip_ansi(&String::from_utf8(stderr).unwrap());
        assert!(text.lines().all(|line| line.width() <= 20), "{text}");
        assert_eq!(
            text.split_whitespace().collect::<Vec<_>>().join(" "),
            "✓ Installed gat.lock semantic merge driver."
        );
    }
}
