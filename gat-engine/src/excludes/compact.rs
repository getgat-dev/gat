//! Pure policy for compacting `gat.lock`'s tracked paths into gat's
//! managed `.git/info/exclude` block, kept entirely
//! separate from reading/writing the actual managed block (`super::sync`).
//!
//! Three pieces compose the final rendered rule set
//! (`super::sync` combines them in this order):
//! 1. `.gat/` -- always, handled by the caller.
//! 2. `git.ignore_patterns` -- user-configured, written verbatim, in
//!    configured order.
//! 3. Exact per-file rules -- one per `gat.lock` entry, except for a file
//!    already covered by (2), so that adding a broad pattern (e.g. a
//!    directory pattern for a large Gat-only tree) never leaves a
//!    redundant exact rule behind.
//!
//! By default (`git.ignore_patterns` empty) every Gat-tracked path without
//! LF gets its own exact exclude rule -- deterministic and safe, since it can
//! never accidentally hide an unrelated untracked file. Patterns are an
//! opt-in, hand-authored optimization for users who want broader coverage
//! (e.g. a large, homogeneous directory) instead of one rule per file.

/// Proven literal-directory coverage plus the unchanged Git-ignore matcher
/// for patterns whose syntax cannot safely be translated to lexical ranges.
/// Pruning is safe because configured patterns cannot un-exclude paths.
pub(super) struct IgnoreCoveragePlan {
    pub excluded: gat_io::DesiredPathExclusions,
    residual: Option<gat_io::GitIgnoreMatcher>,
}

impl IgnoreCoveragePlan {
    pub fn new(patterns: &[gat_core::git_ignore::GitIgnorePattern]) -> Self {
        let mut directories = Vec::new();
        let mut residual = Vec::new();
        for pattern in patterns {
            if let Some(directory) = literal_directory(pattern.as_str()) {
                directories.push(directory);
            } else {
                residual.push(pattern.as_str());
            }
        }
        Self {
            excluded: gat_io::DesiredPathExclusions::new(directories),
            residual: (!residual.is_empty()).then(|| gat_io::GitIgnoreMatcher::new(residual)),
        }
    }

    pub fn residual_matches(&self, path: &gat_core::lexical_path::GatPath) -> bool {
        self.residual
            .as_ref()
            .is_some_and(|search| pattern_matches(search, path.as_str()))
    }

    pub fn covers(&self, path: &gat_core::lexical_path::GatPath) -> bool {
        self.excluded.contains(path) || self.residual_matches(path)
    }
}

fn literal_directory(pattern: &str) -> Option<gat_core::lexical_path::GatPath> {
    let directory = pattern.strip_prefix('/')?.strip_suffix('/')?;
    if directory
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '*' | '?' | '[' | ']' | '\\'))
    {
        return None;
    }
    gat_core::lexical_path::GatPath::parse_canonical(directory).ok()
}

/// Parses `git.ignore_patterns` into a matchable search, reusing
/// `gix_ignore`'s engine (same as `worktree::gatignore`) instead of
/// hand-rolling glob semantics. `None` if `patterns` is empty, so callers
/// can skip the (tiny) matching cost entirely.
#[cfg(test)]
pub fn build_pattern_search(
    patterns: &[gat_core::git_ignore::GitIgnorePattern],
) -> Option<gat_io::GitIgnoreMatcher> {
    if patterns.is_empty() {
        return None;
    }
    Some(gat_io::GitIgnoreMatcher::new(
        patterns
            .iter()
            .map(gat_core::git_ignore::GitIgnorePattern::as_str),
    ))
}

/// Whether `rel` (a root-relative, `/`-separated path) matches a
/// `git.ignore_patterns` entry in `search` -- checking every ancestor
/// directory as well as the file itself, so a directory pattern (e.g.
/// `vendor/`) covers everything underneath it too, the same as
/// `.gitignore` semantics. Patterns are validated exclusion-only (see
/// [`gat_core::git_ignore::GitIgnorePattern`]), so any match here is
/// inherently non-negated.
pub fn pattern_matches(search: &gat_io::GitIgnoreMatcher, rel: &str) -> bool {
    search.is_ignored(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::git_ignore::GitIgnorePattern;

    fn patterns(raw: &[&str]) -> Vec<GitIgnorePattern> {
        raw.iter()
            .map(|p| GitIgnorePattern::parse(*p).unwrap())
            .collect()
    }

    #[test]
    fn coverage_plan_matches_git_ignore_semantics() {
        let paths = [
            "data",
            "data/a",
            "data/sub/x.bin",
            "data0/x",
            "Data/x",
            "other/data/a",
            "emptydir",
            "keep.bin",
            "keep.txt",
            "é/a",
            "a b/x",
            "data/[x]/a",
            "data/a\nb",
        ];
        for rules in [
            vec![],
            vec!["/data/"],
            vec!["/data/sub/", "/data/", "/data/"],
            vec!["*.bin"],
            vec!["/data/", "*.bin", "/é/", "/emptydir/"],
            vec!["data/", "/data/**", "/a b/"],
            vec!["/data/[x]/", "/data//", "/data/./"],
        ] {
            let patterns = patterns(&rules);
            let plan = IgnoreCoveragePlan::new(&patterns);
            let original = build_pattern_search(&patterns);
            for path in paths {
                let path = gat_core::lexical_path::GatPath::parse_canonical(path).unwrap();
                assert_eq!(
                    plan.covers(&path),
                    original
                        .as_ref()
                        .is_some_and(|s| pattern_matches(s, path.as_str())),
                    "{rules:?}: {path}"
                );
            }
        }
    }

    #[test]
    fn ambiguous_directory_rules_remain_residual() {
        for pattern in [
            "data/",
            "/data",
            "/",
            "/data//",
            "/data/../",
            "/data/./",
            "/a b/",
            "/a\tb/",
            "/a*b/",
            "/a?b/",
            "/a[b]/",
            "/a\\b/",
        ] {
            assert!(literal_directory(pattern).is_none(), "{pattern:?}");
        }
        assert!(literal_directory("/data/models/").is_some());
        assert!(literal_directory("/é/").is_some());
    }

    #[test]
    fn pattern_matches_simple_glob() {
        let search = build_pattern_search(&patterns(&["*.safetensors"])).unwrap();
        assert!(pattern_matches(&search, "model.safetensors"));
        assert!(pattern_matches(&search, "nested/model.safetensors"));
        assert!(!pattern_matches(&search, "model.bin"));
    }

    #[test]
    fn pattern_matches_anchored_directory_glob() {
        let search = build_pattern_search(&patterns(&["/artifacts/**/*.bin"])).unwrap();
        assert!(pattern_matches(&search, "artifacts/sub/x.bin"));
        assert!(!pattern_matches(&search, "other/artifacts/sub/x.bin"));
    }

    #[test]
    fn pattern_matches_directory_pattern_covers_descendants() {
        let search = build_pattern_search(&patterns(&["/data/"])).unwrap();
        assert!(pattern_matches(&search, "data/models/x.bin"));
        assert!(!pattern_matches(&search, "data2/x.bin"));
    }

    #[test]
    fn no_patterns_builds_no_search() {
        assert!(build_pattern_search(&[]).is_none());
    }
}
