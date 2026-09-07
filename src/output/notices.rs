//! Renders lifecycle notices to stderr: the runtime-facing
//! sibling of `output::render` (durable `Outcome` rendering) and
//! `output::progress` (transient progress) -- kept out of command
//! implementations entirely, same reasoning as those two. Call sites at
//! the semantic boundary (`app::run` dispatch, `gat config` read/write,
//! actual consumption of a persisted config value) report observations
//! into a `crate::lifecycle::Lifecycle` sink as they happen; the
//! process adapter drains it here, once, before the durable outcome, so
//! a notice never interleaves with -- or is mistaken for -- stdout
//! output a script might be parsing. Deduplication by
//! [`crate::lifecycle::Notice::surface`] happens once, at the
//! `Lifecycle::observe` call site, not here -- by the time `emit` sees
//! a notice list it's already the deduplicated set to print.

use crate::lifecycle::{Lifecycle, Notice, NoticeKind, lifecycle_reason};
use crate::output::terminal as ui;
use crate::output::{Output, WriteFailure};
use crate::presentation::UserLine;

/// Drains `lifecycle` and renders each recorded notice to stderr.
/// Never touches stdout. Notice content is informational, but a failed write
/// propagates to the process boundary like any other durable output failure.
pub fn emit(output: &mut Output<'_>, lifecycle: &Lifecycle) -> Result<(), WriteFailure> {
    for notice in lifecycle.take_notices() {
        ui::caution(output, &notice_line(&notice))?;
        output.stderr(format_args!(""))?;
    }
    Ok(())
}

/// Builds the final one-line notice text as a [`UserLine`], the only
/// place this wording decision is made for stderr rendering.
/// [`lifecycle_reason`] is the single wording decision tree shared by
/// lifecycle consumers (`doc_warning_text`); this function owns only the
/// plain-text prefix/suffix specific to stderr rendering.
fn notice_line(notice: &Notice) -> UserLine {
    let verb = match notice.kind {
        NoticeKind::Experimental => "Experimental: ",
        NoticeKind::Deprecated => "Deprecated: ",
    };
    let reason = lifecycle_reason(notice.kind, notice.replacement, notice.note);
    UserLine::compose([
        UserLine::authored(verb),
        UserLine::identifier(notice.subject),
        UserLine::authored(" is "),
        UserLine::authored(match notice.kind {
            NoticeKind::Experimental => "experimental",
            NoticeKind::Deprecated => "deprecated",
        }),
        UserLine::authored(": "),
        UserLine::identifier(&reason),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{LifecycleObserve, Surface};

    /// `emit` drains whatever `Lifecycle` actually recorded --
    /// including deduplication, which is `Lifecycle::observe`'s job,
    /// not this module's -- rather than reimplementing a separate
    /// `HashSet` check here. Real stderr capture (that `emit` writes to
    /// stderr, that dispatch calls it exactly once per invocation, and
    /// that stdout/exit codes stay unchanged) is covered by the
    /// process-level tests in `tests/cli_integration.rs`.
    #[test]
    fn emit_drains_the_lifecycle_sink_it_was_given() {
        let lifecycle = Lifecycle::new();
        lifecycle.observe(Surface::Command("gc"));
        lifecycle.observe(Surface::Command("gc"));
        lifecycle.observe(Surface::ConfigAlias {
            canonical: "git.ignore_patterns",
            alias: "git.exclude_patterns",
        });
        assert_eq!(
            lifecycle.take_notices().len(),
            2,
            "dedup + distinct surface survive"
        );

        // `take_notices()` above already drained it; observing again and
        // emitting proves `emit` itself does drain (leaves the sink
        // empty) rather than just peeking.
        lifecycle.observe(Surface::Command("gc"));
        emit(
            &mut Output::new(&mut Vec::new(), &mut Vec::new()),
            &lifecycle,
        )
        .unwrap();
        assert!(
            lifecycle.take_notices().is_empty(),
            "emit must drain, not peek"
        );
    }
}
