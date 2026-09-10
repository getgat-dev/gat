//! Renders a [`crate::error::Diagnostic`] as a `✗ error: ...` block on
//! stderr -- the *only* place in the crate that turns an application
//! failure into user-visible text. This module intentionally
//! has no function that accepts `&dyn std::error::Error`,
//! `&anyhow::Error`, or a whole [`crate::error::Failure`]: it only ever
//! sees a `&Diagnostic`, which by construction cannot carry a hidden
//! technical source, so there is no way for third-party/OS/parser error
//! text to reach this renderer even by mistake.

use super::flow;
use crate::error::Diagnostic;
use crate::output::terminal::{self, ERROR};
use crate::output::{Output, Stream, WriteFailure};
use crate::presentation::UserLine;

/// Renders a diagnostic to borrowed stderr: a wrapped summary, optional detail,
/// and hints. Stops on the first write failure without rendering another error.
pub fn render(output: &mut Output<'_>, diagnostic: &Diagnostic) -> Result<(), WriteFailure> {
    let width = output.prose_width(Stream::Stderr);
    let summary = summary_with_subject(diagnostic.summary_line(), diagnostic.subject_line());
    for (index, line) in flow::lines("✗ error: ", "         ", &summary, width).enumerate() {
        if index == 0 {
            output.stderr(format_args!(
                "{} {}{}{}",
                terminal::red(ERROR),
                terminal::bold_red("error:"),
                if line.body.is_empty() { "" } else { " " },
                line.body
            ))?;
        } else {
            output.stderr(format_args!("{line}"))?;
        }
    }

    if !diagnostic.detail_lines().is_empty() {
        terminal::section(output, Stream::Stderr)?;
        for detail in diagnostic.detail_lines() {
            terminal::paragraph(
                output,
                Stream::Stderr,
                detail,
                2,
                terminal::Emphasis::Normal,
            )?;
        }
    }

    terminal::hints(output, Stream::Stderr, diagnostic.hint_lines())?;

    Ok(())
}

/// Compose before placement: subjects stay atomic but can move to the next line.
fn summary_with_subject(summary: &UserLine, subject: Option<&UserLine>) -> UserLine {
    match subject {
        Some(subject) => UserLine::compose([
            summary.clone(),
            UserLine::authored(" `"),
            subject.clone(),
            UserLine::authored("`"),
        ]),
        None => summary.clone(),
    }
}

#[cfg(test)]
fn render_subject_aware(summary: &'static str, subject: Option<&str>, width: usize) -> String {
    let text = summary_with_subject(
        &UserLine::authored(summary),
        subject.map(UserLine::identifier).as_ref(),
    );
    let wrapped = terminal::wrap_user_line("✗ error: ", &text, width);
    wrapped
        .strip_prefix("✗ error: ")
        .unwrap_or(&wrapped)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ErrorCode, Failure};
    use std::fmt;

    fn rendered_lines(diagnostic: &Diagnostic) -> Vec<String> {
        let mut stderr = Vec::new();
        render(
            &mut Output::new(
                &mut Vec::new(),
                &mut anstream::StripStream::new(&mut stderr),
            ),
            diagnostic,
        )
        .unwrap();
        let plain = std::str::from_utf8(&stderr).unwrap();
        plain
            .strip_prefix("✗ ")
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn subject_moves_to_a_continuation_line_before_exceeding_width() {
        let diagnostic = Diagnostic::new_for_test(ErrorCode::Internal, "Read failed")
            .with_subject_for_test("data/file.bin");
        let mut bytes = Vec::new();
        let mut stdout = Vec::new();
        let mut output = Output::new(&mut stdout, &mut bytes);
        output.set_layouts(
            crate::output::OutputLayout::default(),
            crate::output::OutputLayout::bounded(28),
        );
        render(&mut output, &diagnostic).unwrap();
        assert_eq!(
            crate::output::strip_ansi(&String::from_utf8(bytes).unwrap()),
            "✗ error: Read failed\n         `data/file.bin`\n"
        );
    }

    #[test]
    fn summary_wrapping_accounts_for_the_prefix_and_preserves_the_subject() {
        use unicode_width::UnicodeWidthStr;
        let body = render_subject_aware(
            "The tracked files need attention before continuing",
            None,
            24,
        );
        let complete = format!("✗ error: {body}");
        assert!(complete.lines().count() > 1);
        assert!(complete.lines().all(|line| line.width() <= 24));
        assert!(
            complete
                .lines()
                .skip(1)
                .all(|line| line.starts_with("         "))
        );
        let body = render_subject_aware(
            "Path is outside the repository",
            Some("a path with spaces"),
            24,
        );
        assert!(body.ends_with("`a path with spaces`"));
    }

    #[test]
    fn summary_only() {
        let diagnostic = Diagnostic::new_for_test(ErrorCode::NotRepository, "Not a Gat repository");
        let lines = rendered_lines(&diagnostic);
        assert_eq!(lines, vec!["error: Not a Gat repository".to_string()]);
    }

    #[test]
    fn summary_and_details() {
        let diagnostic = Diagnostic::new_for_test(ErrorCode::InvalidConfig, "gat.yaml is invalid")
            .with_detail_for_test("The `mounts` section has an unrecognized key.");
        let lines = rendered_lines(&diagnostic);
        assert_eq!(
            lines,
            vec![
                "error: gat.yaml is invalid".to_string(),
                String::new(),
                "  The `mounts` section has an unrecognized key.".to_string(),
            ]
        );
    }

    #[test]
    fn logical_details_and_identifiers_remain_intact() {
        let path = format!("original  files/{}.bin", "long path ".repeat(15));
        let diagnostic = Diagnostic::new_for_test(ErrorCode::Conflict, "Move rollback failed")
            .with_detail_for_test("The moved file was not restored.")
            .with_detail_for_test(path.clone())
            .with_hint_for_test(path.clone());
        assert_eq!(
            rendered_lines(&diagnostic),
            vec![
                "error: Move rollback failed".to_owned(),
                String::new(),
                "  The moved file was not restored.".to_owned(),
                format!("  {path}"),
                String::new(),
                format!("hint: {path}"),
            ]
        );
    }

    #[test]
    fn summary_and_one_hint() {
        let diagnostic = Diagnostic::new_for_test(ErrorCode::RemoteNotFound, "Remote not found")
            .with_hint_for_test("Run `gat remote add origin <url>` to configure it.");
        let lines = rendered_lines(&diagnostic);
        assert_eq!(
            lines,
            vec![
                "error: Remote not found".to_string(),
                String::new(),
                "hint: Run `gat remote add origin <url>` to configure it.".to_string(),
            ]
        );
    }

    #[test]
    fn detail_is_separated_from_the_hint_block() {
        let diagnostic =
            Diagnostic::new_for_test(ErrorCode::InvalidConfig, "Invalid configuration")
                .with_detail_for_test("The requested scope differs.")
                .with_hint_for_test("Update the defining scope.");
        assert_eq!(
            rendered_lines(&diagnostic),
            vec![
                "error: Invalid configuration",
                "",
                "  The requested scope differs.",
                "",
                "hint: Update the defining scope.",
            ]
        );
    }

    #[test]
    fn summary_and_multiple_hints() {
        let diagnostic =
            Diagnostic::new_for_test(ErrorCode::Conflict, "Sync blocked by a conflict")
                .with_hint_for_test("Resolve the conflict in the working tree.")
                .with_hint_for_test("Then run `gat sync` again.");
        let lines = rendered_lines(&diagnostic);
        assert_eq!(
            lines,
            vec![
                "error: Sync blocked by a conflict".to_string(),
                String::new(),
                "hint: Resolve the conflict in the working tree.".to_string(),
                "hint: Then run `gat sync` again.".to_string(),
            ]
        );
    }

    #[test]
    fn wraps_long_summary_at_given_width() {
        let long = "this summary is deliberately long enough that it must wrap across more than one line at a narrow width";
        let wrapped = terminal::wrap_user_line("", &UserLine::authored(long), 20);
        for line in wrapped.lines() {
            assert!(line.len() <= 20, "line too long: {line:?}");
        }
        assert_eq!(
            wrapped.split_whitespace().collect::<Vec<_>>(),
            long.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn long_path_subject_is_not_word_wrapped() {
        let long_path =
            "/very/deeply/nested/repository/path/that/would/otherwise/be/split/mid-wrap/file.bin";
        let diagnostic = Diagnostic::new_for_test(
            ErrorCode::PathOutsideRepository,
            "Path is outside the repository",
        )
        .with_subject_for_test(long_path);
        let rendered = terminal::wrap_user_line(
            "✗ error: ",
            &summary_with_subject(diagnostic.summary_line(), diagnostic.subject_line()),
            20,
        );
        // The subject appears intact, not split across a wrap boundary.
        assert!(rendered.contains(long_path));
    }

    #[derive(Debug)]
    struct ScaryTechnicalError(&'static str);

    impl fmt::Display for ScaryTechnicalError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for ScaryTechnicalError {}

    #[test]
    fn diagnostic_with_no_hidden_source_renders_only_its_own_text() {
        let failure = Failure::expected_for_test(Diagnostic::new_for_test(
            ErrorCode::NotRepository,
            "Not a Gat repository",
        ));
        let lines = rendered_lines(failure.diagnostic());
        assert_eq!(lines, vec!["error: Not a Gat repository".to_string()]);
    }

    #[test]
    fn rendering_never_surfaces_a_hidden_scary_technical_source() {
        const SCARY: &str = "SqliteFailure(Error { code: DatabaseCorrupt }, Some(\"database disk image is malformed\"))";
        let failure = Failure::infrastructure_for_test(
            Diagnostic::new_for_test(ErrorCode::StateCorrupt, "Can't read Gat's local state")
                .with_hint_for_test("Run `gat system repair` to rebuild it."),
            ScaryTechnicalError(SCARY),
        );
        let lines = rendered_lines(failure.diagnostic());
        let rendered = lines.join("\n");
        assert!(!rendered.contains(SCARY));
        assert!(!rendered.contains("SqliteFailure"));
        assert!(!rendered.contains("DatabaseCorrupt"));
    }
}
