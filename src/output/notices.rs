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

use crate::lifecycle::{Lifecycle, Notice, NoticeKind};
use crate::output::terminal as ui;
use crate::output::{Output, Stream, WriteFailure};
use crate::presentation::UserLine;

/// Drains `lifecycle` and renders each recorded notice to stderr.
/// Never touches stdout. Notice content is informational, but a failed write
/// propagates to the process boundary like any other durable output failure.
pub fn emit(output: &mut Output<'_>, lifecycle: &Lifecycle) -> Result<(), WriteFailure> {
    for notice in lifecycle.take_notices() {
        ui::caution(output, &notice_line(&notice))?;
        ui::section(output, Stream::Stderr)?;
    }
    Ok(())
}

/// Builds the approved logical notice text as a [`UserLine`], the only
/// place this wording decision is made for stderr rendering.
/// Preserve authored prose and replacement identities separately for wrapping.
/// Lifecycle owns the shared wording, including the prose/identity boundary.
fn notice_line(notice: &Notice) -> UserLine {
    let verb = match notice.kind {
        NoticeKind::Experimental => "Experimental: ",
        NoticeKind::Deprecated => "Deprecated: ",
    };
    let reason =
        gat_core::lifecycle::lifecycle_reason_parts(notice.kind, notice.replacement, notice.note);
    UserLine::compose([
        UserLine::authored(verb),
        UserLine::identifier(notice.subject),
        UserLine::authored(" - "),
        UserLine::authored(reason.prefix),
        UserLine::identifier(reason.replacement.unwrap_or_default()),
        UserLine::authored(reason.suffix),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{LifecycleObserve, Surface};

    #[test]
    fn registered_notices_fit_one_line_at_the_default_width() {
        for spec in crate::lifecycle::REGISTRY {
            let Some(notice) = gat_core::lifecycle::notice_for(spec) else {
                continue;
            };
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            ui::caution(
                &mut Output::new(&mut stdout, &mut stderr),
                &notice_line(&notice),
            )
            .unwrap();
            let plain = crate::output::strip_ansi(&String::from_utf8(stderr).unwrap());
            assert_eq!(plain.lines().count(), 1, "{plain}");
            assert!(stdout.is_empty());
        }
        let notice =
            gat_core::lifecycle::notice_for(crate::lifecycle::command("route").unwrap()).unwrap();
        assert_eq!(
            notice_line(&notice).as_str(),
            "Experimental: `gat route` - its interface or behavior may change."
        );
    }

    #[test]
    fn notice_prose_matches_documentation_and_preserves_replacement_identity() {
        for kind in [NoticeKind::Experimental, NoticeKind::Deprecated] {
            for (replacement, note) in [
                (None, None),
                (Some("gat system clean"), None),
                (None, Some("Use the new command.")),
                (Some("new.key"), Some("Ignored note.")),
            ] {
                let notice = Notice {
                    kind,
                    surface: Surface::Command("test"),
                    subject: "test",
                    replacement,
                    note,
                };
                let line = notice_line(&notice);
                assert!(line.as_str().ends_with(&crate::lifecycle::lifecycle_reason(
                    kind,
                    replacement,
                    note
                )));
                let words: Vec<_> = line.wrapping_words().collect();
                assert!(words.len() > 1);
                if kind == NoticeKind::Deprecated && replacement == Some("gat system clean") {
                    assert!(words.contains(&"`gat system clean`"));
                }
            }
        }
    }

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
