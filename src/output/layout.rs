//! Per-stream durable output policy; command outcomes remain complete.

/// Below this width, omit the secondary column to preserve readable labels.
pub(crate) const MIN_TWO_COLUMN_WIDTH: usize = 40;
/// Reserve some detail space when labels are long; short labels can yield more.
pub(crate) const RESERVED_METADATA_COLUMNS: usize = 24;
const MAX_ROW_LINES: usize = 20;
const FALLBACK_COLUMNS: usize = 100;

/// Width and detail policy for durable human output on one stream.
#[derive(Clone, Copy, Debug)]
pub struct OutputLayout {
    columns: usize,
    mode: DetailMode,
}

impl Default for OutputLayout {
    fn default() -> Self {
        Self::bounded(FALLBACK_COLUMNS)
    }
}

impl OutputLayout {
    /// Set physical width; prose caps at 100 and narrow lists omit metadata.
    #[must_use]
    pub const fn bounded(columns: usize) -> Self {
        Self {
            columns: if columns == 0 { 1 } else { columns },
            mode: DetailMode::Compact,
        }
    }

    /// Complete list output with the default prose width.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            columns: FALLBACK_COLUMNS,
            mode: DetailMode::Full,
        }
    }

    pub(crate) const fn columns(self) -> Option<usize> {
        match self.mode {
            DetailMode::Compact => Some(self.columns),
            DetailMode::Full => None,
        }
    }

    pub(crate) const fn detail_mode(self) -> DetailMode {
        self.mode
    }

    /// Preserve physical width when selecting complete list output.
    #[must_use]
    pub const fn with_full_output(mut self, full: bool) -> Self {
        self.mode = if full {
            DetailMode::Full
        } else {
            DetailMode::Compact
        };
        self
    }

    pub(crate) fn prose_width(self) -> usize {
        self.columns.min(FALLBACK_COLUMNS)
    }

    pub(crate) const fn budget(self, total: usize) -> RowBudget {
        let visible = if matches!(self.mode, DetailMode::Compact) && total > MAX_ROW_LINES {
            MAX_ROW_LINES - 1
        } else {
            total
        };
        RowBudget {
            remaining: visible,
            omitted: total - visible,
        }
    }
}

/// Shared by all row groups in a report. The omission marker consumes the
/// twentieth list line and is emitted once, where the first omission occurs.
pub(crate) struct RowBudget {
    remaining: usize,
    omitted: usize,
}

/// Detail selection is presentation policy, independent of column arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetailMode {
    Compact,
    Full,
}

/// The result of consuming one group's share of a report budget. The omission
/// count includes later groups and belongs to this selection alone.
#[derive(Debug)]
#[must_use]
pub(crate) struct RowSelection {
    pub(crate) visible: usize,
    pub(crate) omitted: usize,
}

impl RowBudget {
    /// Consume a complete group's count exactly once, before row projection.
    /// Group counts must partition the total supplied to `OutputLayout::budget`.
    pub(crate) fn take(&mut self, count: usize) -> RowSelection {
        let visible = count.min(self.remaining);
        self.remaining -= visible;
        let omitted = if count > visible {
            std::mem::take(&mut self.omitted)
        } else {
            0
        };
        RowSelection { visible, omitted }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_boundaries_preserve_totals_and_emit_one_omission_marker() {
        for total in [0, 1, 19, 20, 21, 50, 1000] {
            for first_group in 0..=total {
                for layout in [OutputLayout::default(), OutputLayout::full()] {
                    let mut budget = layout.budget(total);
                    let groups = [0, first_group, 0, total - first_group, 0];
                    let selections: Vec<_> =
                        groups.into_iter().map(|count| budget.take(count)).collect();
                    let visible: usize = selections.iter().map(|selection| selection.visible).sum();
                    let omitted: usize = selections.iter().map(|selection| selection.omitted).sum();
                    let markers = selections
                        .iter()
                        .filter(|selection| selection.omitted > 0)
                        .count();
                    assert_eq!(visible + omitted, total);
                    if layout.detail_mode() == DetailMode::Full || total <= 20 {
                        assert_eq!(visible, total);
                        assert_eq!(markers, 0);
                    } else {
                        assert_eq!(visible, 19);
                        assert_eq!(markers, 1);
                    }
                    for (count, selection) in groups.into_iter().zip(selections) {
                        assert!(selection.visible <= count);
                        if count == 0 {
                            assert_eq!(selection.omitted, 0);
                        }
                    }
                }
            }
        }
    }
}
