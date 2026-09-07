//! CLI-level characterization of malformed/invalid `gat.yaml` failures
//! through the extracted typed `gat-command` error boundary.

use crate::common::{gat, init_repo, stderr, stdout};

#[test]
fn malformed_config_yaml_fails_cleanly_without_a_panic_or_raw_parser_text() {
    let repo = init_repo();
    // An unterminated flow sequence is invalid YAML syntax.
    repo.write("gat.yaml", "version: 1\nmounts: [this is not closed\n");

    let out = gat(repo.path(), &["config", "cache.materialization_strategy"]);
    assert!(!out.status.success(), "expected a nonzero exit code");
    let err = stderr(&out);
    assert!(
        err.contains("Could not load the project gat.yaml"),
        "expected the typed repository diagnostic: {err}"
    );
    assert!(
        !err.contains("panicked at"),
        "must fail cleanly, not panic: {err}"
    );
    assert!(
        stdout(&out).is_empty(),
        "no stdout expected on this failure path"
    );
}

#[test]
fn invalid_config_value_fails_cleanly_without_a_panic_or_raw_parser_text() {
    let repo = init_repo();
    // `cache.materialization_strategy` only accepts known mode names.
    repo.write(
        "gat.yaml",
        "version: 1\ncache:\n  materialization_strategy: [not-a-real-mode]\n",
    );

    let out = gat(repo.path(), &["config", "cache.materialization_strategy"]);
    assert!(!out.status.success(), "expected a nonzero exit code");
    let err = stderr(&out);
    assert!(
        err.contains("Could not load the project gat.yaml"),
        "expected the typed repository diagnostic: {err}"
    );
    assert!(
        !err.contains("panicked at"),
        "must fail cleanly, not panic: {err}"
    );
    assert!(
        stdout(&out).is_empty(),
        "no stdout expected on this failure path"
    );
}

#[test]
fn unsupported_config_version_fails_cleanly_without_a_panic_or_raw_parser_text() {
    let repo = init_repo();
    repo.write("gat.yaml", "version: 999999\n");

    let out = gat(repo.path(), &["config", "cache.materialization_strategy"]);
    assert!(!out.status.success(), "expected a nonzero exit code");
    let err = stderr(&out);
    assert!(
        err.contains("Could not load the project gat.yaml"),
        "expected the typed repository diagnostic: {err}"
    );
    assert!(
        !err.contains("panicked at"),
        "must fail cleanly, not panic: {err}"
    );
    assert!(
        stdout(&out).is_empty(),
        "no stdout expected on this failure path"
    );
}
