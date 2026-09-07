//! Transient progress rendering. The concrete,
//! `indicatif`-backed implementation of [`crate::progress`]'s neutral
//! protocol: terminal styling, TTY detection, wording, and the actual
//! [`indicatif::ProgressBar`] lifecycle all live here. `output::progress`
//! owns wording and terminal rendering; [`crate::presentation::UserLine`]
//! owns single-line/control-character safety. `indicatif` receives only
//! the final approved text.
//! Commands never reference this module directly -- only
//! `crate::progress`'s `ProgressReporter`/`ProgressTask`/
//! `ProgressHandle`, constructed once at the application boundary via
//! [`for_environment`].
//!
//! ## Architectural rules
//!
//! - **`gat` owns semantics; `indicatif` owns rendering.**
//!   `crate::progress` decides *what* is happening and what counts as
//!   progress. `indicatif` decides how that is drawn: terminal width,
//!   message truncation, cursor movement, redraw throttling, resize
//!   handling, and clearing are all its responsibility. This module must
//!   never re-implement any of that: no terminal-width queries, no
//!   manual message truncation, no custom redraw loop, no renderer
//!   snapshot state.
//! - **One logical progress task creates exactly one `indicatif`
//!   progress entry.** There are no child progress tasks and no
//!   per-file/per-object rows. Concurrent workers contribute to the
//!   same logical task. This is a claim about entry *identity* only --
//!   how many physical terminal lines that one entry occupies at a
//!   given width, and how it wraps/reflows, is left entirely to
//!   `indicatif`.
//! - **Worker updates are cheap and decoupled from rendering.** `inc`/
//!   `set_activity` only update the shared `indicatif::ProgressBar`'s own
//!   counters/message; they never touch styles, tickers, or terminal
//!   state directly -- indicatif's own throttling governs how often
//!   anything is actually redrawn.
//! - **Transient output remains separate from durable command output**:
//!   progress is cleared before final outcomes/errors are
//!   rendered.
//!
//! `indicatif` stays confined to this module.

use crate::presentation::UserLine;
use crate::progress::{
    ActivityBackend, NoopProgress, ProgressActivity, ProgressOperation, ProgressReporter,
    ProgressSpec, ProgressTask, ProgressUnit,
};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

/// The only place a [`ProgressOperation`] variant's fixed label is
/// decided.
/// `crate::progress` itself has no wording method on this type.
fn operation_line(operation: ProgressOperation) -> UserLine {
    UserLine::authored(match operation {
        ProgressOperation::LoadingState => "loading tracked state",
        ProgressOperation::DiscoveringFiles => "discovering files",
        ProgressOperation::ResolvingSelection => "resolving selection",
        ProgressOperation::Hashing => "hashing",
        ProgressOperation::Fetching => "fetching",
        ProgressOperation::Pushing => "pushing",
        ProgressOperation::RemoteStatus => "checking remote",
        ProgressOperation::Synchronizing => "syncing",
        ProgressOperation::ApplyingChanges => "applying changes",
        ProgressOperation::InspectingCache => "inspecting cache",
        ProgressOperation::Repairing => "repairing",
        ProgressOperation::GarbageCollecting => "collecting garbage",
        ProgressOperation::ComputingReachability => "computing reachability",
        ProgressOperation::ListingRemoteObjects => "listing remote objects",
        ProgressOperation::SystemInspect => "inspecting",
        ProgressOperation::SystemRepair => "repairing",
        ProgressOperation::SystemClean => "cleaning",
        ProgressOperation::CloningSource => "cloning",
    })
}

/// The only place a [`ProgressUnit`] variant's rendered prefix is
/// decided, same reasoning as [`operation_line`].
fn unit_line(unit: ProgressUnit) -> UserLine {
    UserLine::authored(match unit {
        ProgressUnit::Files => "files",
        ProgressUnit::Objects => "objects",
        ProgressUnit::Entries => "entries",
        ProgressUnit::Domains => "domains",
    })
}

/// The only place any [`ProgressActivity`] variant's wording is decided
/// -- commands never construct this text themselves. Dynamic fields
/// (paths, patterns, percentages, an already-redacted URL) are composed
/// through [`UserLine`]'s own approved constructors, so a raw technical
/// value can never reach the terminal unsanitized.
pub(crate) fn activity_line(activity: &ProgressActivity) -> UserLine {
    match activity {
        ProgressActivity::ReshapingLock => UserLine::authored("reshaping gat.lock"),
        ProgressActivity::StagingRows => UserLine::authored("staging matched rows"),
        ProgressActivity::ReplayingRows => UserLine::authored("replaying staged rows"),
        ProgressActivity::ObservingLockState => UserLine::authored("observing on-disk lock state"),
        ProgressActivity::RefreshingDesiredState => UserLine::authored("refreshing desired state"),
        ProgressActivity::RecoveringInterruptedMount => {
            UserLine::authored("recovering interrupted mount")
        }
        ProgressActivity::PublishingConfig => UserLine::authored("publishing config"),
        ProgressActivity::DeletingOwnedRows => UserLine::authored("deleting owned rows"),
        ProgressActivity::DeletingRemoteObjects => UserLine::authored("deleting remote objects"),
        ProgressActivity::OpeningMaterializedState => {
            UserLine::authored("opening materialized state")
        }
        ProgressActivity::PublishingDesiredState => UserLine::authored("publishing desired state"),
        ProgressActivity::RecordingMaterializedState => {
            UserLine::authored("recording materialized state")
        }
        ProgressActivity::RegeneratingExcludes => UserLine::authored("regenerating excludes"),
        ProgressActivity::CheckingReuseStatus => UserLine::authored("checking reuse status"),
        ProgressActivity::HashingFile { path, percent } => match percent {
            Some(percent) => UserLine::compose([
                UserLine::path_text(path.as_str()),
                UserLine::authored(" ("),
                UserLine::number(i64::from(*percent)),
                UserLine::authored("%)"),
            ]),
            None => UserLine::path_text(path.as_str()),
        },
        ProgressActivity::WalkingDirectory => UserLine::authored("walking directory"),
        ProgressActivity::ExpandingPattern { pattern } => UserLine::compose([
            UserLine::authored("expanding pattern "),
            UserLine::identifier(pattern),
        ]),
        ProgressActivity::Connecting => UserLine::authored("connecting"),
        ProgressActivity::ResolvingSelection => UserLine::authored("resolving selection"),
        ProgressActivity::LoadingState => UserLine::authored("loading state"),
        ProgressActivity::CheckingRemote => UserLine::authored("checking remote"),
        ProgressActivity::CheckedRemoteObject { path } => UserLine::path_text(path.as_str()),
        ProgressActivity::ClassifyingSelectors => UserLine::authored("classifying selectors"),
        ProgressActivity::ScanningDesiredState => UserLine::authored("scanning desired state"),
        ProgressActivity::ScanningLock => UserLine::authored("scanning gat.lock"),
        ProgressActivity::MatchingSourcePath => UserLine::authored("matching source path"),
        ProgressActivity::CheckingDestination => UserLine::authored("checking destination"),
        ProgressActivity::CloningSource { location } => UserLine::redacted_url(
            &crate::redaction::RedactedUrl::render(location.as_location_str()),
        ),
        ProgressActivity::TransferringFile { path } => UserLine::path_text(path.as_str()),
        ProgressActivity::LoadingMaterializedState => {
            UserLine::authored("loading materialized state")
        }
        ProgressActivity::LoadingTrackedState => UserLine::authored("loading tracked state"),
        ProgressActivity::ValidatingWorkingTree => UserLine::authored("validating working tree"),
        ProgressActivity::PlanningChanges => UserLine::authored("planning changes"),
        ProgressActivity::ApplyingChanges => UserLine::authored("applying changes"),
    }
}

/// Compose the message indicatif renders: the operation's fixed label,
/// plus an optional activity suffix (e.g. the path currently being
/// hashed/fetched, or a status like "checking remote"). `activity`
/// accepts only approved [`UserLine`] values (or `None`) -- this
/// function performs no sanitization of its own and cannot accept a raw
/// string; that guarantee comes entirely from [`operation_line`]/
/// [`activity_line`] having already produced a [`UserLine`] before this
/// is ever called. The conversion to `String` happens only at the
/// `indicatif::ProgressBar::set_message`/`set_prefix` boundary, which is
/// the one place `indicatif`'s API requires an owned string.
fn compose_message(operation: ProgressOperation, activity: Option<UserLine>) -> UserLine {
    let operation_line = operation_line(operation);
    match activity {
        Some(activity) if activity == operation_line => operation_line,
        Some(activity) => UserLine::compose([operation_line, UserLine::authored(" · "), activity]),
        None => operation_line,
    }
}

/// Renderer classification derived purely from a task's unit/total --
/// never from [`ProgressOperation`] identity. Exactly the three families
/// the issue requires: indeterminate, open-ended counted, and
/// determinate counted. There is no separate "byte-active" class: byte
/// throughput is not currently rendered by this module (see the module
/// docs), so classification stays governed only by item count shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderClass {
    Indeterminate,
    OpenEndedItems(ProgressUnit),
    DeterminateItems(ProgressUnit),
}

const fn classify(unit: Option<ProgressUnit>, total: Option<u64>) -> RenderClass {
    match (unit, total) {
        (Some(unit), Some(_)) => RenderClass::DeterminateItems(unit),
        (Some(unit), None) => RenderClass::OpenEndedItems(unit),
        (None, _) => RenderClass::Indeterminate,
    }
}

/// Every style uses `{wide_msg}` (never combined with `{wide_bar}`) so
/// indicatif itself computes remaining width and truncates the message
/// to fit -- this module performs no terminal-width math of its own.
fn style_for(class: RenderClass) -> ProgressStyle {
    let template = match class {
        RenderClass::Indeterminate => "{spinner:.cyan} {wide_msg}".to_string(),
        RenderClass::OpenEndedItems(_) => "{spinner:.cyan} {pos} {prefix} · {wide_msg}".to_string(),
        RenderClass::DeterminateItems(_) => "{pos}/{len} {prefix} · {wide_msg}".to_string(),
    };
    ProgressStyle::with_template(&template).unwrap_or_else(|_| ProgressStyle::default_spinner())
}

/// Whether `class` renders as a bar with a known length (true
/// `pos/total`) rather than an animated spinner -- drives whether a
/// steady tick is enabled (determinate tasks don't need one).
const fn is_determinate(class: RenderClass) -> bool {
    matches!(class, RenderClass::DeterminateItems(_))
}

/// A cadence modest enough to be imperceptible as flicker but frequent
/// enough to feel live -- indicatif enforces this on our behalf via
/// `enable_steady_tick`, so no per-update redraw ever happens here.
const STEADY_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// The concrete backend for one logical task rendered by `indicatif`:
/// the bar itself (already cheap to clone -- it is internally
/// reference-counted), plus the task's operation so a worker's
/// `set_activity` update can recompose the right label without needing
/// anything else. Implements [`ActivityBackend`], the narrow protocol
/// contract [`ProgressTask`]/[`ProgressHandle`] depend on, so `crate::
/// progress` never needs to name this type directly.
#[derive(Clone)]
struct TerminalTask {
    bar: ProgressBar,
    operation: ProgressOperation,
}

impl TerminalTask {
    fn begin(spec: ProgressSpec) -> Self {
        let bar = ProgressBar::new_spinner();
        let class = classify(spec.unit(), spec.total());
        bar.set_style(style_for(class));
        if let Some(unit) = spec.unit() {
            bar.set_prefix(unit_line(unit).as_str().to_string());
        }
        bar.set_message(compose_message(spec.operation(), None).as_str().to_owned());
        if let Some(total) = spec.total() {
            bar.set_length(total);
        }
        if !is_determinate(class) {
            bar.enable_steady_tick(STEADY_TICK_INTERVAL);
        }
        Self {
            bar,
            operation: spec.operation(),
        }
    }
}

impl ActivityBackend for TerminalTask {
    /// Cheap, thread-safe item counter update -- forwards straight to
    /// the shared bar; indicatif's own throttling decides whether this
    /// actually triggers a redraw.
    fn inc(&self, delta: u64) {
        self.bar.inc(delta);
    }

    /// Update only the semantic activity text; never touches styles or
    /// tickers.
    fn set_activity(&self, activity: &ProgressActivity) {
        let message = compose_message(self.operation, Some(activity_line(activity)));
        self.bar.set_message(message.as_str().to_owned());
    }

    fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

/// The real terminal implementation. Every command in this codebase runs
/// its logical progress tasks strictly sequentially (verified by
/// auditing every `begin` call site), so a single un-multiplexed
/// `indicatif::ProgressBar` per task -- drawing to the default stderr
/// draw target -- is sufficient; there is no genuine case of two
/// concurrently visible logical rows to justify `MultiProgress`'s
/// bookkeeping. `finish_all` still needs to reach every bar that might
/// still be genuinely open (e.g. after an early error), so this keeps a
/// small registry of currently/recently active bars -- pruned of
/// already-finished entries on every `begin`, so it stays bounded under
/// a command that opens many sequential short-lived tasks rather than
/// silently retaining every bar ever created for the lifetime of the
/// command.
#[derive(Default)]
pub struct TerminalProgress {
    active: Mutex<Vec<ProgressBar>>,
}

impl TerminalProgress {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Test-only introspection: how many bars the registry is currently
    /// holding onto, *before* any pruning a subsequent `begin()` would
    /// perform -- lets a test prove the registry stays bounded rather
    /// than growing with the total number of tasks a command opens.
    #[cfg(test)]
    fn registry_len(&self) -> usize {
        self.active.lock().unwrap().len()
    }
}

impl ProgressReporter for TerminalProgress {
    fn begin(&self, spec: ProgressSpec) -> ProgressTask {
        let task = TerminalTask::begin(spec);
        let mut active = self.active.lock().unwrap();
        active.retain(|bar| !bar.is_finished());
        active.push(task.bar.clone());
        drop(active);
        ProgressTask::from_backend(Arc::new(task))
    }

    fn finish_all(&self) {
        // `finish_and_clear` is idempotent (a task that already finished
        // normally simply redraws its already-cleared state), so no
        // bookkeeping is needed to skip bars that finished earlier.
        for bar in self.active.lock().unwrap().drain(..) {
            bar.finish_and_clear();
        }
    }
}

/// Settings that decide whether progress should be drawn at all --
/// deliberately just the handful of signals requirement 5 lists, checked
/// once, here, rather than every command re-deriving them.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProgressOptions {
    /// Running as a Git hook (`gat hook ...`): must stay silent so hook
    /// output doesn't corrupt Git's own UI.
    pub hook_mode: bool,
    /// `--quiet`/machine-readable output requested (not implemented on
    /// the CLI today, but the policy point lives here so adding either
    /// later doesn't need new TTY-detection code).
    pub quiet: bool,
}

/// Build the reporter to use for this invocation: a real terminal
/// reporter only when stderr is a tty and nothing above suppresses it,
/// a no-op otherwise. The one place TTY detection and progress
/// suppression policy live -- commands and `app::run` never re-derive
/// this themselves.
#[must_use]
pub fn for_environment(options: ProgressOptions) -> Box<dyn ProgressReporter> {
    if should_suppress(options, stderr_is_tty()) {
        Box::new(NoopProgress)
    } else {
        Box::new(TerminalProgress::new())
    }
}

/// Pure suppression policy, split out from [`for_environment`] so it can
/// be tested directly against synthetic inputs without controlling a
/// real stderr tty: hook mode, quiet mode, or a non-tty stderr each
/// independently force full suppression.
const fn should_suppress(options: ProgressOptions, stderr_is_tty: bool) -> bool {
    options.hook_mode || options.quiet || !stderr_is_tty
}

fn stderr_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;
    use std::sync::Mutex;

    /// A VT100 terminal emulator (backed directly by the `vt100` crate
    /// -- the same crate indicatif's own `in_memory` feature wraps for
    /// its `InMemoryTerm` test helper) used as an `indicatif::TermLike`
    /// implementation in tests. Cursor movement, line clearing, and
    /// character-level line wrapping are handled by the `vt100` parser
    /// rather than reimplemented here, so a test asking "what does the
    /// whole screen currently look like" reflects that parser's model
    /// of VT100 semantics. This is a smoke test against one real
    /// escape-sequence interpreter, not a claim that every real
    /// terminal emulator's historical reflow behavior is reproduced
    /// exactly.
    ///
    /// Unlike `indicatif::InMemoryTerm` (whose size is fixed at
    /// construction), this wrapper exposes [`FakeTerminal::resize`] so a
    /// test can shrink/expand the same live terminal mid-scenario --
    /// exactly what is needed to reproduce and guard against the
    /// original stale-row-on-resize failure mode.
    #[derive(Clone)]
    pub struct FakeTerminal {
        parser: Arc<Mutex<vt100::Parser>>,
    }

    impl std::fmt::Debug for FakeTerminal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FakeTerminal").finish_non_exhaustive()
        }
    }

    impl FakeTerminal {
        pub(crate) fn new(width: u16) -> Self {
            Self {
                parser: Arc::new(Mutex::new(vt100::Parser::new(1000, width, 0))),
            }
        }

        /// Resize the live terminal in place -- the fake terminal's
        /// screen contents and cursor position are preserved across
        /// this call, modeling the same live-resize scenario a real
        /// terminal emulator faces on `SIGWINCH`, which is what this
        /// module's resize smoke tests need to exercise.
        pub(crate) fn resize(&self, width: u16) {
            let mut parser = self.parser.lock().unwrap();
            let rows = parser.screen().size().0;
            parser.screen_mut().set_size(rows, width);
        }

        /// The full current screen, top to bottom, as trimmed strings --
        /// what this VT100 parser's screen model currently holds.
        pub(crate) fn screen_lines(&self) -> Vec<String> {
            let parser = self.parser.lock().unwrap();
            let (_rows, cols) = parser.screen().size();
            parser
                .screen()
                .rows(0, cols)
                .map(|line| line.trim_end().to_string())
                .collect()
        }

        /// Only the non-blank rows currently on screen, in order -- the
        /// common case for tests that don't care about the trailing
        /// blank rows below the current frame.
        pub(crate) fn lines(&self) -> Vec<String> {
            self.screen_lines()
                .into_iter()
                .filter(|line| !line.is_empty())
                .collect()
        }

        /// How many screen rows currently contain any non-blank content
        /// -- the key assertion for "no stale rows left behind" after a
        /// resize: exactly the number of rows the current logical frame
        /// occupies, never more.
        pub(crate) fn non_blank_row_count(&self) -> usize {
            self.lines().len()
        }
    }

    impl indicatif::TermLike for FakeTerminal {
        fn width(&self) -> u16 {
            self.parser.lock().unwrap().screen().size().1
        }

        fn height(&self) -> u16 {
            self.parser.lock().unwrap().screen().size().0
        }

        fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
            if n == 0 {
                return Ok(());
            }
            self.parser
                .lock()
                .unwrap()
                .process(format!("\x1b[{n}A").as_bytes());
            Ok(())
        }

        fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
            if n == 0 {
                return Ok(());
            }
            self.parser
                .lock()
                .unwrap()
                .process(format!("\x1b[{n}B").as_bytes());
            Ok(())
        }

        fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
            if n == 0 {
                return Ok(());
            }
            self.parser
                .lock()
                .unwrap()
                .process(format!("\x1b[{n}C").as_bytes());
            Ok(())
        }

        fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
            if n == 0 {
                return Ok(());
            }
            self.parser
                .lock()
                .unwrap()
                .process(format!("\x1b[{n}D").as_bytes());
            Ok(())
        }

        fn write_line(&self, s: &str) -> std::io::Result<()> {
            self.write_str(s)?;
            self.parser.lock().unwrap().process(b"\r\n");
            Ok(())
        }

        fn write_str(&self, s: &str) -> std::io::Result<()> {
            self.parser.lock().unwrap().process(s.as_bytes());
            Ok(())
        }

        fn clear_line(&self) -> std::io::Result<()> {
            self.parser.lock().unwrap().process(b"\r\x1b[2K");
            Ok(())
        }

        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn remote_gc_deletion_has_an_explicit_activity_label() {
        let message = super::compose_message(
            gat_core::progress::ProgressOperation::GarbageCollecting,
            Some(super::activity_line(
                &gat_core::progress::ProgressActivity::DeletingRemoteObjects,
            )),
        );
        assert!(message.as_str().contains("deleting remote objects"));
        assert!(!message.as_str().contains("scanning objects"));
    }
    use super::test_support::FakeTerminal;
    use super::*;
    use indicatif::ProgressDrawTarget;

    #[test]
    fn remote_status_activities_keep_their_renderer_owned_wording() {
        assert_eq!(
            activity_line(&ProgressActivity::Connecting).as_str(),
            "connecting"
        );
        assert_eq!(
            activity_line(&ProgressActivity::ResolvingSelection).as_str(),
            "resolving selection"
        );
        assert_eq!(
            activity_line(&ProgressActivity::CheckingRemote).as_str(),
            "checking remote"
        );
        assert_eq!(
            activity_line(&ProgressActivity::CheckedRemoteObject {
                path: gat_core::lexical_path::GatPath::normalize("models/checkpoint.bin").unwrap(),
            })
            .as_str(),
            "models/checkpoint.bin"
        );
    }

    #[test]
    fn activity_line_escapes_control_characters_instead_of_collapsing_them() {
        // `UserLine`'s construction-time escaping is the single
        // place control characters are neutralized. `HashingFile`'s `path` is a
        // `GatPath` now, which structurally rejects control characters
        // at construction time (see `gat_core::lexical_path`), so this
        // test exercises the escaping contract through
        // `ExpandingPattern`'s still-free-text `pattern` field instead.
        let line = activity_line(&ProgressActivity::ExpandingPattern {
            pattern: "line one\nline two\r\ttabbed".to_string(),
        });
        assert!(!line.as_str().contains('\n'));
        assert!(!line.as_str().contains('\r'));
        assert!(!line.as_str().contains('\t'));
    }

    #[test]
    fn cloning_source_activity_line_never_reaches_the_original_credential_bearing_url() {
        // A credential-bearing clone URL must never reach the rendered
        // activity line unredacted. `activity_line` itself performs the
        // redaction (via `RedactedUrl::render`) from the neutral,
        // unvalidated `GitLocationSpec` the protocol carries -- built
        // via format! (not a literal) so the synthetic, never-dialed
        // test secret below isn't mistaken for a real credential.
        let secret_marker = "s3cr3t-token";
        // hygiene-ok: synthetic never-dialed test URL with a fake secret.
        let raw = format!("https://alice:{secret_marker}@example.com/repo.git");
        let location = gat_core::git_location::GitLocationSpec::from_string(raw);
        let line = activity_line(&ProgressActivity::CloningSource { location });
        assert!(!line.as_str().contains(secret_marker));
        assert!(!line.as_str().contains("alice:"));
    }

    #[test]
    fn compose_message_preserves_full_text_regardless_of_length() {
        // Width-dependent truncation is indicatif's job now; composing
        // must never shorten a message on its own.
        let raw = "a".repeat(500);
        assert_eq!(
            compose_message(
                ProgressOperation::Hashing,
                Some(UserLine::identifier_for_test(&raw))
            )
            .as_str(),
            {
                let mut expected = operation_line(ProgressOperation::Hashing)
                    .as_str()
                    .to_string();
                expected.push_str(" · ");
                expected.push_str(&raw);
                expected
            }
        );
    }

    #[test]
    fn compose_message_uses_operation_label_alone_when_no_activity() {
        assert_eq!(
            compose_message(ProgressOperation::Repairing, None).as_str(),
            "repairing"
        );
    }

    #[test]
    fn compose_message_deduplicates_activity_matching_the_operation_label() {
        assert_eq!(
            compose_message(
                ProgressOperation::RemoteStatus,
                Some(activity_line(&ProgressActivity::CheckingRemote)),
            )
            .as_str(),
            "checking remote"
        );
    }

    #[test]
    fn compose_message_appends_activity_after_the_operation_label() {
        assert_eq!(
            compose_message(
                ProgressOperation::Fetching,
                Some(UserLine::identifier_for_test("models/checkpoint.bin"))
            )
            .as_str(),
            "fetching · models/checkpoint.bin"
        );
    }

    #[test]
    fn composed_progress_message_preserves_user_line_boundary() {
        let activity = UserLine::identifier_for_test("path\ninjected");
        let line = compose_message(ProgressOperation::Hashing, Some(activity));

        assert!(!line.as_str().contains('\n'));
        assert!(line.as_str().contains("\\n"));
    }

    #[test]
    fn classify_reports_indeterminate_with_no_unit() {
        assert_eq!(classify(None, None), RenderClass::Indeterminate);
    }

    #[test]
    fn classify_reports_open_ended_items_with_a_unit_and_no_total() {
        assert_eq!(
            classify(Some(ProgressUnit::Files), None),
            RenderClass::OpenEndedItems(ProgressUnit::Files)
        );
    }

    #[test]
    fn classify_reports_determinate_items_with_a_unit_and_a_total() {
        assert_eq!(
            classify(Some(ProgressUnit::Objects), Some(10)),
            RenderClass::DeterminateItems(ProgressUnit::Objects)
        );
    }

    #[test]
    fn is_determinate_is_true_only_for_determinate_items() {
        assert!(is_determinate(RenderClass::DeterminateItems(
            ProgressUnit::Files
        )));
        assert!(!is_determinate(RenderClass::Indeterminate));
        assert!(!is_determinate(RenderClass::OpenEndedItems(
            ProgressUnit::Files
        )));
    }

    #[test]
    fn should_suppress_when_hook_mode_regardless_of_tty() {
        let options = ProgressOptions {
            hook_mode: true,
            quiet: false,
        };
        assert!(should_suppress(options, true));
        assert!(should_suppress(options, false));
    }

    #[test]
    fn should_suppress_when_quiet_regardless_of_tty() {
        let options = ProgressOptions {
            hook_mode: false,
            quiet: true,
        };
        assert!(should_suppress(options, true));
        assert!(should_suppress(options, false));
    }

    #[test]
    fn should_suppress_when_stderr_is_not_a_tty_even_with_no_other_flags() {
        let options = ProgressOptions {
            hook_mode: false,
            quiet: false,
        };
        assert!(should_suppress(options, false));
    }

    #[test]
    fn should_not_suppress_only_when_a_tty_and_neither_hook_nor_quiet() {
        let options = ProgressOptions {
            hook_mode: false,
            quiet: false,
        };
        assert!(!should_suppress(options, true));
    }

    #[test]
    fn noop_progress_finish_all_is_a_harmless_no_op() {
        // `finish_all` must exist and do nothing observable on the
        // suppressed path -- suppressed contexts (hooks, quiet, non-tty)
        // never touch terminal state at all.
        NoopProgress.finish_all();
    }

    #[test]
    fn terminal_progress_registry_stays_bounded_across_many_sequential_tasks() {
        // A command with many sequential phases (or a loop that begins a
        // fresh short-lived task many times) must not leave the registry
        // growing without bound: `begin()` prunes already-finished bars
        // before registering the new one, so the registry never holds
        // more than the handful of tasks that are genuinely still open
        // at any instant -- never proportional to the total number of
        // tasks ever begun over the command's lifetime.
        let progress = TerminalProgress::new();
        for _ in 0..500 {
            let task = progress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
            task.finish();
            assert!(
                progress.registry_len() <= 1,
                "registry must prune finished bars, not accumulate them: {}",
                progress.registry_len()
            );
        }
        // `finish_all` remains a harmless, idempotent safety net even
        // after every task has already been finished individually.
        progress.finish_all();
        progress.finish_all();
        assert_eq!(progress.registry_len(), 0);
    }

    /// Draw a real `indicatif::ProgressBar` (the actual renderer, not a
    /// stand-in) against a [`FakeTerminal`], across a resize sequence
    /// (including a wide -> narrow -> wide round trip), and confirm
    /// every emitted line stays within the terminal's width *at the time
    /// it was written*. This exercises indicatif's own resize/reflow
    /// handling directly rather than re-testing a homemade width
    /// calculation this module deliberately does not have.
    #[test]
    fn terminal_progress_bar_never_emits_a_line_wider_than_the_current_terminal() {
        let long_message =
            "vendor/models/checkpoints/very/deeply/nested/large-checkpoint-shard-0007.bin";
        let term = FakeTerminal::new(120);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(Some(52), draw_target);
        bar.set_style(style_for(RenderClass::DeterminateItems(
            ProgressUnit::Files,
        )));
        bar.set_prefix(unit_line(ProgressUnit::Files).as_str().to_string());
        bar.set_message(
            compose_message(
                ProgressOperation::Hashing,
                Some(UserLine::identifier_for_test(long_message)),
            )
            .as_str()
            .to_owned(),
        );

        for width in [120u16, 48, 100, 32, 120] {
            term.resize(width);
            bar.set_position(bar.position() + 1);
            bar.tick();
            // Only the visible (non-whitespace) content of each drawn
            // line needs to fit within the current width -- indicatif
            // may also emit trailing blank padding to erase a
            // wider preceding line, which is exactly the correct
            // clearing behavior this test relies on indicatif to
            // provide, not a violation of it.
            for line in term.lines() {
                let visible = line.trim_end();
                if visible.is_empty() {
                    continue;
                }
                let display_width = console::measure_text_width(visible);
                assert!(
                    display_width <= width as usize,
                    "line {line:?} (display width {display_width}) exceeds terminal width {width}"
                );
            }
        }
        bar.finish_and_clear();
    }

    /// Same fake-terminal harness, but for a task whose message contains
    /// wide (CJK) and combining Unicode characters -- proving width is
    /// measured in terminal display columns, not `char` count, since a
    /// naive `char`-count-based truncation would under- or over-shoot
    /// the real terminal width for this text.
    #[test]
    fn terminal_progress_bar_handles_wide_and_combining_unicode_without_overflowing_width() {
        let message = "圧縮中 checkpoint-\u{0301}shard.bin"; // combining acute accent
        let term = FakeTerminal::new(24);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(None, draw_target);
        bar.set_style(style_for(RenderClass::Indeterminate));
        bar.set_message(
            compose_message(
                ProgressOperation::Fetching,
                Some(UserLine::identifier_for_test(message)),
            )
            .as_str()
            .to_owned(),
        );
        bar.tick();
        for line in term.lines() {
            assert!(console::measure_text_width(&line) <= 24);
        }
        bar.finish_and_clear();
    }

    /// Assert the whole persistent screen -- not just the strings a
    /// single write happened to contain -- has exactly one non-blank
    /// row after `bar` redraws at `width`, proving no fragment of a
    /// previous (wider or narrower) frame survives the redraw.
    fn assert_screen_shows_exactly_one_row(term: &FakeTerminal, width: u16) {
        let non_blank = term.non_blank_row_count();
        assert_eq!(
            non_blank,
            1,
            "expected exactly one visible progress row at width {width}, screen was {:?}",
            term.screen_lines()
        );
    }

    /// Shrinking a terminal that already has a wide progress row drawn
    /// on it must not leave fragments of that wider line on screen once
    /// the next redraw happens at the narrower width, and widening it
    /// back afterwards must not leave stale narrow-frame fragments
    /// either -- indicatif's own clear-then-redraw sequence (driven
    /// through this fake's persistent screen model) is what is being
    /// exercised here, not a bespoke clearing implementation of this
    /// module's own.
    #[test]
    fn resize_regression_clears_stale_rows_across_shrink_and_expand() {
        let long_message =
            "vendor/models/checkpoints/very/deeply/nested/large-checkpoint-shard-0007.bin";
        let term = FakeTerminal::new(120);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(None, draw_target);
        bar.set_style(style_for(RenderClass::Indeterminate));
        bar.set_message(
            compose_message(
                ProgressOperation::Hashing,
                Some(UserLine::identifier_for_test(long_message)),
            )
            .as_str()
            .to_owned(),
        );
        bar.tick();
        assert_screen_shows_exactly_one_row(&term, 120);

        // Shrink drastically while the same logical task is still
        // active, then force a redraw.
        term.resize(48);
        bar.tick();
        assert_screen_shows_exactly_one_row(&term, 48);

        // Widen again; nothing from the narrow frame should linger.
        term.resize(120);
        bar.tick();
        assert_screen_shows_exactly_one_row(&term, 120);

        bar.finish_and_clear();
        assert_eq!(
            term.non_blank_row_count(),
            0,
            "finishing must clear the full current frame"
        );
    }

    /// Repeated shrink/expand cycles (as a user resizing their terminal
    /// several times while one command runs would trigger) must never
    /// accumulate duplicated rows -- the screen must always converge
    /// back to exactly one visible row after each redraw, regardless of
    /// how many cycles have already happened.
    #[test]
    fn resize_regression_repeated_cycles_do_not_accumulate_duplicate_rows() {
        let message = "reticulating splines across a fairly long activity description";
        let term = FakeTerminal::new(120);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(Some(200), draw_target);
        bar.set_style(style_for(RenderClass::DeterminateItems(
            ProgressUnit::Objects,
        )));
        bar.set_prefix(unit_line(ProgressUnit::Objects).as_str().to_string());
        bar.set_message(
            compose_message(
                ProgressOperation::Fetching,
                Some(UserLine::identifier_for_test(message)),
            )
            .as_str()
            .to_owned(),
        );

        for width in [120u16, 48, 100, 32, 120, 48, 100, 32, 120] {
            term.resize(width);
            bar.set_position(bar.position() + 1);
            bar.tick();
            assert_screen_shows_exactly_one_row(&term, width);
        }
        bar.finish_and_clear();
    }

    /// Same resize sequence, but with a determinate (`pos`/`len`) bar
    /// active throughout -- proving the fixed `pos`/`len`/prefix fields
    /// do not defeat clearing at any width.
    #[test]
    fn resize_regression_with_determinate_bar_active() {
        let term = FakeTerminal::new(100);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(Some(52), draw_target);
        bar.set_style(style_for(RenderClass::DeterminateItems(
            ProgressUnit::Files,
        )));
        bar.set_prefix(unit_line(ProgressUnit::Files).as_str().to_string());
        bar.set_message(
            compose_message(
                ProgressOperation::Hashing,
                Some(UserLine::identifier_for_test("src/lib.rs")),
            )
            .as_str()
            .to_owned(),
        );

        for width in [100u16, 40, 80, 20, 100] {
            term.resize(width);
            bar.set_position(bar.position() + 1);
            bar.tick();
            assert_screen_shows_exactly_one_row(&term, width);
        }
        bar.finish_and_clear();
    }

    /// Same resize sequence for an open-ended item task (no known
    /// total), which renders with `{pos} {prefix}` rather than
    /// `{pos}/{len}`.
    #[test]
    fn resize_regression_with_open_ended_item_task_active() {
        let term = FakeTerminal::new(100);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(None, draw_target);
        bar.set_style(style_for(RenderClass::OpenEndedItems(
            ProgressUnit::Objects,
        )));
        bar.set_prefix(unit_line(ProgressUnit::Objects).as_str().to_string());
        bar.set_message(
            compose_message(
                ProgressOperation::Fetching,
                Some(UserLine::identifier_for_test("checking remote")),
            )
            .as_str()
            .to_owned(),
        );

        for width in [100u16, 40, 80, 20, 100] {
            term.resize(width);
            bar.set_position(bar.position() + 1);
            bar.tick();
            assert_screen_shows_exactly_one_row(&term, width);
        }
        bar.finish_and_clear();
    }

    /// A very long activity message (e.g. a deeply nested path) must
    /// still clear cleanly across a resize even though it wraps onto
    /// multiple physical rows at narrow widths.
    #[test]
    fn resize_regression_with_long_activity_message() {
        let message = "a/very/deeply/nested/directory/structure/that/goes/on/for/quite/a/while/checkpoint-shard-final-0099.safetensors";
        let term = FakeTerminal::new(120);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(None, draw_target);
        bar.set_style(style_for(RenderClass::Indeterminate));
        bar.set_message(
            compose_message(
                ProgressOperation::Hashing,
                Some(UserLine::identifier_for_test(message)),
            )
            .as_str()
            .to_owned(),
        );

        for width in [120u16, 30, 60, 120] {
            term.resize(width);
            bar.tick();
            // A long message can legitimately occupy more than one
            // physical row once wrapped -- the property under test is
            // that every visible row still fits the current width and
            // that no extra stale rows accumulate beyond what the
            // current frame needs.
            for line in term.screen_lines() {
                if line.is_empty() {
                    continue;
                }
                assert!(
                    console::measure_text_width(&line) <= width as usize,
                    "row {line:?} exceeds width {width}"
                );
            }
        }
        bar.finish_and_clear();
    }

    /// Wide (CJK) Unicode characters must not be miscounted as
    /// single-column during resize, which would either overflow the
    /// terminal or leave a stale column behind.
    #[test]
    fn resize_regression_with_wide_unicode_characters() {
        let message = "圧縮中のファイル名は非常に長いパスを含んでいます.bin";
        let term = FakeTerminal::new(60);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(None, draw_target);
        bar.set_style(style_for(RenderClass::Indeterminate));
        bar.set_message(
            compose_message(
                ProgressOperation::Fetching,
                Some(UserLine::identifier_for_test(message)),
            )
            .as_str()
            .to_owned(),
        );

        for width in [60u16, 24, 40, 60] {
            term.resize(width);
            bar.tick();
            for line in term.screen_lines() {
                if line.is_empty() {
                    continue;
                }
                assert!(console::measure_text_width(&line) <= width as usize);
            }
        }
        bar.finish_and_clear();
    }

    /// Combining characters must not be counted as occupying their own
    /// column, which would otherwise make the reported display width
    /// wrong at exactly the boundary a resize needs to get right.
    #[test]
    fn resize_regression_with_combining_characters() {
        let message = "checkpoint-\u{0301}shard-\u{0301}final.bin"; // combining acute accents
        let term = FakeTerminal::new(24);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(None, draw_target);
        bar.set_style(style_for(RenderClass::Indeterminate));
        bar.set_message(
            compose_message(
                ProgressOperation::Hashing,
                Some(UserLine::identifier_for_test(message)),
            )
            .as_str()
            .to_owned(),
        );

        for width in [24u16, 16, 24] {
            term.resize(width);
            bar.tick();
            for line in term.screen_lines() {
                if line.is_empty() {
                    continue;
                }
                assert!(console::measure_text_width(&line) <= width as usize);
            }
        }
        bar.finish_and_clear();
    }

    /// Embedded control characters (newlines, carriage returns, tabs)
    /// must never make one logical row occupy more than one physical
    /// terminal row -- `UserLine`'s own construction-time escaping (via
    /// `activity_line`) neutralizes them before `set_message` ever
    /// reaches indicatif, so this proves that escaping actually
    /// prevents the extra-row failure mode end-to-end, not just that
    /// `activity_line` returns a control-free string in isolation.
    #[test]
    fn resize_regression_control_characters_cannot_create_extra_rows() {
        let term = FakeTerminal::new(80);
        let draw_target = ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), 255);
        let bar = ProgressBar::with_draw_target(None, draw_target);
        let task = TerminalTask {
            bar: bar.clone(),
            operation: ProgressOperation::Hashing,
        };
        bar.set_style(style_for(RenderClass::Indeterminate));
        task.set_activity(&ProgressActivity::ExpandingPattern {
            pattern: "line one\nline two\r\nline three\ttabbed".to_string(),
        });
        bar.tick();
        assert_screen_shows_exactly_one_row(&term, 80);
        bar.finish_and_clear();
    }
}
