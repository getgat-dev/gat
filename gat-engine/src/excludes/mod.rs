//! Managed Git exclusions derived from desired paths and explicit ignore
//! patterns. Sync and mutations maintain one marked `.git/info/exclude`
//! block; unchanged inputs and a valid output proof avoid regeneration.

use crate::repository::Repository as Repo;
use gat_core::lock::Lock;
use gat_io::LockStore;
use gat_io::StateStore;
#[cfg(test)]
use std::path::Path;

mod compact;
mod error;

pub use error::ExcludesError;
use error::Result;

const BEGIN: &str = "# >>> gat >>>";
const END: &str = "# <<< gat <<<";

/// Whether `sync` changed `.git/info/exclude`, and how many paths are now
/// managed there, for a `uv sync`-style status line after every command.
#[derive(Debug)]
pub struct SyncStatus {
    pub changed: bool,
    pub count: usize,
}

/// Regenerate the gat-managed block in `.git/info/exclude` from the
/// on-disk `gat.lock` and `git.ignore_patterns` (see
/// current Git exclude policy). By default every `gat.lock` entry gets its
/// own exact rule except paths containing LF, which Git cannot express
/// exactly and which are recorded as comments instead;
/// a `git.ignore_patterns` entry (opt-in, hand-authored) can cover a
/// whole directory or glob at once, in which case any tracked file it
/// already covers doesn't also get a redundant exact rule. Anything
/// outside the marker lines is left untouched. No-op write if nothing
/// changed. Pass `dry_run: true` (from `gat sync --dry-run`) to compute
/// and report what would change without touching `.git/info/exclude` at
/// all.
pub fn sync(repo: &Repo, dry_run: bool) -> Result<SyncStatus> {
    // Justified full read: the managed exclude block names every
    // tracked path, so this operation's *result* is the full desired state.
    // Callers that already hold a refreshed mirror use `sync_from_store`,
    // which streams the same rows out of SQLite instead.
    let lock = LockStore::load_repository(repo.layout())?;
    sync_from_lock(repo, &lock, dry_run)
}

/// Combine a desired-lock-set fingerprint (see
/// [`gat_io::StateStore::desired_fingerprint`])
/// with `git.ignore_patterns` into the single fingerprint that decides
/// whether the gat-managed `.git/info/exclude` block needs regenerating.
/// Any input that can change what [`sync_from_lock`] would write (the
/// desired lock set's content identity, or the configured exclude
/// patterns) must be folded in here so an unchanged fingerprint really
/// does mean an unchanged managed block.
pub(crate) fn fingerprint(
    desired_fingerprint: &[u8; 32],
    ignore_patterns: &[gat_core::git_ignore::GitIgnorePattern],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gat-excludes-v3\0");
    hasher.update(desired_fingerprint);
    for pattern in ignore_patterns {
        hasher.update(pattern.as_str().as_bytes());
        hasher.update(b"\0");
    }
    *hasher.finalize().as_bytes()
}

/// Regenerate the gat-managed block in `.git/info/exclude` from an already
/// loaded `gat.lock`, avoiding a second lock-file read when the caller
/// already has the desired state in memory.
pub fn sync_from_lock(repo: &Repo, lock: &Lock, dry_run: bool) -> Result<SyncStatus> {
    let cfg = repo.load_config().map_err(Box::new)?;
    sync_from_lock_with_config(repo, lock, dry_run, &cfg)
}

/// As [`sync_from_lock`], but reuses an already-loaded effective config
/// snapshot instead of reading `gat.yaml` again -- the entry point a
/// top-level sync (`engine::workspace::sync::sync_from_snapshot`) uses so
/// regenerating `.git/info/exclude` doesn't reload config a second time
/// within one operation.
pub(crate) fn sync_from_lock_with_config(
    repo: &Repo,
    lock: &Lock,
    dry_run: bool,
    cfg: &gat_core::config::Config,
) -> Result<SyncStatus> {
    let plan = compact::IgnoreCoveragePlan::new(cfg.git.effective_ignore_patterns());
    let mut exact: Vec<_> = lock
        .entries
        .iter()
        .filter(|entry| !plan.covers(&entry.path))
        .map(|entry| entry.path.as_str().to_string())
        .collect();
    // Unlike the store-backed path below, `lock.entries` carries no
    // ordering/uniqueness guarantee of its own.
    if !exact.is_sorted() {
        exact.sort_unstable();
    }
    exact.dedup();
    render(repo, cfg, exact, dry_run)
}

/// Regenerate the gat-managed block in `.git/info/exclude` straight from
/// the state store's desired mirror, streaming ordered desired paths one
/// at a time instead of materializing a full `Lock`/`Vec<Entry>` first (see
/// the state store. Sharded `gat rm`/`gat mv` use this after a sparse desired
/// mutation so a repo-wide `Entry` load is never required solely to keep
/// `.git/info/exclude` current.
#[cfg(test)]
pub fn sync_from_store(repo: &Repo, store: &StateStore, dry_run: bool) -> Result<SyncStatus> {
    let cfg = repo.load_config().map_err(Box::new)?;
    sync_from_store_with_config(repo, store, dry_run, &cfg)
}

/// Regenerate excludes from an opaque repository mutation session.
///
/// The session chooses whether paths come from its retained complete lock or
/// its retained `SQLite` view; this layer sees only semantic paths and cannot
/// branch on publication shape.
pub(crate) fn sync_from_mutation(
    repo: &Repo,
    session: &gat_io::DesiredMutationSession<'_>,
    dry_run: bool,
    cfg: &gat_core::config::Config,
) -> Result<SyncStatus> {
    sync_from_path_source(repo, dry_run, cfg, |excluded, visit| {
        session.visit_desired_paths::<ExcludesError>(excluded, visit)
    })
}

/// Regenerate excludes from an opaque mount mutation session without
/// reopening desired state or exposing its physical publication shape.
pub(crate) fn sync_from_mount_mutation(
    repo: &Repo,
    session: &gat_io::MountMutationSession<'_>,
    dry_run: bool,
    cfg: &gat_core::config::Config,
) -> Result<SyncStatus> {
    sync_from_path_source(repo, dry_run, cfg, |excluded, visit| {
        session.visit_desired_paths::<ExcludesError>(excluded, visit)
    })
}

fn sync_from_path_source(
    repo: &Repo,
    dry_run: bool,
    cfg: &gat_core::config::Config,
    visit_paths: impl FnOnce(
        &gat_io::DesiredPathExclusions,
        &mut dyn FnMut(&gat_core::lexical_path::GatPath) -> Result<()>,
    ) -> Result<()>,
) -> Result<SyncStatus> {
    let plan = compact::IgnoreCoveragePlan::new(cfg.git.effective_ignore_patterns());
    let mut exact = Vec::new();
    visit_paths(&plan.excluded, &mut |path| {
        if !plan.residual_matches(path) {
            exact.push(path.as_str().to_string());
        }
        Ok(())
    })?;
    if !exact.is_sorted() {
        exact.sort_unstable();
    }
    exact.dedup();
    render(repo, cfg, exact, dry_run)
}

/// As [`sync_from_store`], but reuses an already-loaded effective config
/// snapshot instead of reading `gat.yaml` again.
#[cfg(test)]
pub(crate) fn sync_from_store_with_config(
    repo: &Repo,
    store: &StateStore,
    dry_run: bool,
    cfg: &gat_core::config::Config,
) -> Result<SyncStatus> {
    let exact = exact_from_store(store, cfg)?;
    // `path` is the state table's primary key and the cursor above reads
    // it `ORDER BY path`, so the result is already sorted and duplicate-free
    // -- no extra `sort`/`dedup` pass needed, unlike the `Lock`-based path.
    render(repo, cfg, exact, dry_run)
}

/// Collect only uncovered paths; literal directory coverage is applied by
/// indexed seeks before any residual Git-ignore matching in Rust.
fn exact_from_store(store: &StateStore, cfg: &gat_core::config::Config) -> Result<Vec<String>> {
    let plan = compact::IgnoreCoveragePlan::new(cfg.git.effective_ignore_patterns());
    let mut exact = Vec::new();
    store.visit_desired_paths_excluding(&plan.excluded, |path| -> Result<()> {
        if !plan.residual_matches(path) {
            exact.push(path.as_str().to_string());
        }
        Ok(())
    })?;
    Ok(exact)
}

/// Encode a root-anchored literal Git-ignore rule. The caller handles LF
/// paths separately because the line-based format cannot express them.
/// Glob metacharacters and trailing spaces are escaped; CR uses a character
/// class so Git cannot strip it as part of a line ending.
fn exact_exclude_pattern(path: &str) -> String {
    let head = path.trim_end_matches(' ');
    let trailing_spaces = path.len() - head.len();
    let mut escaped = String::with_capacity(path.len() + 1 + trailing_spaces);
    escaped.push('/');
    for c in head.chars() {
        if c == '\r' {
            escaped.push_str("[\r]");
            continue;
        }
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    for _ in 0..trailing_spaces {
        escaped.push_str("\\ ");
    }
    escaped
}

/// Render `exact` (already the final, sorted, deduplicated set of paths that
/// need their own rule) plus `git.ignore_patterns` into the
/// gat-managed `.git/info/exclude` block, writing it out unless `dry_run` or
/// nothing changed. Shared tail of lock-backed and store-backed generation
/// once each has produced its `exact` set. `exact` entries are Gat-managed
/// paths and are rendered through [`exact_exclude_pattern`]; `git.ignore_patterns`
/// are hand-authored and stay verbatim.
fn render(
    repo: &Repo,
    cfg: &gat_core::config::Config,
    exact: Vec<String>,
    dry_run: bool,
) -> Result<SyncStatus> {
    Ok(render_with_proof(repo, cfg, exact, dry_run)?.0)
}

/// As [`render`], but also returns the gat-managed block's own content
/// identity and the opaque I/O update receipt. The only caller that needs
/// either extra value is [`sync_from_store_fast_path`]'s rebuild tier.
///
/// Delegates coherent source observation, bounded retries, and atomic
/// publication to [`gat_io::mutate_info_exclude`]. Only the managed block
/// is replaced; content outside its markers remains user-owned.
fn render_with_proof(
    repo: &Repo,
    cfg: &gat_core::config::Config,
    exact: Vec<String>,
    dry_run: bool,
) -> Result<(SyncStatus, [u8; 32], gat_io::InfoExcludeUpdate)> {
    let mut body = String::new();
    let mut count = 0;
    for pattern in cfg.git.effective_ignore_patterns() {
        body.push_str(pattern.as_str());
        body.push('\n');
        count += 1;
    }
    for path in exact {
        // Git's line-delimited format cannot represent an exact LF path.
        // A comment records the omission without inventing a broader rule.
        if path.contains('\n') {
            use std::fmt::Write as _;
            writeln!(body, "# No exact Git exclusion for LF path: {path:?}").expect("string write");
            continue;
        }
        body.push_str(&exact_exclude_pattern(&path));
        body.push('\n');
        count += 1;
    }
    let block_identity = *blake3::hash(body.as_bytes()).as_bytes();

    let update = gat_io::mutate_info_exclude(repo.layout(), dry_run, |existing| {
        let updated = gat_core::managed_block::upsert(existing, BEGIN, END, &body);
        if updated == existing {
            gat_io::InfoExcludeMutation::Unchanged
        } else {
            gat_io::InfoExcludeMutation::Replace(updated)
        }
    })?;
    Ok((
        SyncStatus {
            changed: update.changed(),
            count,
        },
        block_identity,
        update,
    ))
}

/// Whether `.git/info/exclude` currently contains Gat's managed marker block
/// at all, regardless of whether the block's *contents* are current.
pub fn managed_block_present(repo: &Repo) -> Result<bool> {
    let Some(snapshot) = gat_io::read_info_exclude(repo.layout())? else {
        return Ok(false);
    };
    Ok(snapshot.contents().contains(BEGIN) && snapshot.contents().contains(END))
}

/// Remove only Gat's managed block from `.git/info/exclude`, leaving every
/// other user-managed line untouched. If Gat never wrote a managed block
/// there, this is a no-op returning `false`. Deletes the file entirely when
/// nothing but Gat's block was present.
///
/// Uses [`gat_io::mutate_info_exclude`] to preserve a coherent observation of
/// user-owned content and revalidate it before replacement or removal.
pub fn remove_managed_block(repo: &Repo) -> Result<bool> {
    let update = gat_io::mutate_info_exclude(repo.layout(), false, |existing| {
        if !(existing.contains(BEGIN) && existing.contains(END)) {
            return gat_io::InfoExcludeMutation::Unchanged;
        }
        let updated = gat_core::managed_block::remove(existing, BEGIN, END);
        if updated.trim().is_empty() {
            gat_io::InfoExcludeMutation::Remove
        } else {
            gat_io::InfoExcludeMutation::Replace(updated)
        }
    })?;
    Ok(update.changed())
}

/// Combine `store`'s desired-lock-set fingerprint with `git.ignore_patterns`,
/// then decide whether `.git/info/exclude`'s gat-managed block still needs
/// regenerating -- the one coordinator both `Validation::TrustState` and
/// `Validation::Validate` call, so a warm default validated sync gets the
/// same "prove nothing
/// changed, skip a full desired-path read" fast path `TrustState` already
/// had, instead of only `TrustState` ever skipping the regeneration read.
///
/// Three tiers, cheapest first:
/// 1. **Proof hit**: the recorded fingerprint still matches and a stat of
///    the current output file exactly matches the
///    recorded stat proof -- unchanged, zero file reads.
/// 2. **Proof miss, inputs unchanged**: the recorded fingerprint still
///    matches but the stat proof didn't confirm (moved or never minted)
///    -- read the file exactly once through the shared coherent-
///    observation primitive, hash only the gat-managed block's own bytes
///    (via `gat_core::managed_block::extract_body`), and compare against
///    the recorded block identity. An observed pre/post mutation fails
///    closed rather than trusting bytes that couldn't be proven to
///    describe one coherent filesystem state.
/// 3. **Rebuild**: fingerprint mismatch, missing output file, or a
///    differing block identity -- regenerate only the managed block,
///    preserving every other line, then mint a fresh proof from one
///    post-write observation and record the new fingerprint/count/block
///    identity/proof.
pub(crate) fn sync_from_store_fast_path(
    repo: &Repo,
    store: &mut StateStore,
    cfg: &gat_core::config::Config,
) -> Result<SyncStatus> {
    let expected_fingerprint = fingerprint(
        &store.desired_fingerprint()?,
        cfg.git.effective_ignore_patterns(),
    );
    let record = store.exclude_record()?;
    if record.fingerprint() == Some(expected_fingerprint) {
        let verification = record.verify_info_exclude(repo.layout(), BEGIN, END)?;
        if verification.is_current() {
            let count = record.count();
            store.refresh_exclude_verification(verification)?;
            return Ok(SyncStatus {
                changed: false,
                count,
            });
        }
    }

    // Tier 3: rebuild. Regenerates only the gat-managed block, preserving
    // any bytes outside gat's markers. Both the rebuilt block's own
    // identity and (when a write actually happened) its write proof come
    // straight back from `render_with_proof` -- no re-read of the file
    // and no separate post-write stat are needed.
    let exact = exact_from_store(store, cfg)?;
    let (status, block_identity, update) = render_with_proof(repo, cfg, exact, false)?;
    store.record_exclude_output(expected_fingerprint, status.count, block_identity, update)?;
    Ok(status)
}

/// Test-only seam for deterministically exercising the pre-read/post-read
/// stat race window inside [`sync_from_store_fast_path`]'s tier-2 read,
/// mirroring `engine::workspace::sync::desired_index`'s `race_test_hooks` (see its
/// process-wide because environment mutation cannot be isolated per caller).
/// Production code never calls into this module.
///
/// Because the installed hook is process-wide, callers **must** filter on
/// the exact path they expect inside their closure rather than acting
/// unconditionally on whatever path is passed in -- otherwise, under a
/// parallel test run, this hook would also fire for (and corrupt) an
/// unrelated, concurrently-running test's own `.git/info/exclude` file.
///
/// Filtering on path is not enough by itself, though: since there is only
/// ever one process-wide hook slot per hook point, two tests that both
/// install a hook concurrently would race to overwrite each other's
/// closure. [`RaceHookGuard::acquire`] additionally serializes every test
/// in this module that installs *any* hook against every other one, for
/// the whole install-run-clear window.
#[cfg(test)]
mod race_test_hooks {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    static GATE: Mutex<()> = Mutex::new(());

    /// Held for the whole install-run-clear window of a race-hook test,
    /// serializing it against every other race-hook test in this module.
    pub(super) struct RaceHookGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl RaceHookGuard {
        pub(super) fn acquire() -> Self {
            Self(
                GATE.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }
    }

    pub(super) fn set(hook: impl FnMut(&Path) + Send + 'static) {
        gat_io::git_info_exclude_test_support::set(hook);
    }

    pub(super) fn clear() {
        gat_io::git_info_exclude_test_support::clear();
    }

    /// A second, independent hook point fired immediately before
    /// `render_with_proof`/`remove_managed_block` revalidate that their
    /// already-acquired `ExcludeSource` is still current -- i.e. after the
    /// coherent read has already completed, simulating a concurrent editor
    /// landing a change in the read-to-revalidate window rather than mid-read
    /// (which `BEFORE_READ` above simulates instead). Tests use this to
    /// exercise the bounded-retry/retry-exhaustion behavior specifically,
    /// as distinct from `acquire_exclude_source`'s own mid-read coherence
    /// failure.
    pub(super) fn set_before_revalidate(hook: impl FnMut(&Path) + Send + 'static) {
        gat_io::git_info_exclude_test_support::set_before_revalidate(hook);
    }

    pub(super) fn clear_before_revalidate() {
        gat_io::git_info_exclude_test_support::clear_before_revalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_harness::git_repo;
    use crate::workspace::sync::desired_index;
    use gat_io::StateStore;

    fn insert(lock: &mut Lock, path: &str, oid_digit: char) {
        lock.upsert(
            gat_core::lexical_path::GatPath::parse_canonical(path).unwrap(),
            gat_core::oid::Oid::from_hex(&oid_digit.to_string().repeat(64)).unwrap(),
        );
    }

    fn store_with_lock(repo: &Repo, lock: &Lock) -> StateStore {
        repo.save_lock(lock).unwrap();
        let mut store = StateStore::open(repo.layout()).unwrap();
        desired_index::refresh(repo, &mut store).unwrap();
        store
    }

    fn set_ignore_patterns(repo: &Repo, patterns: &[&str]) {
        let mut cfg = repo.load_config().unwrap();
        cfg.git.ignore_patterns = Some(
            patterns
                .iter()
                .map(|s| gat_core::git_ignore::GitIgnorePattern::parse(*s).unwrap())
                .collect(),
        );
        repo.save_config(&cfg).unwrap();
    }

    /// Writes a hand-edited `gat.yaml` containing a raw, not-yet-validated
    /// `git.ignore_patterns` entry directly, bypassing
    /// [`gat_core::git_ignore::GitIgnorePattern::parse`] entirely --
    /// simulating a project config someone edited by hand rather than
    /// through `gat config`, the only way a negated pattern can still
    /// reach a `Config` at all now that construction validates it.
    fn write_raw_ignore_pattern(root: &Path, pattern: &str) {
        std::fs::write(
            root.join("gat.yaml"),
            format!("git:\n  ignore_patterns:\n  - \"{pattern}\"\n"),
        )
        .unwrap();
    }

    /// Whether git's own ignore engine (via gix) would consider `path`
    /// ignored by `exclude_text` -- proving the *semantics* of a rendered
    /// `.git/info/exclude` block, not merely that some substring of it
    /// happens to appear in the file. Mirrors the ancestor-then-leaf
    /// matching `compact::pattern_matches` uses in production.
    fn is_ignored(exclude_text: &str, path: &str) -> bool {
        let lines: Vec<&str> = exclude_text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .collect();
        gat_io::GitIgnoreMatcher::new(lines).is_ignored(path)
    }

    #[test]
    fn pruned_store_and_lock_render_like_the_original_matcher() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        for path in [
            "data/a",
            "data/sub/b",
            "data0/x",
            "é/x",
            "a b/x",
            "keep.bin",
            "keep.txt",
            "emptydir",
            "odd\npath",
        ] {
            insert(&mut lock, path, 'a');
        }
        let store = store_with_lock(&repo, &lock);
        let exclude_path = tmp.path().join(".git/info/exclude");
        for rules in [
            vec![],
            vec!["/data/"],
            vec!["/data/sub/", "/data/", "/data/"],
            vec!["*.bin"],
            vec!["/é/", "/data/", "*.bin", "/emptydir/"],
            vec!["data/", "/a b/", "/*.txt"],
        ] {
            set_ignore_patterns(&repo, &rules);
            let cfg = repo.load_config().unwrap();
            let matcher = compact::build_pattern_search(cfg.git.effective_ignore_patterns());
            let mut exact: Vec<_> = lock
                .entries
                .iter()
                .filter(|e| {
                    !matcher
                        .as_ref()
                        .is_some_and(|m| compact::pattern_matches(m, e.path.as_str()))
                })
                .map(|e| e.path.as_str().to_string())
                .collect();
            exact.sort();
            exact.dedup();
            render(&repo, &cfg, exact, false).unwrap();
            let expected = std::fs::read_to_string(&exclude_path).unwrap();
            sync_from_lock_with_config(&repo, &lock, false, &cfg).unwrap();
            assert_eq!(std::fs::read_to_string(&exclude_path).unwrap(), expected);
            sync_from_store_with_config(&repo, &store, false, &cfg).unwrap();
            assert_eq!(std::fs::read_to_string(&exclude_path).unwrap(), expected);
        }
    }

    #[test]
    fn control_paths_cannot_inject_rules_or_lose_trailing_carriage_return() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "a\nb", 'a');
        insert(&mut lock, "tail\r", 'b');
        repo.save_lock(&lock).unwrap();
        sync(&repo, false).unwrap();
        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(text.contains("# No exact Git exclusion for LF path:"));
        for (path, ignored) in [
            ("a\nb", false),
            ("a", false),
            ("b", false),
            ("tail\r", true),
            ("tail", false),
        ] {
            let output = test_support_git::GitCommand::new(
                tmp.path(),
                &["check-ignore", "--no-index", "--", path],
            )
            .output();
            assert_eq!(output.status.code(), Some(i32::from(!ignored)), "{path:?}");
        }
    }

    #[test]
    fn exact_exclude_pattern_escapes_glob_metacharacters_and_anchors_root() {
        assert_eq!(exact_exclude_pattern("data/model.bin"), "/data/model.bin");
        assert_eq!(
            exact_exclude_pattern("data/[foo]/model?.bin"),
            "/data/\\[foo\\]/model\\?.bin"
        );
        assert_eq!(exact_exclude_pattern("a*b.bin"), "/a\\*b.bin");
        assert_eq!(exact_exclude_pattern("a\\b.bin"), "/a\\\\b.bin");
        assert_eq!(exact_exclude_pattern("trailing.bin "), "/trailing.bin\\ ");
    }

    /// A root-tracked file must not cause git to also ignore a same-named
    /// file nested elsewhere in the tree -- the unanchored-rule bug this
    /// issue fixes.
    #[test]
    fn root_managed_file_does_not_hide_unrelated_nested_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "model.bin", 'a');
        repo.save_lock(&lock).unwrap();

        sync(&repo, false).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(
            is_ignored(&text, "model.bin"),
            "tracked root file must be ignored"
        );
        assert!(
            !is_ignored(&text, "nested/model.bin"),
            "unrelated nested file with the same name must not be ignored"
        );
    }

    /// Literal glob metacharacters in a tracked path (a directory name in
    /// this case) must be treated as literal characters, not glob syntax,
    /// by git's own ignore engine.
    #[test]
    fn literal_metacharacter_directory_is_ignored_via_git_semantics() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "data/[foo]/model.bin", 'a');
        repo.save_lock(&lock).unwrap();

        sync(&repo, false).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(is_ignored(&text, "data/[foo]/model.bin"));
        // A sibling directory whose name happens to satisfy the character
        // class `[foo]` would match were the rule not escaped.
        assert!(!is_ignored(&text, "data/f/model.bin"));
    }

    /// Same idea, but the metacharacters are in the final filename
    /// component rather than a directory name.
    #[test]
    fn literal_metacharacter_filename_is_ignored_without_wildcarding_to_siblings() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "data/model?.bin", 'a');
        insert(&mut lock, "data/file[1].bin", 'b');
        repo.save_lock(&lock).unwrap();

        sync(&repo, false).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(is_ignored(&text, "data/model?.bin"));
        assert!(is_ignored(&text, "data/file[1].bin"));
        // `?` is a single-char wildcard and `[1]` a character class in
        // plain Git-ignore syntax -- neither must wildcard to a sibling.
        assert!(!is_ignored(&text, "data/modelx.bin"));
        assert!(!is_ignored(&text, "data/file1.bin"));
    }

    #[test]
    fn default_writes_only_exact_tracked_paths_into_info_exclude() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "big.bin", 'a');
        repo.save_lock(&lock).unwrap();

        sync(&repo, false).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(text.contains("big.bin"));
        assert!(!text.contains(".gat/"));
    }

    #[test]
    fn sync_is_idempotent() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "big.bin", 'a');
        repo.save_lock(&lock).unwrap();

        sync(&repo, false).unwrap();
        let first = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        sync(&repo, false).unwrap();
        let second = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn sync_drops_removed_paths_after_lock_update() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "big.bin", 'a');
        repo.save_lock(&lock).unwrap();
        sync(&repo, false).unwrap();

        repo.save_lock(&Lock::default()).unwrap();
        sync(&repo, false).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(!text.contains("big.bin"));
        assert!(!text.contains(".gat/"));
    }

    #[test]
    fn dry_run_reports_change_without_writing() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "big.bin", 'a');
        repo.save_lock(&lock).unwrap();

        let status = sync(&repo, true).unwrap();
        assert!(status.changed);
        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(!text.contains("big.bin"));

        let status = sync(&repo, false).unwrap();
        assert!(status.changed);
        let status = sync(&repo, true).unwrap();
        assert!(!status.changed);
    }

    #[test]
    fn default_empty_patterns_takes_fast_path_and_still_writes_all_exact_paths() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        assert!(
            repo.load_config()
                .unwrap()
                .git
                .effective_ignore_patterns()
                .is_empty()
        );
        let mut lock = Lock::default();
        insert(&mut lock, "b.bin", 'a');
        insert(&mut lock, "a.bin", 'b');
        insert(&mut lock, "a.bin", 'b'); // duplicate insert stays deduped
        repo.save_lock(&lock).unwrap();

        let status = sync(&repo, false).unwrap();
        assert!(status.changed);

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(text.contains("a.bin"));
        assert!(text.contains("b.bin"));
        assert!(!text.contains(".gat/"));
        // exact rules are sorted, so a.bin must precede b.bin
        assert!(text.find("a.bin").unwrap() < text.find("b.bin").unwrap());
    }

    #[test]
    fn custom_exclude_pattern_is_written_and_suppresses_redundant_exact_rule() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_ignore_patterns(&repo, &["*.safetensors"]);
        let mut lock = Lock::default();
        insert(&mut lock, "model.safetensors", 'a');
        repo.save_lock(&lock).unwrap();

        sync(&repo, false).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(text.contains("*.safetensors"));
        // Covered by the pattern already -- must not also get an exact rule.
        assert!(!text.contains("model.safetensors"));
    }

    #[test]
    fn directory_pattern_suppresses_exact_rules_for_every_file_underneath() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_ignore_patterns(&repo, &["/data/"]);
        let mut lock = Lock::default();
        for i in 0..50 {
            let name = format!("data/f{i:05}.bin");
            insert(&mut lock, &name, 'a');
        }
        repo.save_lock(&lock).unwrap();

        sync(&repo, false).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(text.contains("/data/"));
        for i in 0..50 {
            assert!(!text.contains(&format!("f{i:05}.bin")));
        }
    }

    #[test]
    fn negated_custom_pattern_is_rejected() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        write_raw_ignore_pattern(tmp.path(), "!keep.txt");

        let err = sync(&repo, false).unwrap_err();
        assert!(format!("{err:?}").contains("negated"));
    }

    /// `sync_from_store` must write exactly what
    /// `sync_from_lock` would for the same desired set -- proving the
    /// streamed-paths path is a pure optimization, not a behavior change.
    #[test]
    fn sync_from_store_matches_sync_from_lock_byte_for_byte() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "b.bin", 'a');
        insert(&mut lock, "a.bin", 'b');
        insert(&mut lock, "nested/c.bin", 'c');
        let store = store_with_lock(&repo, &lock);

        let exclude_path = tmp.path().join(".git/info/exclude");
        let original = std::fs::read_to_string(&exclude_path).unwrap();
        sync_from_lock(&repo, &lock, false).unwrap();
        let from_lock = std::fs::read_to_string(&exclude_path).unwrap();

        std::fs::write(&exclude_path, &original).unwrap();
        sync_from_store(&repo, &store, false).unwrap();
        let from_store = std::fs::read_to_string(&exclude_path).unwrap();

        assert_eq!(from_lock, from_store);
    }

    /// Same byte-for-byte guarantee, but with tracked paths that contain
    /// Git-ignore metacharacters, so both paths exercise the escaping in
    /// `exact_exclude_pattern` identically.
    #[test]
    fn sync_from_store_matches_sync_from_lock_byte_for_byte_with_metacharacters() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "data/[foo]/model.bin", 'a');
        insert(&mut lock, "data/model?.bin", 'b');
        insert(&mut lock, "data/file[1].bin", 'c');
        insert(&mut lock, "model.bin", 'd');
        let store = store_with_lock(&repo, &lock);

        let exclude_path = tmp.path().join(".git/info/exclude");
        let original = std::fs::read_to_string(&exclude_path).unwrap();
        sync_from_lock(&repo, &lock, false).unwrap();
        let from_lock = std::fs::read_to_string(&exclude_path).unwrap();

        std::fs::write(&exclude_path, &original).unwrap();
        sync_from_store(&repo, &store, false).unwrap();
        let from_store = std::fs::read_to_string(&exclude_path).unwrap();

        assert_eq!(from_lock, from_store);
        assert!(is_ignored(&from_store, "data/[foo]/model.bin"));
        assert!(is_ignored(&from_store, "data/model?.bin"));
        assert!(is_ignored(&from_store, "data/file[1].bin"));
        assert!(is_ignored(&from_store, "model.bin"));
        assert!(!is_ignored(&from_store, "nested/model.bin"));
    }

    /// configured, so both paths exercise the pattern-compaction logic.
    #[test]
    fn sync_from_store_matches_sync_from_lock_with_ignore_patterns() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_ignore_patterns(&repo, &["/data/"]);
        let mut lock = Lock::default();
        for i in 0..20 {
            insert(&mut lock, &format!("data/f{i:05}.bin"), 'a');
        }
        insert(&mut lock, "keep.bin", 'b');
        let store = store_with_lock(&repo, &lock);

        let exclude_path = tmp.path().join(".git/info/exclude");
        let original = std::fs::read_to_string(&exclude_path).unwrap();
        sync_from_lock(&repo, &lock, false).unwrap();
        let from_lock = std::fs::read_to_string(&exclude_path).unwrap();

        std::fs::write(&exclude_path, &original).unwrap();
        sync_from_store(&repo, &store, false).unwrap();
        let from_store = std::fs::read_to_string(&exclude_path).unwrap();

        assert_eq!(from_lock, from_store);
        assert!(from_store.contains("/data/"));
        assert!(from_store.contains("keep.bin"));
    }

    /// User-authored lines outside the managed block must survive
    /// `sync_from_store` exactly as they do for `sync_from_lock`.
    #[test]
    fn sync_from_store_preserves_user_lines_outside_managed_block() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let exclude_path = tmp.path().join(".git/info/exclude");
        std::fs::write(&exclude_path, "# my own rule\n*.log\n").unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "big.bin", 'a');
        let store = store_with_lock(&repo, &lock);

        sync_from_store(&repo, &store, false).unwrap();

        let text = std::fs::read_to_string(&exclude_path).unwrap();
        assert!(text.contains("# my own rule"));
        assert!(text.contains("*.log"));
        assert!(text.contains("big.bin"));
        assert!(!text.contains(".gat/"));
    }

    /// `sync`'s managed block must preserve a pre-existing `CRLF`
    /// `.git/info/exclude`'s line-ending convention: user content around
    /// the block stays `CRLF`, and the newly-written managed rules must
    /// not introduce bare `LF` lines into it.
    #[test]
    fn sync_preserves_crlf_info_exclude_without_mixing_line_endings() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let exclude_path = tmp.path().join(".git/info/exclude");
        std::fs::write(&exclude_path, "# my own rule\r\n*.log\r\n").unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "big.bin", 'a');
        sync_from_lock(&repo, &lock, false).unwrap();

        let text = std::fs::read_to_string(&exclude_path).unwrap();
        assert!(text.contains("# my own rule"));
        assert!(text.contains("big.bin"));
        assert_eq!(
            text.matches('\n').count(),
            text.matches("\r\n").count(),
            "every line must stay CRLF, got {text:?}"
        );
    }

    /// `remove_managed_block` (the "uninstall" path `gat system clean git`
    /// drives) must preserve a pre-existing `CRLF` `.git/info/exclude`'s
    /// convention for whatever user content survives around the removed
    /// block, and delete the file entirely (not leave a stray CRLF/empty
    /// remnant) when Gat's block was the only content.
    #[test]
    fn remove_managed_block_preserves_crlf_for_surviving_content() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let exclude_path = tmp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude_path.parent().unwrap()).unwrap();
        std::fs::write(
            &exclude_path,
            format!("# my own rule\r\n{BEGIN}\r\nbig.bin\r\n{END}\r\nkeep-me\r\n"),
        )
        .unwrap();

        assert!(remove_managed_block(&repo).unwrap());

        let text = std::fs::read_to_string(&exclude_path).unwrap();
        assert_eq!(text, "# my own rule\r\nkeep-me\r\n");
    }

    /// Same as above, but Gat's managed block is the file's only content:
    /// `remove_managed_block` must delete `.git/info/exclude` outright,
    /// regardless of the removed block's own line-ending style.
    #[test]
    fn remove_managed_block_deletes_a_crlf_file_that_was_only_the_managed_block() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let exclude_path = tmp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude_path.parent().unwrap()).unwrap();
        std::fs::write(&exclude_path, format!("{BEGIN}\r\nbig.bin\r\n{END}\r\n")).unwrap();

        assert!(remove_managed_block(&repo).unwrap());

        assert!(!exclude_path.exists());
    }

    /// Local storage protection is independent of the desired-path block.
    #[test]
    fn sync_from_store_has_no_rules_for_empty_desired_state() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let store = store_with_lock(&repo, &Lock::default());

        sync_from_store(&repo, &store, false).unwrap();

        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(!text.contains(".gat/"));
    }

    #[test]
    fn old_exclude_fingerprint_migrates_local_directory_rule_out_of_managed_block() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut store = store_with_lock(&repo, &Lock::default());
        let cfg = repo.load_config().unwrap();
        let mut old_fingerprint = blake3::Hasher::new();
        old_fingerprint.update(b"gat-excludes-v2\0");
        old_fingerprint.update(&store.desired_fingerprint().unwrap());
        let update = gat_io::mutate_info_exclude(repo.layout(), false, |_| {
            gat_io::InfoExcludeMutation::Replace(format!("user-rule\n{BEGIN}\n.gat/\n{END}\n"))
        })
        .unwrap();
        store
            .record_exclude_output(
                *old_fingerprint.finalize().as_bytes(),
                1,
                *blake3::hash(b".gat/\n").as_bytes(),
                update,
            )
            .unwrap();

        let result = sync_from_store_fast_path(&repo, &mut store, &cfg).unwrap();
        assert!(result.changed);
        assert_eq!(result.count, 0);
        let text = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(text.starts_with("user-rule\n"));
        assert!(!text.contains(".gat/"));
        assert!(
            !sync_from_store_fast_path(&repo, &mut store, &cfg)
                .unwrap()
                .changed
        );
    }

    /// A freshly regenerated
    /// `.git/info/exclude` output's post-write proof describes exactly
    /// the bytes just written, so it is persisted immediately and unlocks
    /// tier 1 for the very next call -- no separate settling call is
    /// needed.
    #[test]
    fn fast_path_persists_a_reusable_proof_from_the_very_first_coherent_observation() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let cfg = repo.load_config().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", 'a');
        let mut store = store_with_lock(&repo, &lock);

        // First call: the managed block has never been generated, so this
        // is a tier-3 rebuild. Its post-write proof describes exactly the
        // bytes just written, so it is persisted immediately -- no
        // waiting for the file to "settle" is required under the
        // coherent-observation model.
        let status = sync_from_store_fast_path(&repo, &mut store, &cfg).unwrap();
        assert!(status.changed);
        let record = store.exclude_record().unwrap();
        assert!(
            record.has_reusable_proof(),
            "a freshly rebuilt block's proof describes exactly the bytes just \
             written and must be persisted immediately"
        );
        assert!(record.block_identity().is_some());

        // Second call: the stored proof's stat still matches exactly, so
        // this takes the tier-1 fast path and never re-reads the file.
        let exclude_path = tmp.path().join(".git/info/exclude");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exclude_path, std::fs::Permissions::from_mode(0o000))
                .unwrap();
            let restore = scopeguard(&exclude_path);
            let reads_before = gat_io::git_info_exclude_test_support::content_read_count();
            let status = sync_from_store_fast_path(&repo, &mut store, &cfg).unwrap();
            let reads_after = gat_io::git_info_exclude_test_support::content_read_count();
            drop(restore);
            assert!(!status.changed);
            assert_eq!(
                reads_after, reads_before,
                "a warm proof hit must not read exclude contents"
            );
        }
    }

    #[test]
    fn proof_miss_reads_once_then_refreshes_the_warm_path() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let cfg = repo.load_config().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", 'a');
        let mut store = store_with_lock(&repo, &lock);

        sync_from_store_fast_path(&repo, &mut store, &cfg).unwrap();
        let exclude_path = tmp.path().join(".git/info/exclude");
        let existing = std::fs::read_to_string(&exclude_path).unwrap();
        std::fs::write(&exclude_path, format!("# user rule\n{existing}")).unwrap();

        let reads_before = gat_io::git_info_exclude_test_support::content_read_count();
        let status = sync_from_store_fast_path(&repo, &mut store, &cfg).unwrap();
        let reads_after = gat_io::git_info_exclude_test_support::content_read_count();
        assert!(!status.changed);
        assert_eq!(
            reads_after - reads_before,
            1,
            "a proof miss with an unchanged managed block must read contents once"
        );

        let status = sync_from_store_fast_path(&repo, &mut store, &cfg).unwrap();
        assert!(!status.changed);
        assert_eq!(
            gat_io::git_info_exclude_test_support::content_read_count(),
            reads_after,
            "the refreshed proof must make the next verification read-free"
        );
    }

    /// A rewrite landing
    /// between the pre-read and post-read stat of tier 2's content read
    /// must fail the whole exclude sync closed -- even though the bytes
    /// read still parse into a well-formed managed block -- rather than
    /// letting an equal-looking result paper over an unstable filesystem
    /// generation. Forces the race deterministically via
    /// [`race_test_hooks`] (a real thread-timing race would make this
    /// test flaky), matching `desired_index`'s analogous regression.
    #[test]
    fn fast_path_fails_closed_on_a_mid_read_rewrite_of_info_exclude() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let cfg = repo.load_config().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", 'a');
        let mut store = store_with_lock(&repo, &lock);

        // Establish a stable tier-1 proof first.
        sync_from_store_fast_path(&repo, &mut store, &cfg).unwrap();
        let exclude_path = tmp.path().join(".git/info/exclude");
        assert!(store.exclude_record().unwrap().has_reusable_proof());

        // Force a tier-2 read without touching the desired lock set (the
        // fingerprint must still match, or this would take tier 3
        // instead): rewrite the file with byte-identical managed content,
        // so tier 1's stat-only proof match misses and a tier-2 read is
        // required. The race hook then rewrites the file's content the
        // instant before that read runs -- still a well-formed managed
        // block, so only the pre/post stat mismatch can be responsible
        // for the failure this asserts.
        let existing = std::fs::read_to_string(&exclude_path).unwrap();
        std::fs::write(&exclude_path, &existing).unwrap();
        let target_path = exclude_path;
        let _guard = race_test_hooks::RaceHookGuard::acquire();
        race_test_hooks::set(move |path: &Path| {
            if path == target_path {
                let existing = std::fs::read_to_string(path).unwrap();
                std::fs::write(path, format!("{existing}# rewritten mid-read\n")).unwrap();
            }
        });
        let result = sync_from_store_fast_path(&repo, &mut store, &cfg);
        race_test_hooks::clear();

        assert!(
            result.is_err(),
            "an exclude output observed through an unstable pre-read/post-read stat pair \
             must fail the sync closed rather than trust those bytes"
        );
    }

    #[cfg(unix)]
    fn scopeguard(path: &Path) -> impl Drop + '_ {
        struct Guard<'a>(&'a Path);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o644));
            }
        }
        Guard(path)
    }

    /// A read error other than "the file doesn't exist yet"
    /// (e.g. permission denied) must propagate as an error, not be
    /// silently folded into the "nothing there yet" empty-document case
    /// `acquire_exclude_source` uses for a genuine `NotFound`.
    #[cfg(unix)]
    #[test]
    fn acquire_exclude_source_propagates_a_non_not_found_read_error() {
        let tmp = git_repo();
        let exclude_path = tmp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude_path.parent().unwrap()).unwrap();
        std::fs::write(&exclude_path, "existing\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exclude_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = scopeguard(&exclude_path);

        let layout = gat_io::RepositoryLayout::at(tmp.path().to_path_buf());
        let result = gat_io::read_info_exclude(&layout);

        assert!(
            result.is_err(),
            "a permission-denied read must propagate as an error, not be treated as an \
             empty starting document the way a genuine NotFound is"
        );
    }

    /// The same mid-read coherence protection
    /// [`fast_path_fails_closed_on_a_mid_read_rewrite_of_info_exclude`]
    /// exercises for the fast path also applies to `acquire_exclude_source`
    /// as used by [`sync_from_lock`] -- a rewrite landing between the
    /// pre-read and post-read stat must fail the sync closed.
    #[test]
    fn sync_from_lock_fails_closed_on_a_mid_read_rewrite_of_info_exclude() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", 'a');

        // Establish an existing exclude file to read on the next call.
        sync_from_lock(&repo, &lock, false).unwrap();
        let exclude_path = tmp.path().join(".git/info/exclude");

        // Change the desired set so the next call must actually re-read
        // and rewrite the managed block (not take an early "unchanged"
        // return), then force a rewrite mid-read via `race_test_hooks`.
        insert(&mut lock, "b.bin", 'b');
        let target_path = exclude_path;
        let _guard = race_test_hooks::RaceHookGuard::acquire();
        race_test_hooks::set(move |path: &Path| {
            if path == target_path {
                let existing = std::fs::read_to_string(path).unwrap();
                std::fs::write(path, format!("{existing}# rewritten mid-read\n")).unwrap();
            }
        });
        let result = sync_from_lock(&repo, &lock, false);
        race_test_hooks::clear();

        assert!(
            result.is_err(),
            "an exclude source observed through an unstable pre-read/post-read stat pair \
             must fail the sync closed rather than trust those bytes"
        );
    }

    /// A concurrent edit discovered only at the revalidation
    /// step (i.e. after the coherent read completed, in the window before
    /// publication) must be retried rather than either overwriting the
    /// other process's change or failing immediately -- as long as a
    /// later attempt observes a stable generation within the retry
    /// budget.
    #[test]
    fn render_with_proof_retries_once_and_succeeds_after_a_concurrent_edit_clears() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", 'a');
        sync_from_lock(&repo, &lock, false).unwrap();
        let exclude_path = tmp.path().join(".git/info/exclude");
        insert(&mut lock, "b.bin", 'b');

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let watched = exclude_path;
        let _guard = race_test_hooks::RaceHookGuard::acquire();
        race_test_hooks::set_before_revalidate(move |p: &Path| {
            if p == watched {
                let attempt = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt == 0 {
                    // Simulate one concurrent editor landing a change
                    // right after our read but before our revalidation
                    // check -- only on the first attempt, so the retry
                    // succeeds on the second.
                    let existing = std::fs::read_to_string(p).unwrap();
                    std::fs::write(p, format!("{existing}# concurrent edit\n")).unwrap();
                }
            }
        });
        let result = sync_from_lock(&repo, &lock, false);
        race_test_hooks::clear_before_revalidate();

        assert!(
            result.is_ok(),
            "a concurrent edit that clears within the retry budget must succeed, not bail: {:?}",
            result.err()
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "expected exactly one retry (two total revalidation attempts)"
        );
    }

    /// Once every attempt within the retry budget observes a
    /// concurrent edit, `render_with_proof` must surface a clear
    /// concurrent-modification error rather than retry forever or
    /// silently give up with a misleading success.
    #[test]
    fn render_with_proof_surfaces_a_clear_error_after_exhausting_retries() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", 'a');
        sync_from_lock(&repo, &lock, false).unwrap();
        let exclude_path = tmp.path().join(".git/info/exclude");
        insert(&mut lock, "b.bin", 'b');

        let watched = exclude_path;
        let _guard = race_test_hooks::RaceHookGuard::acquire();
        race_test_hooks::set_before_revalidate(move |p: &Path| {
            if p == watched {
                let existing = std::fs::read_to_string(p).unwrap();
                std::fs::write(p, format!("{existing}# concurrent edit\n")).unwrap();
            }
        });
        let result = sync_from_lock(&repo, &lock, false);
        race_test_hooks::clear_before_revalidate();

        let err = result.expect_err("every attempt observing a concurrent edit must bail");
        let message = err.to_string();
        assert!(
            message.contains("modified concurrently")
                && message.contains("giving up after 3 attempts"),
            "unexpected error message: {message}"
        );
    }

    /// `remove_managed_block`'s independent retry loop gets the
    /// same bounded-retry treatment as `render_with_proof`'s.
    #[test]
    fn remove_managed_block_retries_once_and_succeeds_after_a_concurrent_edit_clears() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let exclude_path = tmp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude_path.parent().unwrap()).unwrap();
        std::fs::write(&exclude_path, format!("{BEGIN}\nbig.bin\n{END}\nkeep-me\n")).unwrap();

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let watched = exclude_path.clone();
        let _guard = race_test_hooks::RaceHookGuard::acquire();
        race_test_hooks::set_before_revalidate(move |p: &Path| {
            if p == watched {
                let attempt = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt == 0 {
                    let existing = std::fs::read_to_string(p).unwrap();
                    std::fs::write(p, format!("{existing}# concurrent edit\n")).unwrap();
                }
            }
        });
        let result = remove_managed_block(&repo);
        race_test_hooks::clear_before_revalidate();

        assert!(result.unwrap());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let text = std::fs::read_to_string(&exclude_path).unwrap();
        assert!(text.contains("keep-me"));
        assert!(!text.contains(BEGIN));
    }

    /// `remove_managed_block`'s retry exhaustion surfaces the
    /// same clear concurrent-modification error as `render_with_proof`'s.
    #[test]
    fn remove_managed_block_surfaces_a_clear_error_after_exhausting_retries() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let exclude_path = tmp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude_path.parent().unwrap()).unwrap();
        std::fs::write(&exclude_path, format!("{BEGIN}\nbig.bin\n{END}\nkeep-me\n")).unwrap();

        let watched = exclude_path;
        let _guard = race_test_hooks::RaceHookGuard::acquire();
        race_test_hooks::set_before_revalidate(move |p: &Path| {
            if p == watched {
                let existing = std::fs::read_to_string(p).unwrap();
                std::fs::write(p, format!("{existing}# concurrent edit\n")).unwrap();
            }
        });
        let result = remove_managed_block(&repo);
        race_test_hooks::clear_before_revalidate();

        let err = result.expect_err("every attempt observing a concurrent edit must bail");
        let message = err.to_string();
        assert!(
            message.contains("modified concurrently")
                && message.contains("giving up after 3 attempts"),
            "unexpected error message: {message}"
        );
    }
}
