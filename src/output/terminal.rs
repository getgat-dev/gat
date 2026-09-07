//! Small terminal UX helpers: colors, symbols, wrapping, error diagnostics
//! -- the one place gat's command output styling lives, so every command
//! reads the same way instead of each inventing its own. Transient
//! progress (spinners/bars) lives separately, under `output::progress`;
//! commands never construct indicatif types directly.
//!
//! The process adapter supplies `anstream`'s auto-detecting streams, so colors are
//! stripped automatically when stdout/stderr isn't a tty, when `NO_COLOR` is
//! set, or on dumb terminals -- no manual tty checks needed here. Only
//! named ANSI colors are used (never raw RGB/gray), so contrast stays sane
//! on light and dark themes alike. Every colored line also carries a plain
//! symbol/word that reads fine with color stripped -- never rely on color
//! alone.
//!
//! Palette: bold for headings/labels, cyan for flags/commands/paths, green
//! for success, yellow for warnings, red for errors, dim for per-row
//! metadata, optional list hints and statistics.
//! Symbols: → action/changed, ✓ success, ! warning, ✗ error.

pub use crate::output::rows::ListStatus as Status;
use crate::output::{Output, WriteFailure};
use anstyle::{AnsiColor, Style};
use unicode_width::UnicodeWidthStr;

use crate::presentation::UserLine;

const BOLD: Style = Style::new().bold();
const CYAN: Style = AnsiColor::Cyan.on_default();
const GREEN: Style = AnsiColor::Green.on_default();
const YELLOW: Style = AnsiColor::Yellow.on_default();
const RED: Style = AnsiColor::Red.on_default();
const BOLD_RED: Style = AnsiColor::Red.on_default().bold();
const DIM: Style = Style::new().dimmed();

/// Keep clap-owned help and usage diagnostics in the same semantic palette.
#[must_use]
pub(crate) const fn help_styles() -> clap::builder::Styles {
    clap::builder::Styles::styled()
        .header(BOLD)
        .usage(BOLD)
        .literal(CYAN)
        .placeholder(CYAN)
        .error(BOLD_RED)
        .valid(GREEN)
        .invalid(YELLOW)
}

/// → action/changed-item symbol.
pub const ACTION: &str = "→";
/// ✓ success symbol.
pub const SUCCESS: &str = "✓";
/// ! warning symbol.
pub const WARNING: &str = "!";
/// ✗ error symbol.
pub const ERROR: &str = "✗";

/// Wrap `word` in red, e.g. for an error.
pub fn red(word: &str) -> String {
    format!("{RED}{word}{RED:#}")
}

/// Wrap row metadata, optional list hints or statistics in a dim style.
pub fn dim(word: &str) -> String {
    format!("{DIM}{word}{DIM:#}")
}

/// Wrap `word` in bold red, e.g. for the `error:` label itself.
pub fn bold_red(word: &str) -> String {
    format!("{BOLD_RED}{word}{BOLD_RED:#}")
}

/// Print a → action line to stderr: something happening or about to
/// happen that isn't yet a final result. Takes an already-approved
/// [`UserLine`] rather than a generic `impl Display`/format template, so
/// callers must classify dynamic text at the call site; there is no raw
/// formatting entry
/// point left here.
pub fn action(output: &mut Output<'_>, line: &UserLine) -> Result<(), WriteFailure> {
    output.stderr(format_args!(
        "{} {}",
        Status::Pending.styled(),
        line.as_str()
    ))
}

/// Print a ✓ success line to stderr: a routine confirmation, not the
/// command's actual stdout result.
pub fn success(output: &mut Output<'_>, line: &UserLine) -> Result<(), WriteFailure> {
    output.stderr(format_args!(
        "{} {}",
        Status::Success.styled(),
        line.as_str()
    ))
}

/// Print a ! warning line to stderr for a problem or partial result needing attention.
pub fn caution(output: &mut Output<'_>, line: &UserLine) -> Result<(), WriteFailure> {
    output.stderr(format_args!(
        "{} {}",
        Status::Warning.styled(),
        line.as_str()
    ))
}

impl Status {
    /// The bare, uncolored symbol -- what a reader sees with `NO_COLOR`/ANSI
    /// stripped, so it must stay unambiguous on its own.
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Pending => ACTION,
            Self::Success => SUCCESS,
            Self::Warning => WARNING,
            Self::Error => ERROR,
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::Untracked => "?",
            Self::Ignored => "i",
            Self::Conflict => "!",
            Self::Skipped => ".",
        }
    }

    /// The symbol colored according to its semantic meaning.
    pub fn styled(self) -> String {
        let style = self.style();
        format!("{style}{}{style:#}", self.symbol())
    }

    const fn style(self) -> Style {
        match self {
            Self::Pending | Self::Renamed | Self::Copied => CYAN,
            Self::Success | Self::Added => GREEN,
            Self::Warning | Self::Modified | Self::Untracked => YELLOW,
            Self::Error | Self::Deleted | Self::Conflict => RED,
            Self::Ignored | Self::Skipped => DIM,
        }
    }
}

/// Render a semantic heading: `<symbol> <bold label>: <summary>`.
pub fn status_heading(status: Status, label: &UserLine, summary: &UserLine) -> String {
    format!(
        "{} {BOLD}{}{BOLD:#}: {}",
        status.styled(),
        label.as_str(),
        summary.as_str()
    )
}

/// A `→`-prefixed heading for a list of pending/in-progress items, e.g.
/// `→ Changes to commit: 5`.
pub fn action_heading(label: &UserLine, summary: &UserLine) -> String {
    status_heading(Status::Pending, label, summary)
}

/// A `✓`-prefixed heading for a list of healthy/complete items, e.g.
/// `✓ Tracked files: 24`.
pub fn success_heading(label: &UserLine, summary: &UserLine) -> String {
    status_heading(Status::Success, label, summary)
}

/// A `!`-prefixed heading for a list of items needing attention, e.g.
/// `! Conflicts: 2`.
pub fn caution_heading(label: &UserLine, summary: &UserLine) -> String {
    status_heading(Status::Warning, label, summary)
}

/// Render a generic semantic group heading without a `: summary`, e.g.
/// `! Lock` or `✓ Cache`.
pub fn status_group(status: Status, title: &UserLine) -> String {
    let style = status.style();
    format!(
        "{style}{}{style:#} {BOLD}{}{BOLD:#}",
        status.symbol(),
        title.as_str()
    )
}

/// One row of a rendered list: a status symbol, the primary
/// path (never wrapped, never dimmed), and optional secondary metadata
/// (begins after at least two spaces; always dimmed). Both `path` and
/// `metadata` are already-approved [`UserLine`]s, not raw strings --
/// the approved-line guarantee survives from the row model all the way
/// to this final renderer.
#[derive(Clone)]
pub struct ListItem<'a> {
    pub status: Status,
    pub path: &'a UserLine,
    pub metadata: Option<&'a UserLine>,
}

impl<'a> ListItem<'a> {
    pub const fn new(status: Status, path: &'a UserLine) -> Self {
        ListItem {
            status,
            path,
            metadata: None,
        }
    }

    pub const fn with_metadata(status: Status, path: &'a UserLine, metadata: &'a UserLine) -> Self {
        Self {
            status,
            path,
            metadata: Some(metadata),
        }
    }
}

/// Render a single item, padding `path` to `path_width` so metadata
/// columns line up across every row that has one.
fn render_item(item: &ListItem, path_width: usize) -> String {
    let status = item.status.styled();
    let path = item.path.as_str();
    match &item.metadata {
        Some(meta) => format!(
            "{status}  {path}{}  {}",
            " ".repeat(path_width.saturating_sub(path.width())),
            dim(meta.as_str())
        ),
        None => format!("{status}  {path}"),
    }
}

/// Render a full group of items, computing the path column width once so
/// every row's metadata (if any) lines up. Formatting is lazy after that
/// width pass, so callers can stop immediately on a write failure.
pub fn list_items<'a>(
    items: impl Iterator<Item = ListItem<'a>> + Clone,
) -> impl Iterator<Item = String> {
    let path_width = items
        .clone()
        .filter(|i| i.metadata.is_some())
        .map(|i| i.path.as_str().width())
        .max()
        .unwrap_or(0);
    items.map(move |i| render_item(&i, path_width))
}

/// Render resource details with bold labels and full-contrast, unwrapped values.
/// Align within the block; repeated labels preserve individual glob patterns.
pub fn fields(fields: &[(UserLine, UserLine)]) -> Vec<String> {
    let width = fields
        .iter()
        .map(|(label, _)| label.as_str().width())
        .max()
        .unwrap_or(0);
    fields
        .iter()
        .map(|(label, value)| {
            format!(
                "  {BOLD}{}{BOLD:#}: {}{}",
                label.as_str(),
                " ".repeat(width.saturating_sub(label.as_str().width())),
                value.as_str()
            )
        })
        .collect()
}

/// Optional context for a list, between its rows and footer on the same stream.
/// The entire line is dim: required warnings and recovery instructions must
/// use standalone normal-contrast messages or diagnostics instead.
pub fn list_hint(text: &UserLine) -> String {
    dim(&hint_text(text, wrap_width()))
}

fn hint_text(text: &UserLine, width: usize) -> String {
    wrap_user_line("hint: ", text, width)
}

/// Render a list footer: optional dim statistics below the items,
/// e.g. `5 changes across 4 files`. Omit the footer entirely
/// (don't call this) when there's nothing worth summarizing.
pub fn list_footer(text: &UserLine) -> String {
    dim(text.as_str())
}

/// Terminal width to wrap prose at: the actual terminal width, capped at
/// 100 columns so lines stay readable on very wide monitors, and falling
/// back to 100 when the width can't be determined (piped output, no tty).
/// Only for human prose (error messages, hints) -- never for paths,
/// hashes, or other machine-readable output, which must never be wrapped.
pub fn wrap_width() -> usize {
    terminal_size::terminal_size().map_or(100, |(terminal_size::Width(w), _)| (w as usize).min(100))
}

/// Word-wraps `text` to `width` columns with a hanging indent: the first
/// line is prefixed with `prefix` (e.g. `"hint: "`) and every wrapped
/// continuation line is indented to align under `text`'s first
/// character instead of under `prefix`, so a multi-line hint reads as
/// one visually distinct paragraph rather than looking like a second,
/// unrelated line.
pub fn wrap_with_hanging_indent(prefix: &str, text: &str, width: usize) -> String {
    wrap_words(prefix, text.split_whitespace(), width)
}

/// Wrap approved prose while preserving dynamic identities, even with spaces.
pub fn wrap_user_line(prefix: &str, text: &UserLine, width: usize) -> String {
    wrap_words(prefix, text.wrapping_words(), width)
}

fn wrap_words<'a>(prefix: &str, words: impl Iterator<Item = &'a str>, width: usize) -> String {
    let indent_width = prefix.width();
    let indent = " ".repeat(indent_width);
    let mut out = String::new();
    let mut column = indent_width;
    let mut has_word = false;
    for word in words {
        if has_word {
            if column + 1 + word.width() > width {
                out.push('\n');
                out.push_str(&indent);
                column = indent_width;
            } else {
                out.push(' ');
                column += 1;
            }
        } else {
            out.push_str(prefix);
        }
        out.push_str(word);
        column += word.width();
        has_word = true;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::strip_ansi as plain;

    fn list_items(items: &[ListItem<'_>]) -> Vec<String> {
        super::list_items(items.iter().cloned()).collect()
    }

    #[test]
    fn list_formatting_stops_when_the_consumer_stops() {
        let path = UserLine::authored("path");
        let visits = std::cell::Cell::new(0);
        let items = (0..10).map(|_| {
            visits.set(visits.get() + 1);
            ListItem::new(Status::Success, &path)
        });
        let mut lines = super::list_items(items);
        assert_eq!(visits.get(), 10); // Width pass only.
        assert!(lines.next().is_some());
        drop(lines); // A failed writer does not request another row.
        assert_eq!(visits.get(), 11);
    }

    #[test]
    fn result_hints_align_continuations_and_preserve_identities() {
        let hint = UserLine::compose([
            UserLine::authored("Paths owned by mount '"),
            UserLine::identifier("my mount"),
            UserLine::authored("' at target '"),
            UserLine::path_text("data  files/日本語.bin"),
            UserLine::authored("' were skipped."),
        ]);
        let wrapped = hint_text(&hint, 28);
        assert!(wrapped.contains("'my mount'"));
        assert!(wrapped.contains("'data  files/日本語.bin'"));
        assert!(wrapped.lines().count() > 1);
        assert!(
            wrapped
                .lines()
                .skip(1)
                .all(|line| line.starts_with("      "))
        );
        assert_eq!(
            hint_text(&UserLine::authored("Nothing skipped."), 100),
            "hint: Nothing skipped."
        );
    }

    #[test]
    fn wrap_breaks_at_width_without_splitting_words() {
        let text = "the quick brown fox jumps over the lazy dog";
        let wrapped = wrap_with_hanging_indent("", text, 15);
        for line in wrapped.lines() {
            assert!(line.len() <= 15, "line too long: {line:?}");
        }
        // No words lost or mangled by the wrap.
        assert_eq!(
            wrapped.split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn wrap_leaves_short_text_on_one_line() {
        assert_eq!(
            wrap_with_hanging_indent("", "short message", 100),
            "short message"
        );
    }

    #[test]
    fn wrap_with_hanging_indent_aligns_continuation_lines_under_the_body_not_the_prefix() {
        let text = "resolve the conflict in the working tree then run gat sync again to finish";
        let wrapped = wrap_with_hanging_indent("hint: ", text, 30);
        let lines: Vec<&str> = wrapped.lines().collect();
        assert!(
            lines.len() > 1,
            "expected the hint to wrap onto more than one line"
        );
        assert!(lines[0].starts_with("hint: "));
        for line in &lines[1..] {
            // Continuation lines align under the first line's text, not
            // under "hint: " itself, and don't repeat the prefix.
            assert!(line.starts_with("      "));
            assert!(!line.trim_start().starts_with("hint:"));
        }
        // No words lost or mangled by the wrap.
        assert_eq!(
            wrapped.split_whitespace().skip(1).collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn status_symbols_only_share_the_attention_marker() {
        let all = [
            Status::Pending,
            Status::Success,
            Status::Warning,
            Status::Error,
            Status::Added,
            Status::Modified,
            Status::Deleted,
            Status::Renamed,
            Status::Copied,
            Status::Untracked,
            Status::Ignored,
            Status::Conflict,
            Status::Skipped,
        ];
        let symbols: Vec<&str> = all.iter().map(|s| s.symbol()).collect();
        let unique: std::collections::HashSet<&&str> = symbols.iter().collect();
        assert_eq!(unique.len(), symbols.len() - 1);
        assert_eq!(Status::Warning.symbol(), "!");
        assert_eq!(Status::Conflict.symbol(), "!");
    }

    #[test]
    fn list_heading_keeps_symbol_and_label_when_ansi_stripped() {
        let heading = success_heading(
            &UserLine::identifier("Tracked files"),
            &UserLine::number(24),
        );
        assert_eq!(plain(&heading), "✓ Tracked files: 24");
    }

    #[test]
    fn list_items_align_metadata_by_longest_path() {
        let path_a = UserLine::identifier("data/report.csv");
        let path_b = UserLine::identifier("assets/image.png");
        let metadata = UserLine::identifier("local ↔ remote");
        let items = vec![
            ListItem::with_metadata(Status::Conflict, &path_a, &metadata),
            ListItem::with_metadata(Status::Conflict, &path_b, &metadata),
        ];
        let lines = list_items(&items);
        let plain_lines: Vec<String> = lines.iter().map(|l| plain(l)).collect();
        // Both metadata columns start at the same offset.
        let offset = |line: &str| line.find("local").unwrap();
        assert_eq!(offset(&plain_lines[0]), offset(&plain_lines[1]));
    }

    #[test]
    fn list_item_without_metadata_has_no_trailing_gap() {
        let path = UserLine::identifier("Cargo.toml");
        let item = ListItem::new(Status::Success, &path);
        let line = plain(&list_items(&[item])[0]);
        assert_eq!(line, "✓  Cargo.toml");
    }

    #[test]
    fn list_alignment_and_wrapping_use_terminal_columns() {
        let a = UserLine::identifier("界.bin");
        let b = UserLine::identifier("e\u{301}.bin");
        let detail = UserLine::authored("value");
        let lines = list_items(&[
            ListItem::with_metadata(Status::Success, &a, &detail),
            ListItem::with_metadata(Status::Success, &b, &detail),
        ]);
        let columns: Vec<_> = lines
            .iter()
            .map(|line| {
                let plain = plain(line);
                plain[..plain.find("value").unwrap()].width()
            })
            .collect();
        assert_eq!(columns, [11, 11]);
        assert_eq!(wrap_with_hanging_indent("", "界 界 界", 5), "界 界\n界");
        assert_eq!(
            wrap_with_hanging_indent("", "e\u{301} e\u{301}", 3),
            "e\u{301} e\u{301}"
        );
    }

    #[test]
    fn fields_remain_plain_and_row_metadata_is_always_dimmed() {
        let label = UserLine::authored("url");
        let value = UserLine::identifier("storage/path with spaces");
        let field = fields(&[(label.clone(), value.clone())]).remove(0);
        assert!(!field.contains("\x1b[2m"));
        assert!(field.ends_with(value.as_str()));
        for status in [
            Status::Success,
            Status::Warning,
            Status::Error,
            Status::Conflict,
            Status::Skipped,
        ] {
            let line = list_items(&[ListItem::with_metadata(status, &label, &value)]).remove(0);
            assert!(line.ends_with(&dim(value.as_str())), "{line:?}");
        }
        let ordinary =
            list_items(&[ListItem::with_metadata(Status::Success, &label, &value)]).remove(0);
        assert!(ordinary.contains("\x1b[2m"));
    }

    #[test]
    fn fields_align_values_and_preserve_repeated_patterns() {
        let path = UserLine::authored("Path");
        let include = UserLine::authored("Include");
        let a = UserLine::identifier("data files");
        let b = UserLine::identifier("**/*, final.bin");
        let rendered = fields(&[(path, a.clone()), (include.clone(), b), (include, a)]);
        let plain: Vec<_> = rendered.iter().map(|line| plain(line)).collect();
        assert_eq!(
            plain,
            [
                "  Path:    data files",
                "  Include: **/*, final.bin",
                "  Include: data files"
            ]
        );
    }
}
