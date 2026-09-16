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

/// Find the first complete pair of marker lines. Restarting at an unmatched
/// opening marker preserves malformed user text before an appended valid block.
fn marker_offsets(existing: &str, begin: &str, end: &str) -> Option<(usize, usize)> {
    if begin.is_empty() || end.is_empty() || begin == end {
        return None;
    }
    let mut start = None;
    let mut offset = 0;
    for line in existing.split_inclusive('\n') {
        let text = crate::newline::strip_terminator(line);
        if text == begin {
            start = Some(offset);
        } else if text == end
            && let Some(start) = start
        {
            return Some((start, offset));
        }
        offset += line.len();
    }
    None
}

/// Replaces a marked block or appends it using the file's detected line endings.
/// `body` uses LF terminators and includes its final newline when nonempty.
/// Markers must occupy complete lines. Unmatched markers and inline marker
/// text are preserved; when no complete pair exists, a new block is appended.
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
/// all other bytes. `None` means no complete pair of marker lines was present;
/// that case allocates nothing and requires no filesystem update.
#[must_use]
pub fn remove(existing: &str, begin: &str, end: &str) -> Option<String> {
    let (start, end_at) = marker_offsets(existing, begin, end)?;
    let before = &existing[..start];
    let after = strip_leading_terminator(&existing[end_at + end.len()..]);
    Some(format!("{before}{after}"))
}

/// Returns the body between a complete pair of marker lines, excluding one line
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
    fn inline_marker_text_is_preserved_and_does_not_hide_a_real_block() {
        for newline in ["\n", "\r\n"] {
            let user = format!("echo '{BEGIN}'{newline}keep this{newline}# example {END}{newline}");
            assert_eq!(extract_body(&user, BEGIN, END), None);
            assert_eq!(remove(&user, BEGIN, END), None);
            let installed = upsert(&user, BEGIN, END, "managed\n");
            assert!(installed.starts_with(&user));
            assert_eq!(remove(&installed, BEGIN, END), Some(user));
            assert_eq!(upsert(&installed, BEGIN, END, "managed\n"), installed);
        }
    }

    #[test]
    fn malformed_markers_are_preserved_without_repeated_appends() {
        for existing in [
            format!("{END}\nuser content\n{BEGIN}\n"),
            format!("{BEGIN}\nuser content\n"),
            format!("{END}\nuser content\n"),
            format!("{BEGIN} suffix\nuser content\n{END} suffix\n"),
        ] {
            assert_eq!(remove(&existing, BEGIN, END), None);
            let installed = upsert(&existing, BEGIN, END, "managed\n");
            assert_eq!(upsert(&installed, BEGIN, END, "managed\n"), installed);
            assert_eq!(remove(&installed, BEGIN, END), Some(existing));
        }
    }

    #[test]
    fn overlapping_markers_are_not_a_block() {
        let existing = "prefix abcde suffix";
        assert_eq!(extract_body(existing, "abcd", "cde"), None);
        assert_eq!(remove(existing, "abcd", "cde"), None);
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
        assert_eq!(extract_body("BEGIN\nEND", "BEGIN", "END"), Some(""));
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
            remove(&existing, BEGIN, END).as_deref(),
            Some("#!/bin/sh\nsome-other-hook\n")
        );
    }

    #[test]
    fn remove_is_a_no_op_without_markers() {
        assert_eq!(remove("#!/bin/sh\nfoo\n", BEGIN, END), None);
    }

    #[test]
    fn remove_leaves_nothing_when_block_was_the_only_content() {
        let existing = format!("{BEGIN}\ngat hook x\n{END}\n");
        assert_eq!(remove(&existing, BEGIN, END), Some(String::new()));
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
            remove(&existing, BEGIN, END).as_deref(),
            Some("#!/bin/sh\r\nsome-other-hook\r\n")
        );
    }

    #[test]
    fn remove_leaves_nothing_when_crlf_block_was_the_only_content() {
        let existing = format!("{BEGIN}\r\ngat hook x\r\n{END}\r\n");
        assert_eq!(remove(&existing, BEGIN, END), Some(String::new()));
    }
}
