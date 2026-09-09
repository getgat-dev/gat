//! Shared list orchestration: select rows, project visible domain values, then
//! write terminal rows and their omission marker. Command wording and report
//! hints belong to `render`; width measurement and styling belong to `terminal`.

use super::layout::{DetailMode, RowSelection};
use super::{Output, Stream, WriteFailure, rows, terminal as ui};

/// A heading and its present rows are separate blocks in every grouped report.
pub(super) fn begin_row_section(
    output: &mut Output<'_>,
    stream: Stream,
    selection: &RowSelection,
) -> Result<(), WriteFailure> {
    if selection.visible > 0 || selection.omitted > 0 {
        ui::section(output, stream)?;
    }
    Ok(())
}

pub(super) fn render_rows(
    output: &mut Output<'_>,
    rows: &[rows::ListRow],
    stream: Stream,
) -> Result<(), WriteFailure> {
    let layout = output.layout(stream);
    render_selected_items(
        output,
        row_items(rows, layout.detail_mode()),
        stream,
        layout.budget(rows.len()).take(rows.len()),
    )
}

fn row_items(
    rows: &[rows::ListRow],
    mode: DetailMode,
) -> impl ExactSizeIterator<Item = ui::ListItem<'_>> + Clone {
    rows.iter().map(move |row| match row.metadata(mode) {
        Some(detail) => ui::ListItem::with_metadata(row.status(), row.path(), detail),
        None => ui::ListItem::new(row.status(), row.path()),
    })
}

/// Count domain rows before projecting the selected prefix. A cloneable
/// iterator supports both slices and filters without duplicating eligibility
/// policy. Cloning and counting must not perform presentation work.
pub(super) fn render_projected_rows<T>(
    output: &mut Output<'_>,
    source: impl Iterator<Item = T> + Clone,
    stream: Stream,
    project: impl FnMut(T) -> rows::ListRow,
) -> Result<(), WriteFailure> {
    let layout = output.layout(stream);
    let total = source.clone().count();
    let selection = layout.budget(total).take(total);
    render_selected_rows(output, source.map(project), stream, selection)
}

/// Project only the selected prefix of owned rows. Selection already carries
/// report counts; skipped rows are never formatted or escaped.
pub(super) fn render_selected_rows(
    output: &mut Output<'_>,
    rows: impl Iterator<Item = rows::ListRow>,
    stream: Stream,
    selection: RowSelection,
) -> Result<(), WriteFailure> {
    let visible: Vec<_> = rows.take(selection.visible).collect();
    render_selected_items(
        output,
        row_items(&visible, output.layout(stream).detail_mode()),
        stream,
        selection,
    )
}

/// Write the selected prefix and its marker without accessing the report budget.
/// `items` must contain at least the selected number of visible rows.
pub(super) fn render_selected_items<'a>(
    output: &mut Output<'_>,
    items: impl ExactSizeIterator<Item = ui::ListItem<'a>> + Clone,
    stream: Stream,
    selection: RowSelection,
) -> Result<(), WriteFailure> {
    let columns = output.layout(stream).columns();
    let RowSelection { visible, omitted } = selection;
    debug_assert!(
        items.len() >= visible,
        "selected rows must be supplied to the renderer"
    );
    for line in ui::list_items(items.take(visible), columns) {
        output.line(stream, format_args!("{line}"))?;
    }
    if omitted > 0 {
        let marker = format!("(... {omitted} more rows)");
        let marker = ui::dim(&ui::truncate(&marker, columns.unwrap_or(usize::MAX)));
        output.line(stream, format_args!("{marker}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::UserLine;

    #[test]
    fn a_failed_row_write_never_attempts_the_omission_marker() {
        struct RejectWrites(usize);
        impl std::io::Write for RejectWrites {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                self.0 += 1;
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for stream in [Stream::Stdout, Stream::Stderr] {
            let mut writer = RejectWrites(0);
            let mut unused = Vec::new();
            let mut output = match stream {
                Stream::Stdout => Output::new(&mut writer, &mut unused),
                Stream::Stderr => Output::new(&mut unused, &mut writer),
            };
            let failure = render_projected_rows(&mut output, [0; 1000].iter(), stream, |_| {
                rows::ListRow::new(rows::ListStatus::Success, UserLine::authored("file.bin"))
            })
            .unwrap_err();
            assert_eq!(failure.stream, stream);
            assert_eq!(failure.source.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(writer.0, 1);
            assert!(unused.is_empty());
        }
    }

    #[test]
    fn projection_constructs_only_visible_rows_and_preserves_full_mode() {
        let source: Vec<_> = (0..1000).collect();
        for (full, expected) in [(false, 19), (true, 1000)] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut output = Output::new(&mut stdout, &mut stderr);
            if full {
                output.set_layouts(
                    crate::output::OutputLayout::full(),
                    crate::output::OutputLayout::full(),
                );
            }
            let mut projected = 0;
            render_projected_rows(&mut output, source.iter(), Stream::Stdout, |index| {
                projected += 1;
                rows::ListRow::new(rows::ListStatus::Success, UserLine::number(*index))
            })
            .unwrap();
            assert_eq!(projected, expected);
            let text = crate::output::strip_ansi(&String::from_utf8(stdout).unwrap());
            assert_eq!(text.contains("(... 981 more rows)"), !full);
        }
    }

    #[test]
    fn filtering_counts_without_projecting_excluded_or_hidden_rows() {
        for total in [0, 1, 20, 25] {
            for full in [false, true] {
                let mut bytes = Vec::new();
                let mut stderr = Vec::new();
                let mut output = Output::new(&mut bytes, &mut stderr);
                output.set_full_output(full);
                let mut source = (0..total * 2).filter(|index| index % 2 == 0).peekable();
                // Report assembly can inspect emptiness without consuming the
                // first row or changing the count used for omission.
                assert_eq!(source.peek().is_some(), total > 0);
                let mut projected = Vec::new();
                render_projected_rows(&mut output, source, Stream::Stdout, |index| {
                    projected.push(index);
                    rows::ListRow::new(rows::ListStatus::Added, UserLine::number(index))
                })
                .unwrap();
                let visible = if !full && total > 20 { 19 } else { total };
                assert_eq!(
                    projected,
                    (0..visible).map(|index| index * 2).collect::<Vec<_>>()
                );
                let text = crate::output::strip_ansi(&String::from_utf8(bytes).unwrap());
                assert_eq!(text.is_empty(), total == 0);
                assert_eq!(text.contains("(... 6 more rows)"), total == 25 && !full);
                assert!(stderr.is_empty());
            }
        }
    }
}
