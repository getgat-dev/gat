//! Resolved path matching shared by CLI and configured selections.
//!
//! Every selection-aware command (`status`, `ls-files`, `diff`, `push`,
//! `fetch`, `pull`, `sync`, and `mount add`) selects tracked paths the
//! same way:
//!
//! ```text
//! CLI or configuration input
//!         ↓ normalize/parse at its input boundary
//!         ↓ resolve defaults in command orchestration
//! Selection { scope, filter }
//!         ↓
//! selection.matches(path)
//! ```
//!
//! A [`Selection`] pairs a [`PathScope`] (the `--path` subtree, `.` meaning
//! the root selection) with an include/exclude `GlobFilter` evaluated
//! **relative to that scope**. Commands never re-implement path-scope + glob
//! matching: they receive one [`Selection`] and call [`Selection::matches`],
//! so `status`, `ls-files`, the transfer commands, `sync`, and mount import
//! cannot drift semantically.
//!
//! `git.ignore_patterns` deliberately stays separate (see
//! [`crate::config::GitConfig`]) because it uses Git-ignore
//! semantics rather than these Gat selection-glob semantics.

use crate::globs::{GatGlobPattern, GlobFilter};
use crate::path_scope::PathScope;

/// A validated path selection: a scope plus an include/exclude filter
/// matched relative to that scope. Input boundaries normalize paths and compile
/// patterns before construction. Configuration precedence belongs in command
/// orchestration; engine APIs receive only this resolved predicate.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    scope: PathScope,
    filter: GlobFilter,
}

impl Selection {
    /// Build a selection from an already-normalized scope and already-
    /// compiled patterns. Ownership moves into the selection without
    /// recompilation or an intermediate vector clone.
    #[must_use]
    pub const fn from_scope_patterns(
        scope: PathScope,
        include: Vec<GatGlobPattern>,
        exclude: Vec<GatGlobPattern>,
    ) -> Self {
        Self {
            scope,
            filter: GlobFilter::from_patterns(include, exclude),
        }
    }

    /// Select the whole repository without consulting configured defaults.
    #[must_use]
    pub fn root() -> Self {
        Self::default()
    }

    /// The normalised scope path (`None` for the repository root), for
    /// callers that narrow by scope separately before applying the filter
    /// (e.g. `gat.lock` streaming with a `SQLite` lexical range, or
    /// content-addressed object selection). The include/exclude filter must
    /// still be applied via [`Selection::matches`].
    #[must_use]
    pub const fn scope_path(&self) -> Option<&crate::lexical_path::GatPath> {
        match &self.scope {
            PathScope::Root => None,
            PathScope::Path(s) => Some(s),
        }
    }

    /// Whether this selection restricts nothing at all: root scope and an
    /// unrestricted include/exclude filter. Lets a caller skip per-path
    /// matching entirely on the common, unrestricted path.
    #[must_use]
    pub const fn is_unrestricted(&self) -> bool {
        matches!(self.scope, PathScope::Root) && self.filter.is_unrestricted()
    }

    #[must_use]
    pub fn include_globs(&self) -> &[GatGlobPattern] {
        self.filter.include_globs()
    }

    /// Whether this selection's include/exclude filter is unrestricted
    /// (no glob patterns at all), independent of its scope -- lets a
    /// caller distinguish "a plain scope path/subtree" from "a
    /// glob-filtered selection" without re-deriving the filter itself
    /// (e.g. deciding whether a `--path` scope also names a single exact
    /// candidate row, which only holds when no include/exclude glob is in
    /// play).
    #[must_use]
    pub const fn has_no_glob_filter(&self) -> bool {
        self.filter.is_unrestricted()
    }

    /// Whether `candidate` (a canonical, root-relative tracked path) is
    /// selected: it must fall under the scope, and its path *relative to
    /// that scope* must satisfy the include/exclude filter (empty include
    /// matches everything; exclude always wins).
    #[must_use]
    pub fn matches(&self, candidate: &crate::lexical_path::GatPath) -> bool {
        self.matches_str(candidate.as_str())
    }

    /// The row-scanning variant of [`Self::matches`], for the one
    /// accepted exception to the "always pass typed `GatPath`" rule:
    /// filtering raw `gat.lock` row text *while it is being parsed*,
    /// before a [`GatPath`](crate::lexical_path::GatPath)/[`crate::lock::Entry`]
    /// exists yet (e.g. `crate::lock::Lock::visit_filtered`/
    /// `parse_filtered`'s `keep` predicate).
    #[must_use]
    pub fn matches_str(&self, candidate: &str) -> bool {
        match self.relative(candidate) {
            Some(rel) => self.filter.matches(rel),
            None => false,
        }
    }

    /// The portion of `candidate` relative to the scope, if `candidate` is
    /// in scope -- exposed for `mount add`, which reparents each selected
    /// source path under a destination `PREFIX`. Root scope yields the
    /// whole path; an exact-file scope yields the empty string; a
    /// directory scope yields the path beneath it. `None` when out of
    /// scope. This is the pre-filter relative path; callers that also need
    /// include/exclude filtering should gate on [`Selection::matches`].
    #[must_use]
    pub fn reparent_relative<'a>(
        &self,
        candidate: &'a crate::lexical_path::GatPath,
    ) -> Option<&'a str> {
        self.relative(candidate.as_str())
    }

    /// The portion of `candidate` relative to the scope, if `candidate` is
    /// in scope. Root scope yields the whole path; an exact-file scope
    /// yields the empty string (matched by an empty include); a directory
    /// scope yields the path beneath it.
    fn relative<'a>(&self, candidate: &'a str) -> Option<&'a str> {
        match &self.scope {
            PathScope::Root => Some(candidate),
            PathScope::Path(s) => {
                if candidate == s.as_str() {
                    Some("")
                } else {
                    candidate
                        .strip_prefix(s.as_str())
                        .and_then(|rest| rest.strip_prefix('/'))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globs::GatGlobPattern;
    use crate::path_scope::normalize_path_scope;
    use std::path::Path;

    fn gp(path: &str) -> crate::lexical_path::GatPath {
        crate::lexical_path::GatPath::parse_canonical(path).unwrap()
    }

    fn sel(path: Option<&str>, include: &[&str], exclude: &[&str]) -> Selection {
        let scope = path
            .map(Path::new)
            .map(normalize_path_scope)
            .transpose()
            .unwrap()
            .unwrap_or(PathScope::Root);
        let include = include
            .iter()
            .map(|pattern| GatGlobPattern::parse(pattern))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let exclude = exclude
            .iter()
            .map(|pattern| GatGlobPattern::parse(pattern))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        Selection::from_scope_patterns(scope, include, exclude)
    }

    // Table-driven semantic coverage: root/nested path, include-only,
    // exclude-only, include+exclude, exclude-wins, and path-relative
    // matching all resolve through the one `Selection::matches` path.
    #[test]
    fn selection_semantics_table() {
        struct Case {
            name: &'static str,
            path: Option<&'static str>,
            include: &'static [&'static str],
            exclude: &'static [&'static str],
            matches: &'static [&'static str],
            rejects: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "root selects everything",
                path: None,
                include: &[],
                exclude: &[],
                matches: &["a.bin", "data/b.bin", "deep/nested/c.bin"],
                rejects: &[],
            },
            Case {
                name: "dot is the root selection",
                path: Some("."),
                include: &[],
                exclude: &[],
                matches: &["a.bin", "data/b.bin"],
                rejects: &[],
            },
            Case {
                name: "nested path scope",
                path: Some("data"),
                include: &[],
                exclude: &[],
                matches: &["data", "data/b.bin", "data/nested/c.bin"],
                rejects: &["a.bin", "data.bin", "database/x.bin"],
            },
            Case {
                name: "include-only at root",
                path: None,
                include: &["**/*.onnx"],
                exclude: &[],
                matches: &["models/a.onnx", "a.onnx"],
                rejects: &["models/a.bin"],
            },
            Case {
                name: "exclude-only at root",
                path: None,
                include: &[],
                exclude: &["tests/**"],
                matches: &["models/a.onnx"],
                rejects: &["tests/a.onnx"],
            },
            Case {
                name: "include+exclude, exclude wins",
                path: None,
                include: &["**/*.onnx"],
                exclude: &["tests/**"],
                matches: &["models/a.onnx"],
                rejects: &["tests/a.onnx", "models/a.bin"],
            },
            Case {
                name: "globs are relative to the path scope",
                path: Some("data"),
                include: &["*.onnx"],
                exclude: &["skip/**"],
                // Matching is on the path *relative to* `data`, and Gat globs
                // are component-aware: `*.onnx` matches only the immediate
                // child `a.onnx`, not `nested/a.onnx`.
                matches: &["data/a.onnx"],
                rejects: &[
                    "data/a.bin",
                    "data/nested/a.onnx",
                    "data/skip/x.onnx",
                    "a.onnx",
                ],
            },
        ];
        for case in cases {
            let selection = sel(case.path, case.include, case.exclude);
            for m in case.matches {
                assert!(
                    selection.matches(&gp(m)),
                    "{}: expected match for {m}",
                    case.name
                );
            }
            for r in case.rejects {
                assert!(
                    !selection.matches(&gp(r)),
                    "{}: expected reject for {r}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn root_selection_is_unrestricted() {
        assert!(Selection::root().is_unrestricted());
        assert!(sel(Some("."), &[], &[]).is_unrestricted());
        assert!(!sel(Some("data"), &[], &[]).is_unrestricted());
        assert!(!sel(None, &["*.bin"], &[]).is_unrestricted());
    }

    #[test]
    fn scope_path_is_none_for_root_and_some_for_nested() {
        assert_eq!(sel(Some("."), &[], &[]).scope_path(), None);
        assert_eq!(sel(None, &[], &[]).scope_path(), None);
        assert_eq!(
            sel(Some("data/models"), &[], &[]).scope_path(),
            Some(&gp("data/models"))
        );
    }

    #[test]
    fn recursive_globs_match_nested_paths_relative_to_scope() {
        let selection = sel(Some("data"), &["**/*.onnx"], &[]);
        assert!(selection.matches(&gp("data/a.onnx")));
        assert!(selection.matches(&gp("data/nested/a.onnx")));
        assert!(selection.matches(&gp("data/deep/nested/a.onnx")));
        assert!(!selection.matches(&gp("other/a.onnx")));
    }

    #[test]
    fn backslash_globs_are_normalized_relative_to_scope() {
        let selection = sel(Some(r".\data"), &[r"nested\*.onnx"], &[]);
        assert!(selection.matches(&gp("data/nested/a.onnx")));
        assert!(!selection.matches(&gp("data/nested/deep/a.onnx")));
    }
}
