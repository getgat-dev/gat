//! Enforces that `src/lifecycle.rs`'s static tables stay
//! in sync with the three places that can otherwise drift independently:
//! concise Clap `--help` text, `app::run`'s runtime notices, and command
//! reference metadata. Every entry in
//! `lifecycle::commands()` is enumerated here so a status
//! change added to the table without updating `cli.rs`/regenerating docs
//! fails a test immediately instead of silently drifting.

use clap::{CommandFactory, Parser};
use gat::app::{self, Context};
use gat::cli::{self, Cli};
use gat::lifecycle::{self, NoticeKind, Status};
use gat::progress::NoopProgress;

#[path = "common/mod.rs"]
mod common;
use common::test_repo;

/// Delegates to the shared `test-support` crate's `parse_cli`, so this
/// file doesn't keep its own duplicate of the fixed `"gat"` argv[0]
/// prepending logic.
fn parse(args: &[&str]) -> Cli {
    test_support::parse_cli(args)
}

/// One minimal, always-successful invocation per experimental command,
/// used both to check dispatch notices and (indirectly) that the command
/// stays discoverable/executable.
fn minimal_success_args(name: &str) -> &'static [&'static str] {
    match name {
        "gc" => &["gc", "--dry-run"],
        "mount" => &["mount", "list"],
        "route" => &["route", "list"],
        "selection" => &["selection", "list"],
        "system" => &["system", "inspect"],
        other => panic!("no minimal invocation registered for experimental command {other}"),
    }
}

/// Every [`Status::Experimental`] command in [`lifecycle::experimental_commands`]
/// (not every [`lifecycle::commands`] entry -- the invariants here only
/// apply to the Experimental status, so this stays correct even once a
/// command registered with a different status must have
/// concise `--help` text without lifecycle wording, dispatch to exactly
/// one [`NoticeKind::Experimental`] notice naming it, and have a
/// command reference metadata with `tag: "Experimental"` and
/// the standard callout -- proving `cli.rs`, `app::run`, and the
/// command references all agree with the lifecycle table.
#[test]
fn every_experimental_command_stays_in_sync_across_help_notices_and_docs() {
    let root = Cli::command();

    for spec in lifecycle::experimental_commands() {
        assert_eq!(
            spec.status,
            Status::Experimental,
            "experimental_commands() must only yield Experimental-status commands"
        );
        let name = match spec.surface {
            lifecycle::Surface::Command(name) => name,
            other => panic!("unexpected non-command surface in lifecycle::commands(): {other:?}"),
        };

        // 1. `--help` text.
        let sub = root
            .find_subcommand(name)
            .unwrap_or_else(|| panic!("no `{name}` subcommand found on the CLI"));
        let about = sub
            .get_long_about()
            .or_else(|| sub.get_about())
            .unwrap_or_else(|| panic!("`{name}` has no about/long_about text"))
            .to_string();
        assert!(
            !about.contains(lifecycle::EXPERIMENTAL_COMMAND_HELP_PREFIX)
                && !about.contains("Experimental"),
            "`gat {name} --help` must leave lifecycle rendering to the registry, got: {about}"
        );

        // 2. Runtime notice.
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let context = Context::new(repo);
        let result = app::run(parse(minimal_success_args(name)), &context, &NoopProgress);
        result.unwrap_or_else(|err| {
            panic!(
                "`gat {name}` dispatch failed: {}",
                err.diagnostic().summary()
            )
        });
        let notices = context.lifecycle.take_notices();
        assert_eq!(
            notices.len(),
            1,
            "`gat {name}` must produce exactly one lifecycle notice, got {notices:?}"
        );
        assert_eq!(notices[0].kind, NoticeKind::Experimental);
        assert_eq!(notices[0].subject, spec.subject);

        // 3. Command references.
        let doc_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/commands")
            .join(format!("{name}.mdx"));
        let doc = std::fs::read_to_string(&doc_path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", doc_path.display()));
        assert!(
            doc.contains("tag: \"Experimental\""),
            "{} must carry `tag: \"Experimental\"` frontmatter",
            doc_path.display()
        );
        assert!(
            doc.contains("is **Experimental**"),
            "{} must carry the standard Experimental callout",
            doc_path.display()
        );
    }
}

/// Lifecycle wording is registry-rendered and must not leak back into
/// concise clap descriptions.
#[test]
fn no_command_help_duplicates_the_experimental_lifecycle_prefix() {
    let root = Cli::command();
    for sub in root.get_subcommands() {
        let about = sub
            .get_long_about()
            .or_else(|| sub.get_about())
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        assert!(
            !about.contains(lifecycle::EXPERIMENTAL_COMMAND_HELP_PREFIX),
            "`gat {}` duplicates lifecycle wording in clap help",
            sub.get_name()
        );
    }
}

/// A failing experimental command must still surface its notice: the
/// dispatch match arm reports the command was observed before calling
/// into the command implementation itself, so `context.lifecycle`
/// carries the notice independent of whether the command goes on to
/// succeed (see `app::run`'s doc comment).
#[test]
fn failing_experimental_command_still_returns_its_notice() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    // `gat mount list` never fails, so force a failure via `gat gc
    // --remote <name>` naming a remote that was never configured.
    let result = app::run(
        parse(&["gc", "--dry-run", "--remote", "does-not-exist"]),
        &context,
        &NoopProgress,
    );

    assert!(
        result.is_err(),
        "expected gc with conflicting history flags to fail"
    );
    let notices = context.lifecycle.take_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, NoticeKind::Experimental);
    assert_eq!(notices[0].subject, "`gat gc`");
}

/// Sanity check that `cli::Cli` itself is reachable the same way
/// `main.rs`/docs generation build it, so the `find_subcommand` lookups
/// above reflect the real CLI shape.
#[test]
fn cli_command_tree_exposes_gc_mount_route_selection_system() {
    let root = Cli::command();
    for name in ["gc", "mount", "route", "selection", "system"] {
        assert!(
            root.find_subcommand(name).is_some(),
            "missing `{name}` subcommand"
        );
    }
    assert!(
        root.find_subcommand("hooks").is_none(),
        "`gat hooks` was removed in favor of declarative `gat init`"
    );
    let _ = cli::Cli::parse_from(["gat", "status"]);
}
