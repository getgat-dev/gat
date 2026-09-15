//! The dedicated presentation layer: [`UserLine`] is the single approved
//! single-line, user-visible text type, and this module is the only
//! place it is defined. `error` (`Diagnostic`/`Failure`/`UserProblem`)
//! and `output` (rows, messages, progress wording, notices) both build
//! their user-facing text out of `UserLine`, but neither owns the type
//! itself -- keeping it in its own module makes the ownership split
//! explicit: `error` classifies *what kind* of failure occurred,
//! `output` decides *how* to present outcomes/progress/notices, and
//! `presentation` alone decides what counts as safe, approved,
//! single-line text.
//!
//! ## The two output classes
//!
//! Not every value gat prints is human presentation, and `UserLine`
//! deliberately does not try to cover both classes:
//!
//! - **Human presentation** -- headings, list rows, messages, progress
//!   wording, notices: anything a person reads as prose or as a styled
//!   terminal row. This class always goes through `UserLine`; there is
//!   no raw `format!`/`println!` escape hatch for it (`output::terminal`,
//!   `output::render`, `output::progress`).
//! - **Structured/machine-readable command output** must come from a
//!   specific validated or redacted domain representation, never an
//!   arbitrary error-derived string. This is distinct from human
//!   presentation: `gat config` reads are human-readable reports with
//!   values and a dim source line, and therefore use `UserLine` too.

use std::fmt;
use std::path::Path;

/// Escapes every ASCII control character (anything below `0x20`, plus
/// `DEL`) in `text`, replacing it with its Rust `\u{..}`/`\r`/`\n`/
/// `\t`-style escape sequence so it can never reach a terminal as a raw
/// control byte -- including `\n`, so only the renderer can introduce physical line breaks into an approved
/// logical line.
fn sanitize_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.push_str(&c.escape_default().collect::<String>());
        } else {
            out.push(c);
        }
    }
    out
}

/// The single approved user-visible presentation type. `UserLine`
/// means: intentionally approved for user-visible output. Terminal
/// sanitization (escaping every control character, including `\n`, `\r`,
/// `ESC`, `BEL`, backspace, `NUL`, `DEL`, ... to its printable Rust
/// `\u{..}`-style form) happens exactly once, inside `UserLine`'s own
/// constructors, so a `UserLine` can never inject a raw newline, forge a
/// fake `hint:`/`error:` line, or emit an ANSI control sequence when
/// interpolated into rendered output. `UserLine` is used for every
/// semantic role a `Diagnostic`/`Message`/`ListRow` needs to express --
/// summary, subject, hint, row label, title, footer, message text --
/// with the field or container itself expressing which role a given
/// `UserLine` plays, rather than a family of narrowly named wrapper
/// types.
///
/// The conversion graph is closed by construction: there is no
/// `From<String>`, no blanket `From<T: Into<String>>`, and no
/// `From<dyn Error>`/generic `Display` conversion, so an error's
/// `Display`, a `format!`-composed `String`, or any other arbitrary text
/// cannot become a `UserLine` by accident. The only ways to build one
/// are:
/// - [`UserLine::authored`] -- a `&'static str` literal baked into the
///   binary by a developer (the ergonomic common case, safe because
///   `String` does not implement `Into<UserLine>`);
/// - `UserLine::compose` -- an explicit, closed set of safe fragments
///   (static text, a sanitized identifier, a bounded number);
/// - the narrow, domain-specific dynamic-identity constructors below
///   ([`UserLine::path`], `UserLine::path_text`,
///   `UserLine::identifier`, `UserLine::config_key`,
///   `UserLine::oid`, `UserLine::redacted_url`), each of which
///   documents exactly what kind of value it approves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserLine(LineStorage);

// Only mixed prose/identity compositions need a separate allocation for ranges.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LineStorage {
    Prose(std::borrow::Cow<'static, str>),
    Identity(std::borrow::Cow<'static, str>),
    Mixed(Box<LineContent>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LineContent {
    text: String,
    protected: Vec<std::ops::Range<usize>>,
}

impl UserLine {
    /// Sanitizes a dynamic value and protects it from prose wrapping.
    /// Authored prose is sanitized separately and allows whitespace breaks.
    fn raw(text: impl AsRef<str>) -> Self {
        Self(LineStorage::Identity(sanitize_line(text.as_ref()).into()))
    }

    /// Approved static product prose; borrows literals that need no sanitization.
    #[must_use]
    pub fn authored(text: &'static str) -> Self {
        let text = if text.chars().any(char::is_control) {
            std::borrow::Cow::Owned(sanitize_line(text))
        } else {
            std::borrow::Cow::Borrowed(text)
        };
        Self(LineStorage::Prose(text))
    }

    /// Keep already-approved fragments together during prose wrapping, for
    /// example a command assembled from a static verb and a dynamic name.
    /// This changes layout metadata only; it cannot approve new text.
    #[must_use]
    pub(crate) fn unbroken(self) -> Self {
        let text = match self.0 {
            LineStorage::Prose(text) | LineStorage::Identity(text) => text,
            LineStorage::Mixed(content) => content.text.into(),
        };
        Self(LineStorage::Identity(text))
    }

    /// Composes approved prose by concatenating already-approved
    /// [`UserLine`] values. Composition itself performs no semantic
    /// approval -- every element must already have crossed an approval
    /// boundary ([`UserLine::authored`], `UserLine::identifier`,
    /// [`UserLine::number`], ...) before it can appear here, so there is
    /// deliberately no way to hand this function an arbitrary
    /// `Display`/`String`. Crate-visible (not `pub(in crate::presentation)`):
    /// both `error::map` (mapper authors composing dynamic diagnostics)
    /// and `output::render` (composing outcome-summary prose from
    /// already safe, typed domain fields -- never a raw technical
    /// error) are legitimate, trusted callers.
    pub(crate) fn compose(parts: impl IntoIterator<Item = Self>) -> Self {
        let mut out = String::new();
        let mut protected = Vec::new();
        for part in parts {
            let offset = out.len();
            protected.extend(
                part.protected_ranges()
                    .map(|range| range.start + offset..range.end + offset),
            );
            out.push_str(part.as_str());
        }
        match protected.as_slice() {
            [] => Self(LineStorage::Prose(out.into())),
            [range] if range.start == 0 && range.end == out.len() => {
                Self(LineStorage::Identity(out.into()))
            }
            _ => Self(LineStorage::Mixed(Box::new(LineContent {
                text: out,
                protected,
            }))),
        }
    }

    /// Join approved values without erasing their individual wrapping boundaries.
    pub(crate) fn join(parts: impl IntoIterator<Item = Self>, separator: &'static str) -> Self {
        Self::compose(parts.into_iter().enumerate().flat_map(|(index, part)| {
            [
                Self::authored(if index == 0 { "" } else { separator }),
                part,
            ]
        }))
    }

    /// Convenience wrapper around `UserLine::compose` for the common
    /// "prefix, one approved dynamic value, suffix" shape, so callers
    /// don't need to spell a three-element array for the routine case.
    pub(crate) fn with_identifier(prefix: &'static str, value: &str, suffix: &'static str) -> Self {
        Self::compose([
            Self::authored(prefix),
            Self::identifier(value),
            Self::authored(suffix),
        ])
    }

    /// A filesystem path (repo-relative or absolute) that a diagnostic
    /// is about. The only dynamic-identity `UserLine` constructor
    /// callable from outside `crate::error::map`: paths are handed
    /// around widely enough (worktree/config/lock/atomic mappers,
    /// `commands`) that restricting it to the mapping layer alone isn't
    /// practical, and a `&Path` is already a structured, non-`String`
    /// domain value.
    #[must_use]
    pub fn path(path: &Path) -> Self {
        Self::raw(path.display().to_string())
    }

    /// A path-shaped value that is currently represented as `String`
    /// rather than `Path`/`PathBuf` in its subsystem error type (e.g. a
    /// repo-relative row path parsed out of `gat.lock`, or an object
    /// storage key). This constructor is separate from
    /// `UserLine::identifier` so a path is never conflated with an
    /// ordinary short domain name or label.
    pub(crate) fn path_text(path: &str) -> Self {
        Self::raw(path)
    }

    /// A canonical, root-relative Gat tracked path
    /// ([`gat_core::lexical_path::GatPath`]). The typed counterpart of
    /// `UserLine::path_text` for call sites that already hold a
    /// validated `GatPath` rather than a bare `String`/`&str` -- so a
    /// canonical tracked path can be rendered without a caller needing to
    /// downcast it to `&str` first just to satisfy this constructor.
    pub(crate) fn gat_path(path: &gat_core::lexical_path::GatPath) -> Self {
        Self::raw(path.as_str())
    }

    /// A canonical Gat subpath ([`gat_core::lexical_path::GatSubpath`]),
    /// rendering [`gat_core::lexical_path::GatSubpath::Root`] as `.`
    /// and a non-root subpath as its canonical path text -- the typed
    /// counterpart of [`UserLine::gat_path`] for call sites that already
    /// hold a `GatSubpath` rather than a non-root `GatPath`, so a mount's
    /// source subpath can be rendered without first flattening it through
    /// `Display`/`to_string()`.
    pub(crate) fn gat_subpath(path: &gat_core::lexical_path::GatSubpath) -> Self {
        match path.as_path() {
            Some(path) => Self::raw(path.as_str()),
            None => Self::raw("."),
        }
    }

    /// A remote/mount/route name, revision, or other short non-path
    /// domain identifier that a diagnostic or output row is about.
    /// Crate-visible (not `pub`): only code within this crate (mapper
    /// authors, `commands`, `output::render`) can construct one --
    /// nothing outside the crate can turn arbitrary text into a
    /// `UserLine` through this constructor.
    pub(crate) fn identifier(value: &str) -> Self {
        Self::raw(value)
    }

    /// A config key that a diagnostic is about. Crate-visible for the
    /// same reason as `UserLine::identifier`; only
    /// config error mapping calls it in practice.
    pub(crate) fn config_key(value: &str) -> Self {
        Self::raw(value)
    }

    /// An object id (or object-id-shaped prefix) that a diagnostic or
    /// output row is about. Crate-visible (like `UserLine::identifier`)
    /// so `commands`/`output::render` can label rows by oid without
    /// misclassifying it as an ordinary domain identifier.
    pub(crate) fn oid(value: &str) -> Self {
        Self::raw(value)
    }

    /// The typed counterpart of `UserLine::oid` for call sites that
    /// already hold a native [`gat_core::oid::Oid`] -- so a canonical
    /// object id can be rendered without a caller needing to hex-encode
    /// it first just to satisfy this constructor.
    pub(crate) fn oid_value(oid: &gat_core::oid::Oid) -> Self {
        Self::raw(oid.to_hex())
    }

    /// A bounded numeric value (a count, a version, a byte size, ...)
    /// rendered in decimal. Crate-visible for the same reason as
    /// `UserLine::identifier`: numbers are never a leak vector on
    /// their own, but keeping construction explicit avoids an implicit
    /// `impl From<i64>`/`Display` conversion path.
    pub(crate) fn number(value: i64) -> Self {
        Self::raw(value.to_string())
    }

    /// A remote URL that a diagnostic is about. Takes a
    /// `crate::redaction::RedactedUrl` rather than a raw `&str`/
    /// `String` -- the type itself is the proof that userinfo/query
    /// secrets have already been stripped, so it is not possible to
    /// construct a URL subject that skips redaction.
    pub(crate) fn redacted_url(url: &crate::redaction::RedactedUrl) -> Self {
        Self::raw(url.as_str())
    }

    #[must_use]
    pub const fn as_str(&self) -> &str {
        match &self.0 {
            LineStorage::Prose(std::borrow::Cow::Borrowed(text))
            | LineStorage::Identity(std::borrow::Cow::Borrowed(text)) => text,
            LineStorage::Prose(std::borrow::Cow::Owned(text))
            | LineStorage::Identity(std::borrow::Cow::Owned(text)) => text.as_str(),
            LineStorage::Mixed(content) => content.text.as_str(),
        }
    }

    fn protected_ranges(&self) -> impl Iterator<Item = std::ops::Range<usize>> {
        let (whole, ranges) = match &self.0 {
            LineStorage::Prose(_) => (None, &[][..]),
            LineStorage::Identity(text) => ((!text.is_empty()).then_some(0..text.len()), &[][..]),
            LineStorage::Mixed(content) => (None, content.protected.as_slice()),
        };
        whole.into_iter().chain(ranges.iter().cloned())
    }

    /// Prose words suitable for wrapping, keeping dynamic identities intact,
    /// including their internal spaces and adjacent punctuation.
    pub(crate) fn wrapping_words(&self) -> impl Iterator<Item = &str> {
        let mut start = 0;
        let mut chars = self.as_str().char_indices();
        let mut protected = self.protected_ranges().peekable();
        std::iter::from_fn(move || {
            for (index, ch) in chars.by_ref() {
                while protected.peek().is_some_and(|range| range.end <= index) {
                    protected.next();
                }
                if ch.is_whitespace()
                    && !protected.peek().is_some_and(|range| range.contains(&index))
                {
                    let word = &self.as_str()[start..index];
                    start = index + ch.len_utf8();
                    if !word.is_empty() {
                        return Some(word);
                    }
                }
            }
            let word = &self.as_str()[start..];
            start = self.as_str().len();
            (!word.is_empty()).then_some(word)
        })
    }

    /// Test-only equivalent of `UserLine::compose`/`UserLine::identifier`
    /// for modules outside `crate::error`/`crate::error::map` (test
    /// fixtures, the renderer's own smoke tests, `app`'s policy tests)
    /// that need a throwaway dynamic `UserLine` without going through
    /// the closed fragment/identity API.
    #[cfg(test)]
    pub(crate) fn identifier_for_test(value: impl Into<String>) -> Self {
        Self::raw(value.into())
    }
}

impl fmt::Display for UserLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Allows the common case (`Diagnostic::new(code, "...")`,
/// `.with_hint("...")`, `UserProblem::new("...")`, ...) to take an
/// authored literal directly instead of requiring
/// `UserLine::authored("...")` at every call site. This remains safe only
/// because `String` does not implement `Into<UserLine>` -- there is no
/// corresponding `impl From<String> for UserLine` or blanket
/// `impl<T: Into<String>> From<T> for UserLine`, so `err.to_string()`
/// still does not satisfy `impl Into<UserLine>` and remains a compile
/// error at every call site that accepts one.
impl From<&'static str> for UserLine {
    fn from(text: &'static str) -> Self {
        Self::authored(text)
    }
}

/// A canonical [`gat_core::lexical_path::GatPath`] is already a
/// validated domain value (never arbitrary user-supplied text), so
/// letting it convert into a [`UserLine`] directly is as safe as the
/// `&'static str` impl above -- unlike a blanket `From<String>`, this
/// can't be satisfied by an unapproved runtime string.
impl From<&gat_core::lexical_path::GatPath> for UserLine {
    fn from(path: &gat_core::lexical_path::GatPath) -> Self {
        Self::gat_path(path)
    }
}

/// A native [`gat_core::oid::Oid`] is already a validated, fixed-size
/// domain value (never arbitrary user-supplied text), so letting it
/// convert into a [`UserLine`] directly is as safe as the `GatPath` impl
/// above.
impl From<&gat_core::oid::Oid> for UserLine {
    fn from(oid: &gat_core::oid::Oid) -> Self {
        Self::oid_value(oid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbroken_compositions_preserve_approval_and_identity_whitespace() {
        assert!(matches!(
            UserLine::authored("gat sync").unbroken().0,
            LineStorage::Identity(std::borrow::Cow::Borrowed(_))
        ));
        let command = UserLine::compose([
            UserLine::authored("gat route remove "),
            UserLine::identifier("  name with spaces\n\u{1b}  "),
        ]);
        let approved = command.as_str().to_owned();
        let command = command.unbroken();
        assert_eq!(command.as_str(), approved);
        assert_eq!(
            command.wrapping_words().collect::<Vec<_>>(),
            vec![approved.as_str()]
        );
        assert!(!command.as_str().contains('\n'));
        assert!(!command.as_str().contains('\u{1b}'));
    }

    #[test]
    fn plain_prose_borrows_and_identities_remain_indivisible() {
        let prose = "file names with spaces";
        let line = UserLine::authored(prose);
        assert_eq!(line.as_str().as_ptr(), prose.as_ptr());
        assert_eq!(
            line.wrapping_words().collect::<Vec<_>>(),
            ["file", "names", "with", "spaces"]
        );
        let identity = UserLine::path_text(prose);
        assert_eq!(identity.wrapping_words().collect::<Vec<_>>(), [prose]);
        let composed =
            UserLine::compose([UserLine::authored(""), identity, UserLine::authored("")]);
        assert_eq!(composed.wrapping_words().collect::<Vec<_>>(), [prose]);
        assert_eq!(UserLine::authored("a\nb").as_str(), "a\\nb");
    }

    #[test]
    fn wrapping_words_preserve_nested_identities_and_skip_prose_whitespace() {
        let text = UserLine::compose([
            UserLine::authored("  paths  "),
            UserLine::compose([
                UserLine::path_text("日本語  files/a.bin"),
                UserLine::authored(" and "),
                UserLine::identifier("my mount"),
            ]),
            UserLine::authored("  done  "),
        ]);
        let mut words = text.wrapping_words();
        assert_eq!(
            words.by_ref().collect::<Vec<_>>(),
            ["paths", "日本語  files/a.bin", "and", "my mount", "done"]
        );
        assert_eq!(words.next(), None);
        assert_eq!(words.next(), None);
        assert_eq!(UserLine::authored("   ").wrapping_words().next(), None);
    }

    #[test]
    fn user_message_authored_accepts_static_text() {
        let message = UserLine::authored("Could not read the configuration");
        assert_eq!(message.as_str(), "Could not read the configuration");
    }

    #[test]
    fn user_message_still_escapes_embedded_control_characters() {
        // `UserLine` adds provenance (was this ever meant to face a
        // user?), it does not relax `UserLine`'s terminal-safety
        // guarantee -- a composed fragment carrying a raw control
        // character must still come out escaped.
        let message = UserLine::compose([
            UserLine::authored("repository "),
            UserLine::identifier_for_test("evil\x1b[31m\ninjected"),
        ]);
        assert!(!message.as_str().contains('\x1b'));
        assert!(!message.as_str().contains('\n'));
    }

    #[test]
    fn user_message_compose_embeds_dynamic_identifiers_and_numbers_explicitly() {
        let message = UserLine::compose([
            UserLine::authored("mount "),
            UserLine::identifier_for_test("cache"),
            UserLine::authored(" already has "),
            UserLine::number(3),
            UserLine::authored(" conflicting routes"),
        ]);
        assert_eq!(
            message.as_str(),
            "mount cache already has 3 conflicting routes"
        );
    }

    #[test]
    fn user_message_construction_never_requires_a_raw_technical_source() {
        // Every `UserLine` constructor (`authored`, `compose`) takes
        // only `&'static str`/`UserLine`/`i64` fragments -- there is no
        // constructor overload that accepts a `dyn Error`, a `Display`
        // value, or an owned `String`, so a technical source is never
        // needed (or accepted) to build one. This test exists as a
        // This assertion guards the constructor's source-free contract.
        let message = UserLine::authored("static, no source involved");
        assert_eq!(message.as_str(), "static, no source involved");
    }

    #[test]
    fn safe_line_containing_embedded_newlines_stays_exactly_one_line() {
        let line = UserLine::identifier_for_test("first\nsecond\nthird");
        assert!(!line.as_str().contains('\n'));
        assert!(line.as_str().contains("\\n"));
    }

    #[test]
    fn safe_line_escapes_every_control_character_at_construction() {
        let line = UserLine::identifier_for_test("esc\x1bcr\rtab\tbel\x07del\x7f");
        let rendered = line.as_str();
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\r'));
        assert!(!rendered.contains('\t'));
        assert!(!rendered.contains('\x07'));
        assert!(!rendered.contains('\x7f'));
    }
}
