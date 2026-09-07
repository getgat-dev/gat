//! Repository-wide newline policy primitives: a single, small place defines
//! what counts as a line terminator so every line-oriented reader/editor
//! applies the same `\r\n` handling.
//!
//! The policy is:
//! 1. Both `LF` (`\n`) and `CRLF` (`\r\n`) are accepted line terminators
//!    wherever line-oriented text is a supported input.
//! 2. A `CR` is only ever treated as part of a terminator when it
//!    immediately precedes the terminating `LF`; a bare `CR` elsewhere is
//!    data and must never be silently trimmed.
//! 3. Gat-owned canonical formats (e.g. `gat.lock`) are always written with
//!    `LF` only; CRLF is accepted on read but never produced on write.
//! 4. Editing a file gat doesn't own outright (Git hooks, `.git/info/
//!    exclude`, `.git/info/attributes`, ...) must preserve that file's
//!    existing newline convention rather than normalizing it.
//!
//! These helpers are intentionally allocation-free and streaming-friendly
//! (they operate on borrowed `&str`/byte slices) so adopting them doesn't
//! reintroduce whole-document buffering on bounded-memory paths.

/// Strip exactly one trailing line terminator from `line`, if present:
/// a trailing `"\r\n"` becomes `""`-suffixed (both bytes removed), a
/// trailing `"\n"` alone becomes `""`-suffixed (one byte removed), and a
/// bare trailing `"\r"` (with no following `"\n"`) is left untouched,
/// since it is not a `CRLF` terminator and remains meaningful data.
#[must_use]
pub fn strip_terminator(line: &str) -> &str {
    match line.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => line,
    }
}

/// The line-ending convention already used by a shared/user-owned file, so
/// an editor (e.g. [`crate::managed_block`]) can preserve it rather than
/// normalizing the whole file to gat's own canonical style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    /// `"\n"` only.
    Lf,
    /// `"\r\n"`.
    Crlf,
}

impl Eol {
    /// The literal terminator string for this style.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }

    /// Detects the convention already used by `existing`: `Crlf` if the
    /// first `"\n"` found is immediately preceded by `"\r"`, `Lf`
    /// otherwise (including when `existing` has no newline at all -- a
    /// new/empty file defaults to gat's own canonical `Lf` style).
    #[must_use]
    pub fn detect(existing: &str) -> Self {
        match existing.find('\n') {
            Some(idx) if existing.as_bytes()[..idx].last() == Some(&b'\r') => Self::Crlf,
            _ => Self::Lf,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_terminator_removes_lf() {
        assert_eq!(strip_terminator("foo\n"), "foo");
    }

    #[test]
    fn strip_terminator_removes_crlf() {
        assert_eq!(strip_terminator("foo\r\n"), "foo");
    }

    #[test]
    fn strip_terminator_leaves_bare_cr_untouched() {
        assert_eq!(strip_terminator("foo\r"), "foo\r");
    }

    #[test]
    fn strip_terminator_leaves_untermianted_line_untouched() {
        assert_eq!(strip_terminator("foo"), "foo");
    }

    #[test]
    fn strip_terminator_leaves_embedded_cr_untouched() {
        assert_eq!(strip_terminator("fo\ro\n"), "fo\ro");
    }

    #[test]
    fn eol_detect_lf() {
        assert_eq!(Eol::detect("a\nb\n"), Eol::Lf);
    }

    #[test]
    fn eol_detect_crlf() {
        assert_eq!(Eol::detect("a\r\nb\r\n"), Eol::Crlf);
    }

    #[test]
    fn eol_detect_defaults_to_lf_without_newline() {
        assert_eq!(Eol::detect("no newline here"), Eol::Lf);
        assert_eq!(Eol::detect(""), Eol::Lf);
    }
}
