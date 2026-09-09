//! Named selection parsing, process behavior, and result output.
use crate::common::{gat, init_repo, stderr, stdout};

#[test]
fn named_selection_reuse_default_and_inline_replacement() {
    let repo = init_repo();
    for path in [
        "models/a.onnx",
        "models/experimental/b.onnx",
        "datasets/a.parquet",
    ] {
        repo.write(path, path);
    }
    assert!(
        gat(repo.path(), &["add", "models", "datasets"])
            .status
            .success()
    );
    for args in [
        vec![
            "selection",
            "add",
            "runtime",
            "--path",
            "models",
            "--include",
            "**/*.onnx",
            "--exclude",
            "experimental/**",
        ],
        vec![
            "selection",
            "add",
            "training",
            "--path",
            "datasets",
            "--include",
            "**/*.parquet",
        ],
    ] {
        let result = gat(repo.path(), &args);
        assert!(result.status.success(), "{}", stderr(&result));
    }
    // Saving the first definition never activates it.
    assert_eq!(
        stdout(&gat(repo.path(), &["ls-files"]))
            .lines()
            .filter(|line| line.starts_with("✓  "))
            .count(),
        3
    );
    assert!(
        gat(repo.path(), &["selection", "default", "runtime"])
            .status
            .success()
    );
    let saved = gat(&repo.path().join("datasets"), &["ls-files"]);
    assert_eq!(
        stdout(&saved),
        "✓ Tracked files: 1\n\n✓  models/a.onnx\n\nhint: Configured path selection applied; other paths were not checked. Use --path . to select the\n      whole repository.\n\n1 file(s)\n"
    );
    assert!(!stderr(&saved).contains("Configured path selection applied"));
    let named = gat(repo.path(), &["ls-files", "--selection", "training"]);
    assert!(named.status.success(), "{}", stderr(&named));
    assert_eq!(
        stdout(&named),
        "✓ Tracked files: 1\n\n✓  datasets/a.parquet\n\nhint: Results cover selected paths only; other paths were not checked.\n\n1 file(s)\n"
    );
    assert_eq!(stdout(&gat(repo.path(), &["ls-files"])), stdout(&saved));
    assert_eq!(
        stdout(&gat(repo.path(), &["ls-files", "--path", "."]))
            .lines()
            .filter(|line| line.starts_with("✓  "))
            .count(),
        3
    );
    assert!(
        gat(
            repo.path(),
            &["selection", "default", "training", "--local"]
        )
        .status
        .success()
    );
    assert_eq!(
        stdout(&gat(repo.path(), &["ls-files"]))
            .lines()
            .filter(|line| line.starts_with("✓  "))
            .collect::<Vec<_>>(),
        stdout(&named)
            .lines()
            .filter(|line| line.starts_with("✓  "))
            .collect::<Vec<_>>()
    );
    let details = gat(repo.path(), &["selection", "default"]);
    let rendered = stdout(&details);
    assert!(rendered.contains("Chosen in: local"));
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.starts_with('✓'))
            .count(),
        1
    );
    assert!(rendered.starts_with(
        "✓ Default selection: training\n  Chosen in: local · Defined in: project\n\n"
    ));
    assert!(stdout(&details).contains("Defined in: project"));
    assert!(
        gat(repo.path(), &["selection", "default", "--local", "--unset"])
            .status
            .success()
    );
    assert_eq!(stdout(&gat(repo.path(), &["ls-files"])), stdout(&saved));
}

#[test]
fn selection_management_respects_scope_and_validates_removal() {
    let repo = init_repo();
    for args in [
        vec![
            "selection",
            "add",
            "runtime",
            "--path",
            "models",
            "--exclude",
            "experimental/**",
        ],
        vec!["selection", "default", "runtime", "--local"],
    ] {
        assert!(gat(repo.path(), &args).status.success());
    }
    assert!(
        !gat(
            repo.path(),
            &[
                "selection",
                "update",
                "runtime",
                "--local",
                "--path",
                "data"
            ]
        )
        .status
        .success()
    );
    assert!(
        gat(
            repo.path(),
            &["selection", "add", "runtime", "--local", "--path", "data"]
        )
        .status
        .success()
    );
    let show = gat(repo.path(), &["selection", "show", "runtime"]);
    assert!(stdout(&show).contains("Path:    data"));
    assert!(!stdout(&show).contains("experimental"));
    assert!(
        !gat(
            repo.path(),
            &["selection", "update", "runtime", "--path", "ignored"]
        )
        .status
        .success()
    );
    let removed = gat(repo.path(), &["selection", "remove", "runtime", "--local"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(stderr(&removed).contains("→ Revealed definition in: project"));
    assert!(
        !gat(repo.path(), &["selection", "remove", "runtime"])
            .status
            .success()
    );
    assert!(
        gat(repo.path(), &["selection", "default", "--local", "--unset"])
            .status
            .success()
    );
    assert!(
        gat(repo.path(), &["selection", "remove", "runtime"])
            .status
            .success()
    );
}

#[test]
fn named_selection_rejects_ambiguity_unknown_names_and_invalid_paths() {
    let repo = init_repo();
    for args in [
        vec!["ls-files", "--selection", "missing"],
        vec!["ls-files", "--selection", "runtime", "--path", "."],
        vec!["ls-files", "--selection", "runtime", "--include", "**"],
        vec!["ls-files", "--selection", "runtime", "--exclude", "**"],
        vec![
            "ls-files",
            "--selection",
            "runtime",
            "--selection",
            "training",
        ],
        vec!["selection", "add", "default"],
        vec!["selection", "default", "missing"],
        vec!["selection", "add", "bad", "--path", "../escape"],
        vec!["selection", "add", "bad", "--include", "["],
    ] {
        assert!(!gat(repo.path(), &args).status.success(), "{args:?}");
    }
    repo.write("gat.yaml", "selections:\n  default: missing\n");
    for command in ["ls-files", "status", "diff"] {
        let result = gat(repo.path(), &[command]);
        assert!(!result.status.success(), "{command}");
    }
    // Unsetting can repair a manually authored dangling reference.
    assert!(
        gat(repo.path(), &["selection", "default", "--unset"])
            .status
            .success()
    );
}

#[test]
fn generic_config_cannot_write_resource_settings_and_legacy_selection_is_rejected() {
    let repo = init_repo();
    for key in [
        "remotes.default",
        "mounts.assets.path",
        "routes.assets.path",
        "selection.path",
        "selection.include",
        "selections.default",
        "selections.runtime.path",
    ] {
        assert!(
            !gat(repo.path(), &["config", key, "runtime"])
                .status
                .success(),
            "{key}"
        );
    }
    repo.write("gat.yaml", "selection:\n  include: ['models/**']\n");
    assert!(!gat(repo.path(), &["ls-files"]).status.success());
}

#[test]
fn invalid_selection_reports_typed_errors_without_parser_text() {
    let repo = init_repo();
    repo.write(
        "gat.yaml",
        "selections:\n  runtime:\n    include: ['[SENTINEL_SELECTION_ERROR']\n",
    );
    for command in ["status", "diff", "ls-files"] {
        let result = gat(repo.path(), &[command]);
        assert!(!result.status.success());
        assert!(!stderr(&result).contains("SENTINEL_SELECTION_ERROR"));
    }
}

#[test]
fn remote_defaults_are_explicit_and_scope_safe() {
    let repo = init_repo();
    let remote_dir = tempfile::tempdir().unwrap();
    let url = crate::support::remote_url(remote_dir.path());
    for name in ["origin", "backup"] {
        let added = gat(repo.path(), &["remote", "add", name, &url]);
        assert!(added.status.success(), "{}", stderr(&added));
    }
    assert!(stdout(&gat(repo.path(), &["remote", "default"])).contains("none"));
    assert!(
        gat(repo.path(), &["remote", "default", "origin", "--local"])
            .status
            .success()
    );
    let default = gat(repo.path(), &["remote", "default"]);
    assert!(stdout(&default).contains("Chosen in: local"));
    assert!(stdout(&default).contains("Defined in: project"));
    assert!(
        !gat(repo.path(), &["remote", "remove", "origin"])
            .status
            .success()
    );
    assert!(
        !gat(
            repo.path(),
            &["remote", "update", "origin", "--local", "--url", &url]
        )
        .status
        .success()
    );
    assert!(
        gat(repo.path(), &["remote", "default", "--local", "--unset"])
            .status
            .success()
    );
    assert!(
        gat(repo.path(), &["remote", "remove", "origin"])
            .status
            .success()
    );
    assert!(stdout(&gat(repo.path(), &["remote", "default"])).contains("none"));
}

#[test]
fn selection_updates_preserve_omitted_fields_and_clear_lists_explicitly() {
    let repo = init_repo();
    for path in ["archive/a.bin", "archive/a.onnx", "archive/scratch/b.bin"] {
        repo.write(path, path);
    }
    assert!(gat(repo.path(), &["add", "archive"]).status.success());
    for args in [
        vec![
            "selection",
            "add",
            "runtime",
            "--path",
            "models",
            "--include",
            "**/*.bin",
            "--exclude",
            "scratch/**",
        ],
        vec!["selection", "update", "runtime", "--path", "archive"],
    ] {
        assert!(gat(repo.path(), &args).status.success());
    }
    assert_eq!(
        stdout(&gat(repo.path(), &["ls-files", "--selection", "runtime"])),
        "✓ Tracked files: 1\n\n✓  archive/a.bin\n\nhint: Results cover selected paths only; other paths were not checked.\n\n1 file(s)\n"
    );
    assert!(
        gat(
            repo.path(),
            &["selection", "update", "runtime", "--clear-exclude"]
        )
        .status
        .success()
    );
    assert_eq!(
        stdout(&gat(repo.path(), &["ls-files", "--selection", "runtime"]))
            .lines()
            .filter(|line| line.starts_with("✓  "))
            .count(),
        2
    );
    assert!(
        gat(
            repo.path(),
            &["selection", "update", "runtime", "--clear-include"]
        )
        .status
        .success()
    );
    assert_eq!(
        stdout(&gat(repo.path(), &["ls-files", "--selection", "runtime"]))
            .lines()
            .filter(|line| line.starts_with("✓  "))
            .count(),
        3
    );
    assert!(stdout(&gat(repo.path(), &["selection", "default"])).contains("none"));
}
