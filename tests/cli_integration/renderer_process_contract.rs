//! Renderer/process-boundary contracts: `NO_COLOR` handling, exit-code contracts, and the
//! ordering of progress/notices/errors relative to each other.

use crate::common::{
    assert_failure_code, assert_ok, assert_success, commit_all, gat, gat_with_env, init_repo,
    stderr,
};

/// A real spawned process run with `NO_COLOR=1` must never emit ANSI
/// escape bytes on either stream, even on a failing command (where the
/// renderer's `✗ error:` path is exercised).
#[test]
fn no_color_env_var_suppresses_ansi_escapes_on_a_real_spawned_process() {
    let tmp = tempfile::tempdir().unwrap();

    // A failing command (not a repo here) exercises the error renderer.
    let out = gat_with_env(tmp.path(), &["status"], &[("NO_COLOR", Some("1"))]);
    assert!(!out.status.success(), "expected `status` to fail here");
    assert_no_ansi_escape_bytes(&out.stdout);
    assert_no_ansi_escape_bytes(&out.stderr);

    // A successful command exercises the ordinary success renderer too.
    let tmp2 = init_repo();
    let dir = tmp2.path();
    std::fs::write(dir.join("a.bin"), b"payload").unwrap();
    let out = gat_with_env(dir, &["add", "a.bin"], &[("NO_COLOR", Some("1"))]);
    assert_ok(&out, "gat add with NO_COLOR=1");
    assert_no_ansi_escape_bytes(&out.stdout);
    assert_no_ansi_escape_bytes(&out.stderr);
}

/// Raw byte-level check (not the ANSI-stripped string comparison used
/// elsewhere) for the ESC (`0x1B`) control byte that begins every ANSI
/// escape sequence.
fn assert_no_ansi_escape_bytes(bytes: &[u8]) {
    assert!(
        !bytes.contains(&0x1B),
        "expected no ANSI escape (ESC, 0x1B) bytes with NO_COLOR=1, got: {:?}",
        String::from_utf8_lossy(bytes)
    );
}

/// Application-level failures (typed `Failure`, e.g. "not a repository")
/// exit exactly `1`.
#[test]
fn application_failure_exits_exactly_one() {
    let tmp = tempfile::tempdir().unwrap();
    let args = ["status"];
    let out = gat(tmp.path(), &args);
    assert_failure_code(tmp.path(), &args, &out, 1);
}

/// Clap usage/syntax failures (e.g. a required argument omitted) exit
/// exactly `2`, distinct from every application-level `Failure`'s `1`,
/// and never go through Gat's own `✗ error:` renderer.
#[test]
fn clap_usage_failure_exits_exactly_two_and_skips_the_application_renderer() {
    let tmp = init_repo();
    let args = ["add"];
    let out = gat(tmp.path(), &args);
    assert_failure_code(tmp.path(), &args, &out, 2);
    let err = stderr(&out);
    assert!(err.contains("Usage:"), "expected Clap usage text: {err}");
    assert!(
        !err.contains('✗'),
        "Clap failures must never use Gat's application error renderer: {err}"
    );
}

/// A successful command exits exactly `0`.
#[test]
fn successful_command_exits_exactly_zero() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("a.bin"), b"payload").unwrap();
    let out = gat(dir, &["add", "a.bin"]);
    assert_success(dir, &["add", "a.bin"], &out);
}

/// A lifecycle notice (experimental-command warning) that fires on a
/// command which then goes on to fail must still be rendered, ahead of
/// the final error -- notices are not suppressed just because the
/// overall command later fails, and "notice, then error" ordering must
/// hold within the single combined stderr stream.
#[test]
fn lifecycle_notice_still_renders_before_the_final_error_when_the_command_then_fails() {
    let tmp = init_repo();
    let dir = tmp.path();

    let out = gat(dir, &["gc", "--remote", "does-not-exist"]);
    assert!(
        !out.status.success(),
        "expected `gat gc --remote does-not-exist` to fail"
    );
    let err = stderr(&out);
    let notice_pos = err.find("Experimental");
    assert!(
        notice_pos.is_some(),
        "expected the experimental-command notice on stderr: {err}"
    );
    let error_pos = err.find('✗');
    assert!(
        error_pos.is_some(),
        "expected the application error renderer's `✗` marker on stderr: {err}"
    );
    assert!(
        notice_pos.unwrap() < error_pos.unwrap(),
        "expected the lifecycle notice to render before the final error: {err}"
    );
}

/// Progress UI (spinner/bar redraws) must be fully finished -- no
/// trailing carriage-return redraw sequences -- before any outcome rows
/// or the final error render, on both success and failure paths.
#[test]
fn progress_ui_is_finished_before_outcome_rows_render() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("a.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add");
    commit_all(dir, "add a.bin");

    let out = gat(dir, &["sync"]);
    assert_ok(&out, "gat sync");
    let err = stderr(&out);
    // Any in-place progress redraw uses '\r'; once the final outcome is
    // printed there must be no further carriage-return before it -- i.e.
    // no '\r' appears anywhere after the last '\r' in the same buffer
    // followed by real content without a redraw. The simplest robust
    // check: the very last emitted line (after the final newline) must
    // not itself contain a bare '\r' redraw artifact.
    if let Some(last_line) = err.lines().last() {
        assert!(
            !last_line.contains('\r'),
            "expected the final rendered line to be a finished line, not a mid-redraw \
             progress artifact: {err:?}"
        );
    }
}

#[cfg(target_os = "linux")]
fn full_output() -> std::fs::File {
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap()
}

fn output_command(dir: &std::path::Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(crate::common::gat_bin());
    command.current_dir(dir).args(args).env("NO_COLOR", "1");
    crate::common::isolated_child_env(&mut command);
    command
}

#[test]
fn successful_status_has_exact_output_bytes() {
    let repo = init_repo();
    let out = output_command(repo.path(), &["status"]).output().unwrap();
    assert_ok(&out, "status");
    assert_eq!(out.stdout, "✓ Gat lock: no gat-tracked files\n".as_bytes());
    assert!(out.stderr.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn stdout_failure_after_add_keeps_the_completed_mutation() {
    let repo = init_repo();
    std::fs::write(repo.path().join("payload.bin"), b"payload").unwrap();
    let out = output_command(repo.path(), &["add", "payload.bin"])
        .stdout(full_output())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stderr,
        "✗ error: Could not write command output to stdout\n".as_bytes()
    );
    let listed = output_command(repo.path(), &["ls-files"]).output().unwrap();
    assert_ok(&listed, "ls-files after failed output");
    assert_eq!(
        listed.stdout,
        "✓ Tracked files: 1\n\n✓  payload.bin\n\n1 file(s)\n".as_bytes()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn stderr_failure_has_no_recursive_or_stdout_fallback() {
    let repo = init_repo();
    let outside = tempfile::tempdir().unwrap();
    for (dir, args) in [
        (outside.path(), vec!["status"]),
        (repo.path(), vec!["gc", "--remote", "does-not-exist"]),
    ] {
        let out = output_command(dir, &args)
            .stderr(full_output())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn broken_output_pipes_preserve_success_and_failure_exit_codes() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    let repo = init_repo();
    let outside = tempfile::tempdir().unwrap();
    for (dir, stderr, code) in [(repo.path(), false, 0), (outside.path(), true, 1)] {
        let (reader, writer) = UnixStream::pair().unwrap();
        drop(reader);
        let mut command = output_command(dir, &["status"]);
        let destination = std::process::Stdio::from(OwnedFd::from(writer));
        if stderr {
            command.stderr(destination);
        } else {
            command.stdout(destination);
        }
        let out = command.output().unwrap();
        assert_eq!(out.status.code(), Some(code));
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn help_output_failure_is_reported_and_usage_failure_keeps_exit_two() {
    let outside = tempfile::tempdir().unwrap();
    let out = output_command(outside.path(), &["--help"])
        .stdout(full_output())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stderr,
        "✗ error: Could not write command output to stdout\n".as_bytes()
    );
    let out = output_command(outside.path(), &["add"])
        .stderr(full_output())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn output_failure_does_not_retry_a_completed_move() {
    let repo = init_repo();
    std::fs::write(repo.path().join("before.bin"), b"payload").unwrap();
    assert_ok(&gat(repo.path(), &["add", "before.bin"]), "add before move");
    let out = output_command(repo.path(), &["mv", "before.bin", "after.bin"])
        .stderr(full_output())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    // Retrying would fail on the now-absent source. Only the output failure
    // from the completed move may reach the process boundary.
    assert!(out.stderr.is_empty());
    assert!(out.stdout.is_empty());
    assert!(!repo.path().join("before.bin").exists());
    assert_eq!(
        std::fs::read(repo.path().join("after.bin")).unwrap(),
        b"payload"
    );
    let listed = output_command(repo.path(), &["ls-files"]).output().unwrap();
    assert_ok(&listed, "ls-files after move");
    assert_eq!(
        listed.stdout,
        "✓ Tracked files: 1\n\n✓  after.bin\n\n1 file(s)\n".as_bytes()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn config_sequence_write_failure_uses_the_output_error_contract() {
    let repo = init_repo();
    let out = output_command(repo.path(), &["config", "cache.materialization_strategy"])
        .stdout(full_output())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stderr,
        "✗ error: Could not write command output to stdout\n".as_bytes()
    );
}

#[test]
fn resource_output_has_shared_style_and_separates_confirmations_from_results() {
    let repo = init_repo();
    let run = |args: &[&str]| {
        let out = output_command(repo.path(), args).output().unwrap();
        assert_ok(&out, "resource UI");
        assert_no_ansi_escape_bytes(&out.stdout);
        assert_no_ansi_escape_bytes(&out.stderr);
        out
    };
    let empty = run(&["selection", "list"]);
    assert_eq!(
        empty.stdout,
        "✓ Selections: none configured\n\nhint: Run `gat selection add <name> --path <path>`\n"
            .as_bytes()
    );
    let saved = run(&[
        "selection",
        "add",
        "runtime",
        "--path",
        "data files",
        "--include",
        "**/*, final.bin",
    ]);
    assert!(saved.stdout.is_empty());
    assert!(
        saved
            .stderr
            .ends_with("✓ Saved selection: runtime\n".as_bytes())
    );
    let list = run(&["selection", "list"]);
    assert_eq!(
        list.stdout,
        "✓ Selections: 1 configured\n\n✓  runtime  data files\n".as_bytes()
    );
    let show = run(&["selection", "show", "runtime"]);
    assert_eq!(show.stdout, "✓ Selection: runtime\n  Defined in: project\n\n  Default: no\n  Path:    data files\n  Include: **/*, final.bin\n".as_bytes());
    assert!(stderr(&show).starts_with("! Experimental:"));
    let removed = run(&["selection", "remove", "runtime"]);
    assert!(removed.stdout.is_empty());
    assert!(
        removed
            .stderr
            .ends_with("✓ Removed selection: runtime\n".as_bytes())
    );
    for args in [["remote", "default"], ["selection", "default"]] {
        let out = run(&args);
        assert!(out.stdout.starts_with("✓ Default".as_bytes()));
        assert!(out.stderr.is_empty() || stderr(&out).starts_with("! Experimental:"));
    }
}

#[test]
fn config_reads_show_values_and_their_effective_source() {
    let repo = init_repo();
    let read = |key| {
        let out = output_command(repo.path(), &["config", key])
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert_ok(&out, "config read");
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(
        read("sync.auto_fetch"),
        "✓ Config: sync.auto_fetch\n\n  false\n  Source: built-in default\n"
    );
    for args in [
        vec!["config", "sync.auto_fetch", "true"],
        vec!["config", "sync.auto_fetch", "false", "--local"],
        vec!["config", "git.ignore_patterns", "a,b", "data files"],
    ] {
        let out = output_command(repo.path(), &args).output().unwrap();
        assert_ok(&out, "config set");
    }
    assert_eq!(
        read("sync.auto_fetch"),
        "✓ Config: sync.auto_fetch\n\n  false\n  Source: local\n"
    );
    assert_eq!(
        read("git.ignore_patterns"),
        "✓ Config: git.ignore_patterns\n\n  a,b\n  data files\n  Source: project\n"
    );
    let out = output_command(
        repo.path(),
        &["config", "git.ignore_patterns", "--clear", "--local"],
    )
    .output()
    .unwrap();
    assert_ok(&out, "config clear");
    assert_eq!(
        read("git.ignore_patterns"),
        "✓ Config: git.ignore_patterns\n\n  (empty)\n  Source: local\n"
    );
    let cache = repo.path().join("override-cache");
    let out = output_command(repo.path(), &["config", "cache.location"])
        .env("NO_COLOR", "1")
        .env("GAT_CACHE_LOCATION", &cache)
        .output()
        .unwrap();
    assert_ok(&out, "config environment source");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(
            "✓ Config: cache.location\n\n  {}\n  Source: GAT_CACHE_LOCATION environment variable\n",
            cache.display()
        )
    );
}

#[test]
fn status_and_diff_append_effective_mount_ownership_to_existing_metadata() {
    let repo = init_repo();
    repo.write("README", "baseline");
    commit_all(repo.path(), "baseline");
    repo.write("vendor/a.bin", "mounted");
    repo.write("vendor-other/a.bin", "root owned");
    assert_ok(
        &gat(repo.path(), &["add", "vendor", "vendor-other"]),
        "track files",
    );
    repo.write(
        "gat.yaml",
        "mounts:\n  models:\n    url: ./source\n    target: vendor\n",
    );
    for command in ["status", "diff"] {
        for explicit in [false, true] {
            let mut args = vec![command];
            if explicit {
                args.extend(["--path", "."]);
            }
            let out = output_command(repo.path(), &args).output().unwrap();
            assert_ok(&out, "report mount ownership");
            let rendered = String::from_utf8(out.stdout).unwrap();
            let mounted = rendered
                .lines()
                .find(|line| line.contains("vendor/a.bin"))
                .unwrap();
            assert!(
                mounted.ends_with(if command == "status" {
                    "new, cached (mount models)"
                } else {
                    "new (mount models)"
                }),
                "{mounted}"
            );
            let root = rendered
                .lines()
                .find(|line| line.contains("vendor-other/a.bin"))
                .unwrap();
            assert!(!root.contains("(mount"));
        }
    }
}

#[test]
fn redirected_lists_are_bounded_and_full_output_is_global() {
    use unicode_width::UnicodeWidthStr;
    let tmp = init_repo();
    let dir = tmp.path();
    for index in 0..25 {
        let name = format!("{index:02}-{}.bin", "long-name-".repeat(12));
        std::fs::write(dir.join(name), b"payload").unwrap();
    }
    assert_ok(&gat(dir, &["add", "."]), "add files");
    let bounded = gat(dir, &["ls-files"]);
    assert_ok(&bounded, "bounded list");
    let text = String::from_utf8(bounded.stdout).unwrap();
    assert!(text.contains("(... 6 more rows)"), "{text}");
    assert_eq!(
        text.lines().filter(|line| line.starts_with("✓  ")).count(),
        19
    );
    assert!(text.lines().all(|line| line.width() <= 100));
    assert!(text.ends_with("25 file(s)\n"));
    for args in [
        ["--full-output", "ls-files"],
        ["ls-files", "--full-output"],
        ["-o", "ls-files"],
        ["ls-files", "-o"],
    ] {
        let full = gat(dir, &args);
        assert_ok(&full, "full list");
        let text = String::from_utf8(full.stdout).unwrap();
        assert!(!text.contains("more rows"));
        assert_eq!(
            text.lines().filter(|line| line.starts_with("✓  ")).count(),
            25
        );
        assert!(text.lines().any(|line| line.width() > 100));
    }
}

#[test]
fn incomplete_sync_has_one_completion_report_and_a_nonzero_exit() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("file.bin"), b"original").unwrap();
    assert_ok(&gat(dir, &["add", "file.bin"]), "add file");
    std::fs::write(dir.join("file.bin"), b"locally changed").unwrap();
    for args in [vec!["sync", "--dry-run"], vec!["sync", "--dry-run", "-o"]] {
        let result = gat(dir, &args);
        assert_failure_code(dir, &args, &result, 1);
        let text = stderr(&result);
        assert_eq!(text.matches("Sync incomplete").count(), 1, "{text}");
        assert!(text.contains("Conflicts: 1"));
        assert!(!text.contains("✗ error:"), "{text}");
        assert!(result.stdout.is_empty());
    }
    let result = gat(dir, &["hook", "post-checkout"]);
    assert_ok(&result, "hook tolerates unresolved paths");
    let text = stderr(&result);
    assert_eq!(text.matches("Sync incomplete").count(), 1, "{text}");
    assert!(text.contains("Conflicts: 1"), "{text}");
    assert!(!text.contains("Sync complete"), "{text}");
}
