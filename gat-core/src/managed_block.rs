//! Pure editing of marked blocks inside user-owned text files.

use crate::newline::Eol;

fn push_body_with_eol(out: &mut String, body: &str, eol: Eol) {
    match eol {
        Eol::Lf => out.push_str(body),
        Eol::Crlf => {
            let mut rest = body;
            while let Some(idx) = rest.find('\n') {
                out.push_str(&rest[..idx]);
                out.push_str("\r\n");
                rest = &rest[idx + 1..];
            }
            out.push_str(rest);
        }
    }
}

fn strip_leading_terminator(s: &str) -> &str {
    s.strip_prefix("\r\n")
        .unwrap_or_else(|| s.strip_prefix('\n').unwrap_or(s))
}

/// Locate the first marker pair only when its markers do not overlap.
fn marker_offsets(existing: &str, begin: &str, end: &str) -> Option<(usize, usize)> {
    let start = existing.find(begin)?;
    let end_at = existing.find(end)?;
    (end_at > start && end_at >= start + begin.len()).then_some((start, end_at))
}

/// Replaces a marked block or appends it using the file's detected line endings.
/// `body` uses LF terminators and includes its final newline when nonempty.
/// Missing, reversed, or overlapping markers leave the existing text intact
/// and cause a new block to be appended.
#[must_use]
pub fn upsert(existing: &str, begin: &str, end: &str, body: &str) -> String {
    let eol = Eol::detect(existing);
    let nl = eol.as_str();
    let (before, after, append) = match marker_offsets(existing, begin, end) {
        Some((start, end_at)) => (
            &existing[..start],
            strip_leading_terminator(&existing[end_at + end.len()..]),
            false,
        ),
        None => (existing, "", true),
    };
    // Render directly into the destination: exclude bodies can contain one
    // rule per tracked file, so a separate full block would duplicate them.
    let mut out = String::with_capacity(
        before.len() + body.len() + after.len() + begin.len() + end.len() + 3 * nl.len(),
    );
    out.push_str(before);
    if append && !before.is_empty() && !before.ends_with('\n') {
        out.push_str(nl);
    }
    out.push_str(begin);
    out.push_str(nl);
    push_body_with_eol(&mut out, body, eol);
    out.push_str(end);
    out.push_str(nl);
    out.push_str(after);
    out
}

/// Removes a marked block and its following line terminator, preserving
/// all other bytes. Missing, reversed, or overlapping markers are a no-op.
#[must_use]
pub fn remove(existing: &str, begin: &str, end: &str) -> String {
    match marker_offsets(existing, begin, end) {
        Some((start, end_at)) => {
            let before = &existing[..start];
            let after = &existing[end_at + end.len()..];
            let after = strip_leading_terminator(after);
            format!("{before}{after}")
        }
        _ => existing.to_string(),
    }
}

/// Returns the body between non-overlapping markers, excluding one line
/// terminator immediately after the opening marker.
#[must_use]
pub fn extract_body<'a>(existing: &'a str, begin: &str, end: &str) -> Option<&'a str> {
    let (start, end_at) = marker_offsets(existing, begin, end)?;
    Some(strip_leading_terminator(
        &existing[start + begin.len()..end_at],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEGIN: &str = "# >>> gat >>>";
    const END: &str = "# <<< gat <<<";

    #[test]
    fn overlapping_markers_are_not_a_block() {
        let existing = "prefix abcde suffix";
        assert_eq!(extract_body(existing, "abcd", "cde"), None);
        assert_eq!(remove(existing, "abcd", "cde"), existing);
        assert_eq!(
            upsert(existing, "abcd", "cde", "body\n"),
            "prefix abcde suffix\nabcd\nbody\ncde\n"
        );
    }

    #[test]
    fn extraction_strips_only_the_opening_line_terminator() {
        for newline in ["\n", "\r\n"] {
            let existing = format!("{BEGIN}{newline}body{newline}{END}{newline}");
            assert_eq!(
                extract_body(&existing, BEGIN, END),
                Some(format!("body{newline}").as_str())
            );
        }
        assert_eq!(extract_body("BEGINEND", "BEGIN", "END"), Some(""));
        assert_eq!(extract_body("END BEGIN", "BEGIN", "END"), None);
    }

    #[test]
    fn upsert_appends_when_no_markers_present() {
        let out = upsert("", BEGIN, END, "big.bin\n");
        assert_eq!(out, format!("{BEGIN}\nbig.bin\n{END}\n"));
    }

    #[test]
    fn upsert_preserves_content_around_markers() {
        let existing = format!("*.log\n{BEGIN}\nold.bin\n{END}\nkeep-me\n");
        let out = upsert(&existing, BEGIN, END, "new.bin\n");
        assert_eq!(out, format!("*.log\n{BEGIN}\nnew.bin\n{END}\nkeep-me\n"));
    }

    #[test]
    fn upsert_adds_newline_before_block_if_missing() {
        let out = upsert("*.log", BEGIN, END, "big.bin\n");
        assert_eq!(out, format!("*.log\n{BEGIN}\nbig.bin\n{END}\n"));
    }

    #[test]
    fn remove_drops_the_managed_block_only() {
        let existing = format!("#!/bin/sh\nsome-other-hook\n{BEGIN}\ngat hook x\n{END}\n");
        assert_eq!(
            remove(&existing, BEGIN, END),
            "#!/bin/sh\nsome-other-hook\n"
        );
    }

    #[test]
    fn remove_is_a_no_op_without_markers() {
        assert_eq!(remove("#!/bin/sh\nfoo\n", BEGIN, END), "#!/bin/sh\nfoo\n");
    }

    #[test]
    fn remove_leaves_nothing_when_block_was_the_only_content() {
        let existing = format!("{BEGIN}\ngat hook x\n{END}\n");
        assert_eq!(remove(&existing, BEGIN, END), "");
    }

    #[test]
    fn upsert_appends_using_crlf_when_existing_file_is_crlf() {
        let existing = "*.log\r\nkeep-me\r\n";
        let out = upsert(existing, BEGIN, END, "big.bin\n");
        assert_eq!(
            out,
            format!("*.log\r\nkeep-me\r\n{BEGIN}\r\nbig.bin\r\n{END}\r\n")
        );
    }

    #[test]
    fn upsert_replaces_in_place_preserving_crlf_around_the_block() {
        let existing = format!("*.log\r\n{BEGIN}\r\nold.bin\r\n{END}\r\nkeep-me\r\n");
        let out = upsert(&existing, BEGIN, END, "new.bin\n");
        assert_eq!(
            out,
            format!("*.log\r\n{BEGIN}\r\nnew.bin\r\n{END}\r\nkeep-me\r\n")
        );
    }

    #[test]
    fn upsert_on_crlf_file_without_trailing_newline_adds_matching_terminator() {
        let out = upsert("*.log\r\nno-final-newline", BEGIN, END, "big.bin\n");
        assert_eq!(
            out,
            format!("*.log\r\nno-final-newline\r\n{BEGIN}\r\nbig.bin\r\n{END}\r\n")
        );
    }

    #[test]
    fn upsert_never_introduces_a_bare_lf_line_into_a_crlf_file() {
        let existing = format!("*.log\r\n{BEGIN}\r\nold.bin\r\n{END}\r\nkeep-me\r\n");
        let out = upsert(&existing, BEGIN, END, "a.bin\nb.bin\n");
        for line in out.split("\r\n") {
            assert!(!line.contains('\n'), "line {line:?} has a bare LF");
        }
    }

    #[test]
    fn upsert_is_idempotent_on_a_crlf_file() {
        let once = upsert("*.log\r\n", BEGIN, END, "big.bin\n");
        assert_eq!(once, upsert(&once, BEGIN, END, "big.bin\n"));
    }

    #[test]
    fn remove_preserves_crlf_around_the_removed_block() {
        let existing =
            format!("#!/bin/sh\r\nsome-other-hook\r\n{BEGIN}\r\ngat hook x\r\n{END}\r\n");
        assert_eq!(
            remove(&existing, BEGIN, END),
            "#!/bin/sh\r\nsome-other-hook\r\n"
        );
    }

    #[test]
    fn remove_leaves_nothing_when_crlf_block_was_the_only_content() {
        let existing = format!("{BEGIN}\r\ngat hook x\r\n{END}\r\n");
        assert_eq!(remove(&existing, BEGIN, END), "");
    }
}
