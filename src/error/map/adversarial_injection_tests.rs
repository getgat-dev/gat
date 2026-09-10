//! Focused adversarial tests inject a control-character/newline payload
//! into every named dynamic surface (remote
//! name, mount name, route name, revision, config key) through its real
//! typed error and mapper, and assert the resulting `Diagnostic` can
//! never forge an extra rendered line or raw control sequence.
//!
//! `UserLine`'s own construction-time escaping (`src/presentation.rs`)
//! is what actually guarantees this; these tests exist to prove each
//! named surface is wired through it end-to-end via its real mapper,
//! not to re-prove `UserLine`'s escaping itself. Path and progress-path
//! injection are covered by
//! `gc_repository_path_containing_raw_control_characters_is_always_escaped`
//! (`tests/cli_integration/error_leak_adversarial.rs`) and
//! `activity_line_escapes_control_characters_instead_of_collapsing_them`
//! (`src/output/progress.rs`) respectively.

use crate::error::Failure;
use gat_command::{ConfigError, MountError, RouteError};
use gat_core::name::{MountName, RemoteName, RouteName};

const PAYLOAD: &str = "evil\x1b[31m\n\rinjected\x07pwned";

fn assert_no_forged_line(failure: &Failure) {
    let diagnostic = failure.diagnostic();
    assert!(!diagnostic.summary().contains('\n'));
    assert!(!diagnostic.summary().contains('\x1b'));
    if let Some(subject) = diagnostic.subject() {
        assert!(!subject.contains('\n'));
        assert!(!subject.contains('\x1b'));
        assert!(!subject.contains('\r'));
        assert!(!subject.contains('\x07'));
    }
    for hint in diagnostic.hints() {
        assert!(!hint.contains('\n'));
        assert!(!hint.contains('\x1b'));
    }
}

#[test]
fn unknown_remote_name_never_forges_an_extra_rendered_line() {
    let failure: Failure = RouteError::UnknownRemote {
        remote: RemoteName::from_string(PAYLOAD.to_string()),
    }
    .into();
    assert_no_forged_line(&failure);
}

#[test]
fn route_already_exists_name_never_forges_an_extra_rendered_line() {
    let failure: Failure = RouteError::AlreadyExists {
        name: RouteName::from_string(PAYLOAD.to_string()),
    }
    .into();
    assert_no_forged_line(&failure);
}

#[test]
fn mount_already_exists_name_never_forges_an_extra_rendered_line() {
    let failure: Failure = MountError::AlreadyExists {
        name: MountName::from_string(PAYLOAD.to_string()),
    }
    .into();
    assert_no_forged_line(&failure);
}

#[test]
fn unknown_config_key_never_forges_an_extra_rendered_line() {
    let failure: Failure = ConfigError::UnknownKey {
        key: PAYLOAD.to_string(),
    }
    .into();
    assert_no_forged_line(&failure);
}

#[test]
fn unresolvable_revision_never_forges_an_extra_rendered_line() {
    let tmp = ::test_support::empty_git_repo();
    let repo = gat_engine::Repository::at(tmp.path().to_path_buf());
    let selection = gat_core::history::HistorySelection {
        roots: vec![gat_core::history::HistoryRoot::Revision(PAYLOAD.into())],
        ..Default::default()
    };
    let error = repo
        .visit_history_commits::<gat_engine::HistoryError>(&selection, |_| Ok(()))
        .unwrap_err();
    let failure: Failure = error.into();
    assert_no_forged_line(&failure);
}
