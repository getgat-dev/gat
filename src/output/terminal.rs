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

use super::flow;
pub use crate::output::rows::ListStatus as Status;
use crate::output::{Output, Stream, WriteFailure};
use anstyle::{AnsiColor, Style};
use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;
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
    message(output, Stream::Stderr, Status::Pending, line)
}

/// Print a ✓ success line to stderr: a routine confirmation, not the
/// command's actual stdout result.
pub fn success(output: &mut Output<'_>, line: &UserLine) -> Result<(), WriteFailure> {
    message(output, Stream::Stderr, Status::Success, line)
}

/// Print a ! warning line to stderr for a problem or partial result needing attention.
pub fn caution(output: &mut Output<'_>, line: &UserLine) -> Result<(), WriteFailure> {
    message(output, Stream::Stderr, Status::Warning, line)
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

#[cfg(test)]
pub fn success_heading_text(label: &UserLine, summary: &UserLine, width: usize) -> String {
    let mut bytes = Vec::new();
    let mut stderr = Vec::new();
    let mut output = Output::new(&mut bytes, &mut stderr);
    output.set_layouts(
        super::OutputLayout::bounded(width),
        super::OutputLayout::default(),
    );
    success_heading(&mut output, Stream::Stdout, label, summary).unwrap();
    String::from_utf8(bytes)
        .unwrap()
        .trim_end_matches('\n')
        .to_owned()
}

/// Separate adjacent semantic blocks with one empty physical line. Callers own
/// report order and omit absent blocks; this helper owns separator formatting.
pub fn section(output: &mut Output<'_>, stream: Stream) -> Result<(), WriteFailure> {
    output.line(stream, format_args!(""))
}

/// Hints form one optional block, independent of the number of visible rows.
pub fn hints(
    output: &mut Output<'_>,
    stream: Stream,
    hints: &[UserLine],
) -> Result<(), WriteFailure> {
    if !hints.is_empty() {
        section(output, stream)?;
    }
    for hint in hints {
        list_hint(output, stream, hint)?;
    }
    Ok(())
}

/// Required content retains normal contrast; only optional context is muted.
#[derive(Clone, Copy)]
pub enum Emphasis {
    Normal,
    Muted,
}

fn indentation(columns: usize, width: usize) -> String {
    " ".repeat(if columns < width { columns } else { 0 })
}

/// Write a paragraph incrementally; no subsequent line is laid out after a
/// write failure. Dim is reserved for optional context, never recovery steps.
pub fn paragraph(
    output: &mut Output<'_>,
    stream: Stream,
    text: &UserLine,
    indent: usize,
    emphasis: Emphasis,
) -> Result<(), WriteFailure> {
    let width = output.prose_width(stream);
    let prefix = indentation(indent, width);
    for line in flow::lines(&prefix, &prefix, text, width) {
        match emphasis {
            Emphasis::Muted => output.line(stream, format_args!("{DIM}{line}{DIM:#}"))?,
            Emphasis::Normal => output.line(stream, format_args!("{line}"))?,
        }
    }
    Ok(())
}

/// The flow engine omits padding for prefix-only lines. Body whitespace belongs
/// to approved text and must survive even when it begins with spaces.
const fn body_gap(line: &flow::PhysicalLine<'_>) -> &'static str {
    if line.body.is_empty() { "" } else { " " }
}

fn message(
    output: &mut Output<'_>,
    stream: Stream,
    status: Status,
    text: &UserLine,
) -> Result<(), WriteFailure> {
    let prefix = format!("{} ", status.symbol());
    for (index, line) in flow::lines(&prefix, "  ", text, output.prose_width(stream)).enumerate() {
        if index == 0 {
            output.line(
                stream,
                format_args!("{}{}{}", status.styled(), body_gap(&line), line.body),
            )?;
        } else {
            output.line(stream, format_args!("{line}"))?;
        }
    }
    Ok(())
}

/// Semantic output methods resolve width from the destination, never the environment.
pub fn status_heading(
    output: &mut Output<'_>,
    stream: Stream,
    status: Status,
    label: &UserLine,
    summary: &UserLine,
) -> Result<(), WriteFailure> {
    let width = output.prose_width(stream);
    let prefix = format!("{} {}: ", status.symbol(), label.as_str());
    if prefix.width() >= width {
        let title = UserLine::compose([label.clone(), UserLine::authored(":")]);
        status_group(output, stream, status, &title)?;
        if summary.wrapping_words().next().is_some() {
            paragraph(output, stream, summary, 2, Emphasis::Normal)?;
        }
        return Ok(());
    }
    for (index, line) in flow::lines(&prefix, "  ", summary, width).enumerate() {
        if index == 0 {
            output.line(
                stream,
                format_args!(
                    "{} {BOLD}{}{BOLD:#}:{}{}",
                    status.styled(),
                    label.as_str(),
                    body_gap(&line),
                    line.body
                ),
            )?;
        } else {
            output.line(stream, format_args!("{line}"))?;
        }
    }
    Ok(())
}
pub fn action_heading(
    output: &mut Output<'_>,
    stream: Stream,
    label: &UserLine,
    summary: &UserLine,
) -> Result<(), WriteFailure> {
    status_heading(output, stream, Status::Pending, label, summary)
}
pub fn success_heading(
    output: &mut Output<'_>,
    stream: Stream,
    label: &UserLine,
    summary: &UserLine,
) -> Result<(), WriteFailure> {
    status_heading(output, stream, Status::Success, label, summary)
}
pub fn caution_heading(
    output: &mut Output<'_>,
    stream: Stream,
    label: &UserLine,
    summary: &UserLine,
) -> Result<(), WriteFailure> {
    status_heading(output, stream, Status::Warning, label, summary)
}
pub fn status_group(
    output: &mut Output<'_>,
    stream: Stream,
    status: Status,
    title: &UserLine,
) -> Result<(), WriteFailure> {
    let prefix = format!("{} ", status.symbol());
    for (index, line) in flow::lines(&prefix, "  ", title, output.prose_width(stream)).enumerate() {
        if index == 0 {
            output.line(
                stream,
                format_args!(
                    "{}{}{BOLD}{}{BOLD:#}",
                    status.styled(),
                    body_gap(&line),
                    line.body
                ),
            )?;
        } else {
            output.line(stream, format_args!("{BOLD}{line}{BOLD:#}"))?;
        }
    }
    Ok(())
}
pub fn list_hint(
    output: &mut Output<'_>,
    stream: Stream,
    text: &UserLine,
) -> Result<(), WriteFailure> {
    for line in flow::lines("hint: ", "      ", text, output.prose_width(stream)) {
        output.line(stream, format_args!("{DIM}{line}{DIM:#}"))?;
    }
    Ok(())
}
pub fn list_footer(
    output: &mut Output<'_>,
    stream: Stream,
    text: &UserLine,
) -> Result<(), WriteFailure> {
    paragraph(output, stream, text, 0, Emphasis::Muted)
}

/// One row of a rendered list: a status symbol, the primary
/// path (never wrapped or dimmed; clipped in bounded mode), and optional secondary metadata
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

/// Computed once per visible group; all status symbols occupy one column.
struct RowColumns {
    total: Option<usize>,
    label: usize,
    metadata: Option<usize>,
}

const STATUS_PREFIX_COLUMNS: usize = 3;
const COLUMN_GAP: usize = 2;

impl RowColumns {
    fn measure<'a>(items: impl Iterator<Item = ListItem<'a>>, total: Option<usize>) -> Self {
        let metadata = total.map(|width| {
            if width < super::layout::MIN_TWO_COLUMN_WIDTH {
                0
            } else {
                super::layout::MAX_METADATA_COLUMNS
                    .min(width.saturating_sub(STATUS_PREFIX_COLUMNS) / 3)
            }
        });
        // A one-column layout needs neither alignment nor a metadata scan.
        if metadata == Some(0) {
            return Self {
                total,
                label: 0,
                metadata,
            };
        }
        let (label, detail) = items.fold((0, 0), |(label, detail), item| {
            (
                label.max(item.path.as_str().width()),
                detail.max(item.metadata.map_or(0, |text| text.as_str().width())),
            )
        });
        let label = total.map_or(label, |width| {
            let reserved = detail.min(metadata.unwrap_or(0));
            let gap = if reserved > 0 { COLUMN_GAP } else { 0 };
            label.min(width.saturating_sub(STATUS_PREFIX_COLUMNS + reserved + gap))
        });
        Self {
            total,
            label,
            metadata,
        }
    }

    fn render(&self, item: &ListItem<'_>) -> String {
        if let Some(width) = self.total
            && width <= STATUS_PREFIX_COLUMNS
        {
            return truncate("...", width).into_owned();
        }
        let metadata = item
            .metadata
            .filter(|_| self.metadata != Some(0))
            .map(|text| clip(text.as_str(), self.metadata))
            .filter(|text| !text.is_empty());
        let label_width = self.total.map(|width| {
            if metadata.is_some() {
                self.label
            } else {
                width.saturating_sub(STATUS_PREFIX_COLUMNS)
            }
        });
        let label = clip(item.path.as_str(), label_width);
        let status = item.status.styled();
        match metadata {
            Some(metadata) => {
                let padding = self.label.saturating_sub(label.width());
                // Format padding and metadata style directly into the final allocation.
                format!("{status}  {label}{:padding$}  {DIM}{metadata}{DIM:#}", "")
            }
            None => format!("{status}  {label}"),
        }
    }
}

fn clip(text: &str, columns: Option<usize>) -> Cow<'_, str> {
    columns.map_or_else(|| Cow::Borrowed(text), |width| truncate(text, width))
}

/// Clip approved, escaped text before adding ANSI styles. Never split a
/// grapheme (including combining marks and joined emoji).
pub(crate) fn truncate(text: &str, columns: usize) -> Cow<'_, str> {
    if text.width() <= columns {
        return Cow::Borrowed(text);
    }
    if columns <= 3 {
        return Cow::Owned(".".repeat(columns));
    }
    let mut result = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let next = grapheme.width();
        if width + next > columns - 3 {
            break;
        }
        result.push_str(grapheme);
        width += next;
    }
    result.push_str("...");
    Cow::Owned(result)
}

/// Align only visible items. Formatting stays lazy so write failures stop work.
pub fn list_items<'a>(
    items: impl Iterator<Item = ListItem<'a>> + Clone,
    columns: Option<usize>,
) -> impl Iterator<Item = String> {
    let layout = RowColumns::measure(items.clone(), columns);
    items.map(move |item| layout.render(&item))
}

/// Align field values within a block; stack labels on narrow terminals.
/// Protected values remain copyable even when wider than the soft limit.
pub fn fields(
    output: &mut Output<'_>,
    stream: Stream,
    fields: &[(UserLine, UserLine)],
) -> Result<(), WriteFailure> {
    let width = output.prose_width(stream);
    let label_width = fields
        .iter()
        .map(|(label, _)| label.as_str().width())
        .max()
        .unwrap_or(0);
    let stacked = width < super::layout::MIN_TWO_COLUMN_WIDTH || label_width + 4 > width / 2;
    let indent = indentation(2, width);
    for (label, value) in fields {
        if stacked {
            let label = UserLine::compose([label.clone(), UserLine::authored(":")]);
            for line in flow::lines(&indent, &indent, &label, width) {
                output.line(stream, format_args!("{BOLD}{line}{BOLD:#}"))?;
            }
            paragraph(output, stream, value, 4, Emphasis::Normal)?;
        } else {
            let prefix = format!(
                "  {}: {}",
                label.as_str(),
                " ".repeat(label_width - label.as_str().width())
            );
            let continuation = " ".repeat(prefix.width());
            for (index, line) in flow::lines(&prefix, &continuation, value, width).enumerate() {
                if index == 0 {
                    let gap = if line.body.is_empty() {
                        0
                    } else {
                        label_width - label.as_str().width() + 1
                    };
                    output.line(
                        stream,
                        format_args!(
                            "  {BOLD}{}{BOLD:#}:{:gap$}{}",
                            label.as_str(),
                            "",
                            line.body
                        ),
                    )?;
                } else {
                    output.line(stream, format_args!("{line}"))?;
                }
            }
        }
    }
    Ok(())
}

/// Optional context below rows; required recovery uses normal paragraphs.
#[cfg(test)]
pub fn list_hint_text(text: &UserLine, width: usize) -> String {
    flow::lines("hint: ", "      ", text, width)
        .map(|line| dim(&line.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
fn hint_text(text: &UserLine, width: usize) -> String {
    wrap_user_line("hint: ", text, width)
}

#[cfg(test)]
pub fn wrap_user_line(prefix: &str, text: &UserLine, width: usize) -> String {
    flow::wrap(prefix, &" ".repeat(prefix.width()), text, width)
}

#[cfg(test)]
fn wrap_with_hanging_indent(prefix: &str, text: &'static str, width: usize) -> String {
    wrap_user_line(prefix, &UserLine::authored(text), width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::strip_ansi as plain;

    fn capture(width: usize, full: bool, render: impl FnOnce(&mut Output<'_>)) -> String {
        let mut bytes = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut bytes, &mut stderr);
        output.set_layouts(
            super::super::OutputLayout::bounded(width),
            super::super::OutputLayout::default(),
        );
        output.set_full_output(full);
        render(&mut output);
        plain(&String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn an_empty_heading_summary_does_not_create_an_extra_block_when_stacked() {
        for width in [1, 8, 40, 100] {
            let rendered = capture(width, false, |output| {
                success_heading(
                    output,
                    Stream::Stdout,
                    &UserLine::authored("Summary"),
                    &UserLine::authored(""),
                )
                .unwrap();
            });
            assert!(!rendered.ends_with("\n\n"));
            assert!(rendered.contains("Summary:"));
        }
    }

    #[test]
    fn durable_elements_share_width_and_keep_prose_in_full_mode() {
        let body = UserLine::authored("A few files need care before the next sync can start.");
        for width in [20, 39, 40, 80, 100, 140] {
            let render = |output: &mut Output<'_>| {
                super::success_heading(output, Stream::Stdout, &UserLine::authored("Sync"), &body)
                    .unwrap();
                super::list_hint(output, Stream::Stdout, &body).unwrap();
                super::list_footer(output, Stream::Stdout, &body).unwrap();
                super::fields(
                    output,
                    Stream::Stdout,
                    &[(UserLine::authored("Note"), body.clone())],
                )
                .unwrap();
            };
            let compact = capture(width, false, render);
            assert_eq!(compact, capture(width, true, render));
            for line in compact.lines() {
                assert!(line.width() <= width.min(100), "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn fields_stack_below_minimum_and_align_continuation_values() {
        let entries = [(
            UserLine::authored("Note"),
            UserLine::authored("A few files need care before the next sync can start."),
        )];
        let narrow = capture(39, false, |output| {
            super::fields(output, Stream::Stdout, &entries).unwrap();
        });
        assert!(narrow.starts_with("  Note:\n    A few"));
        assert!(narrow.lines().skip(1).all(|line| line.starts_with("    ")));
        let wide = capture(40, false, |output| {
            super::fields(output, Stream::Stdout, &entries).unwrap();
        });
        assert!(wide.starts_with("  Note: A few"));
        assert!(
            wide.lines()
                .skip(1)
                .all(|line| line.starts_with("        "))
        );
    }

    #[test]
    fn stream_widths_are_independent_and_zero_is_normalized() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut output = Output::new(&mut stdout, &mut stderr);
        output.set_layouts(
            super::super::OutputLayout::bounded(20),
            super::super::OutputLayout::bounded(80),
        );
        let text = UserLine::authored("A few files need care before the next sync can start.");
        super::list_hint(&mut output, Stream::Stdout, &text).unwrap();
        super::list_hint(&mut output, Stream::Stderr, &text).unwrap();
        assert!(plain(&String::from_utf8(stdout).unwrap()).lines().count() > 1);
        assert_eq!(
            plain(&String::from_utf8(stderr).unwrap()).lines().count(),
            1
        );
        assert_eq!(super::super::OutputLayout::bounded(0).prose_width(), 1);
    }

    #[test]
    fn field_values_preserve_spaces_and_escaped_controls() {
        let value = UserLine::identifier("  日本語 file  \n\u{1b}");
        for width in [1, 8, 39, 40, 100] {
            let rendered = capture(width, false, |output| {
                super::fields(
                    output,
                    Stream::Stdout,
                    &[(UserLine::authored("Path"), value.clone())],
                )
                .unwrap();
            });
            assert!(rendered.contains(value.as_str()));
            assert!(!rendered.contains('\u{1b}'));
        }
    }

    #[test]
    fn group_headings_preserve_protected_spaces_and_do_not_pad_empty_prefixes() {
        for width in [1, 2, 8, 40, 100] {
            let title = UserLine::identifier("  release  ");
            let rendered = capture(width, false, |output| {
                status_group(output, Stream::Stdout, Status::Success, &title).unwrap();
            });
            assert!(rendered.contains(title.as_str()));
            if width <= 2 {
                assert_eq!(rendered.lines().next(), Some("✓"));
            }
        }
    }

    #[test]
    fn indentation_collapses_consistently_when_it_consumes_the_width() {
        for width in [1, 2] {
            let rendered = capture(width, false, |output| {
                paragraph(
                    output,
                    Stream::Stdout,
                    &UserLine::authored("a b"),
                    2,
                    Emphasis::Normal,
                )
                .unwrap();
            });
            assert_eq!(rendered, "a\nb\n");
        }
    }

    #[test]
    fn field_styling_targets_the_label_even_when_it_is_whitespace() {
        let mut bytes = Vec::new();
        super::fields(
            &mut Output::new(&mut bytes, &mut Vec::new()),
            Stream::Stdout,
            &[(UserLine::identifier(" "), UserLine::authored("value"))],
        )
        .unwrap();
        let rendered = String::from_utf8(bytes).unwrap();
        assert_eq!(rendered, format!("  {BOLD} {BOLD:#}: value\n"));
    }

    fn fields(entries: &[(UserLine, UserLine)]) -> Vec<String> {
        let mut bytes = Vec::new();
        super::fields(
            &mut Output::new(&mut bytes, &mut Vec::new()),
            Stream::Stdout,
            entries,
        )
        .unwrap();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn list_items(items: &[ListItem<'_>]) -> Vec<String> {
        super::list_items(items.iter().cloned(), None).collect()
    }

    #[test]
    fn uncut_text_is_borrowed_and_narrow_layout_skips_alignment() {
        assert!(matches!(truncate("日本語", 6), Cow::Borrowed(_)));
        assert!(matches!(clip("complete label", None), Cow::Borrowed(_)));
        let path = UserLine::authored("data.bin");
        let visits = std::cell::Cell::new(0);
        let items = (0..10).map(|_| {
            visits.set(visits.get() + 1);
            ListItem::new(Status::Success, &path)
        });
        let mut lines = super::list_items(items, Some(39));
        assert_eq!(visits.get(), 0);
        assert_eq!(plain(&lines.next().unwrap()), "✓  data.bin");
        assert_eq!(visits.get(), 1);
    }

    #[test]
    fn every_status_symbol_has_the_width_reserved_by_row_layout() {
        for status in [
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
        ] {
            assert_eq!(status.symbol().width() + COLUMN_GAP, STATUS_PREFIX_COLUMNS);
        }
    }

    #[test]
    fn bounded_rows_fit_unicode_and_ansi_at_every_width() {
        let path = UserLine::identifier("日本語/e\u{301}/👩‍💻/long-file-name.bin\n\t\u{1b}[31m");
        let meta = UserLine::authored("new, cached (mount extremely-long-name)");
        for width in 0..=150 {
            let row = super::list_items(
                std::iter::once(ListItem::with_metadata(Status::Added, &path, &meta)),
                Some(width),
            )
            .next()
            .unwrap();
            let plain = plain(&row);
            assert!(plain.width() <= width, "width {width}: {plain:?}");
            assert!(!plain.contains(['\n', '\r', '\t', '\u{1b}']));
        }
        assert_eq!(truncate("e\u{301}abcdef", 4), "e\u{301}...");
        assert_eq!(truncate("👩‍💻abcdef", 5), "👩‍💻...");
    }

    #[test]
    fn two_column_layout_respects_its_minimum_width() {
        let path = UserLine::authored("data.bin");
        let detail = UserLine::authored("cached");
        for (width, expected) in [(39, "✓  data.bin"), (40, "✓  data.bin  cached")] {
            let line = super::list_items(
                std::iter::once(ListItem::with_metadata(Status::Success, &path, &detail)),
                Some(width),
            )
            .next()
            .unwrap();
            assert_eq!(plain(&line), expected);
        }
    }

    #[test]
    fn bounded_metadata_aligns_and_hidden_rows_do_not_affect_columns() {
        let short = UserLine::authored("short");
        let long = UserLine::authored("a-very-long-label-that-needs-truncation");
        let new = UserLine::authored("new");
        let cached = UserLine::authored("cached");
        let items = [
            ListItem::with_metadata(Status::Added, &short, &new),
            ListItem::with_metadata(Status::Success, &long, &cached),
        ];
        let lines: Vec<_> = super::list_items(items.iter().cloned(), Some(40))
            .map(|line| plain(&line))
            .collect();
        assert_eq!(
            lines[0][..lines[0].find("new").unwrap()].width(),
            lines[1][..lines[1].find("cached").unwrap()].width()
        );
        let only = super::list_items(items.iter().take(1).cloned(), Some(40))
            .next()
            .unwrap();
        assert_eq!(plain(&only), "A  short  new");
    }

    #[test]
    fn list_formatting_stops_when_the_consumer_stops() {
        let path = UserLine::authored("path");
        let visits = std::cell::Cell::new(0);
        let items = (0..10).map(|_| {
            visits.set(visits.get() + 1);
            ListItem::new(Status::Success, &path)
        });
        let mut lines = super::list_items(items, None);
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
        let heading = success_heading_text(
            &UserLine::identifier("Tracked files"),
            &UserLine::number(24),
            100,
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
