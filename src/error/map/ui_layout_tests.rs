//! Exercise real mapper output, rather than idealized prose supplied to flow.
use crate::error::Failure;
use crate::output::{Output, OutputLayout};
use gat_core::config::{ConfigScope, IngestStrategy, MaterializationMode};
use unicode_width::UnicodeWidthStr;

fn render(failure: &Failure, width: usize, full: bool) -> String {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut stdout, &mut stderr);
    output.set_layouts(OutputLayout::bounded(1), OutputLayout::bounded(width));
    output.set_full_output(full);
    crate::output::error::render(&mut output, failure.diagnostic()).unwrap();
    assert!(stdout.is_empty());
    crate::output::strip_ansi(&String::from_utf8(stderr).unwrap())
}

#[test]
fn canonical_alternatives_wrap_between_values_without_losing_choices() {
    let failures = [
        Failure::from(
            gat_command::ConfigRequest::from_raw(
                "selection".into(),
                gat_command::ConfigAction::Get,
                ConfigScope::Project,
            )
            .unwrap_err(),
        ),
        Failure::from("invalid".parse::<MaterializationMode>().unwrap_err()),
        Failure::from("invalid".parse::<IngestStrategy>().unwrap_err()),
    ];
    for (failure, choices) in failures.iter().zip([
        gat_core::config_keys::ConfigKey::CANONICAL
            .into_iter()
            .map(gat_core::config_keys::ConfigKey::as_str)
            .collect::<Vec<_>>(),
        vec!["reflink", "hardlink", "symlink", "copy"],
        vec!["safe", "hybrid", "mmap"],
    ]) {
        for width in [20, 39, 40, 60, 100, 120] {
            let text = render(failure, width, false);
            assert_eq!(text, render(failure, width, true));
            for choice in &choices {
                assert!(text.contains(choice));
            }
            for line in text.lines() {
                // Only a single oversized key or prose word can exceed the cap.
                if line.width() > width.min(100) {
                    assert_eq!(line.split_whitespace().count(), 1, "{line}");
                }
            }
            assert!(!text.contains("\n\n\n"));
        }
    }
}

#[test]
fn mapped_commands_remain_copyable_in_summaries_details_and_hints() {
    use gat_command::{AddError, RemoteError, SystemError};
    use gat_core::{lexical_path::GatPath, name::RemoteName};
    for (failure, command) in [
        (
            Failure::from(RemoteError::DefaultWouldDangle {
                name: RemoteName::from_string("origin".into()),
            }),
            "`gat remote default`",
        ),
        (
            Failure::from(SystemError::RecoveryChoiceOnlyForLock),
            "`gat system repair lock`",
        ),
        (
            Failure::from(SystemError::CachePurgeOnlyForCacheScope),
            "`gat system clean cache`",
        ),
        (
            Failure::from(crate::app::AppError::HistoryRequiresRemote),
            "`gat status`",
        ),
        (
            Failure::from(AddError::AlreadyGitTracked {
                path: GatPath::parse_canonical("a file.bin").unwrap(),
            }),
            "`git rm --cached a file.bin`",
        ),
    ] {
        for width in [20, 39, 40, 60, 100, 120] {
            let text = render(&failure, width, false);
            assert!(text.contains(command), "{width}: {text}");
            assert_eq!(text, render(&failure, width, true));
        }
    }
}

#[test]
fn resource_hints_distinguish_reads_from_writes_and_keep_commands_copyable() {
    use gat_command::{ConfigAction, ConfigRequest};
    for (key, inspect, update) in [
        (
            "remotes.default",
            "gat remote default",
            "gat remote default NAME",
        ),
        (
            "selections.default",
            "gat selection default",
            "gat selection default NAME",
        ),
        (
            "remotes.origin.url",
            "gat remote show NAME",
            "gat remote update NAME --url URL",
        ),
        (
            "routes.models.path",
            "gat route show NAME",
            "gat route update NAME --path PATH --remote REMOTE",
        ),
        (
            "mounts.models.rev_lock",
            "gat mount show NAME",
            "gat mount update NAME",
        ),
        (
            "mounts.models.rev",
            "gat mount show NAME",
            "gat mount update NAME",
        ),
        (
            "selections.runtime.path",
            "gat selection show NAME",
            "gat selection update NAME",
        ),
    ] {
        for (action, expected) in [
            (ConfigAction::Get, inspect),
            (ConfigAction::Set(vec!["value".into()]), update),
        ] {
            let failure = Failure::from(
                ConfigRequest::from_raw(key.into(), action, ConfigScope::Project).unwrap_err(),
            );
            for width in [20, 40, 80] {
                let text = render(&failure, width, false);
                assert!(text.contains(expected), "{width}: {text}");
                assert_eq!(
                    text.contains("generated"),
                    key == "mounts.models.rev_lock" && expected == update,
                    "unexpected revision hint: {text}"
                );
                if key.ends_with(".default") {
                    assert!(!text.contains("show NAME"));
                    assert!(!text.contains("update NAME"));
                }
                if expected == inspect {
                    assert!(!text.contains(update), "read suggests mutation: {text}");
                }
            }
        }
    }
}
