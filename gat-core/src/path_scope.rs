//! Shared path-scope primitives used by every path-scoped command
//! (`add`, `rm`, `status`, `ls-files`, `mv`, `push`, `fetch`, `pull`,
//! `sync`, `diff --path`) to normalise user-supplied path
//! arguments into a canonical scope, and to match tracked paths against
//! that scope.
//!
//! A [`PathScope`] is either `Root` (the whole repo, expressed as `.`,
//! `./`, or the empty string on the CLI) or a canonical root-relative path
//! that matches itself and everything nested beneath it, component-aware so
//! `data` matches `data/a.bin` but not `data.bin` or `database/x.bin`.
//!
//! The selection-aware read/transfer/sync commands combine a [`PathScope`]
//! with compiled include/exclude patterns through the single
//! [`Selection`](crate::selection::Selection)
//! type; this module only owns the scope half of that pipeline plus the
//! `add`/`rm` glob-metacharacter detection, both of which pre-date and feed
//! `Selection`.

use crate::lexical_path::{GatPath, LexicalPath, LexicalPathError, normalize_relative_path};
use std::path::Path;

/// A canonical path scope: either the entire repo root, or a normalised
/// root-relative path prefix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PathScope {
    /// The whole repository root (CLI `.`, `./`, empty string).
    #[default]
    Root,
    /// A canonical root-relative, `/`-separated path (never `.`, never empty).
    Path(GatPath),
}

/// Normalise a user-supplied path argument into a [`PathScope`], mapping
/// `.`, `./`, and the empty string to `Root`, and everything else through
/// `normalize_relative_path` (which rejects `..`, absolute paths, etc.).
///
/// This module has no path-scope-specific failure mode of its own -- every
/// way a path can be rejected here is really a lexical-path rejection, so
/// it surfaces [`LexicalPathError`] directly rather than defining a
/// distinct wrapper error.
pub fn normalize_path_scope<P: AsRef<Path>>(path: P) -> Result<PathScope, LexicalPathError> {
    match normalize_relative_path(path)? {
        LexicalPath::Empty => Ok(PathScope::Root),
        LexicalPath::Path(path) => Ok(PathScope::Path(path)),
    }
}

/// Whether a canonical tracked `path` falls under the given `scope`.
#[must_use]
pub fn matches_scope(candidate: &GatPath, scope: &PathScope) -> bool {
    match scope {
        PathScope::Root => true,
        PathScope::Path(s) => {
            let candidate = candidate.as_str();
            let s = s.as_str();
            candidate == s
                || candidate
                    .strip_prefix(s)
                    .is_some_and(|rest| rest.starts_with('/'))
        }
    }
}

/// Check whether a normalised path argument contains glob metacharacters.
#[must_use]
pub fn has_glob_metacharacters(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gp(s: &str) -> GatPath {
        GatPath::parse_canonical(s).unwrap()
    }

    #[test]
    fn dot_normalizes_to_root() {
        assert_eq!(normalize_path_scope(".").unwrap(), PathScope::Root);
        assert_eq!(normalize_path_scope("./").unwrap(), PathScope::Root);
        assert_eq!(normalize_path_scope("././").unwrap(), PathScope::Root);
    }

    #[test]
    fn empty_string_normalizes_to_root() {
        // Empty OsStr
        assert_eq!(
            normalize_path_scope(std::path::Path::new("")).unwrap(),
            PathScope::Root
        );
    }

    #[test]
    fn default_path_scope_is_root() {
        assert_eq!(PathScope::default(), PathScope::Root);
    }

    #[test]
    fn data_normalizes_to_path() {
        assert_eq!(
            normalize_path_scope("data").unwrap(),
            PathScope::Path(gp("data"))
        );
    }

    #[test]
    fn data_slash_normalizes_same_as_data() {
        assert_eq!(
            normalize_path_scope("data/").unwrap(),
            normalize_path_scope("data").unwrap(),
        );
    }

    #[test]
    fn dot_data_slash_normalizes_same_as_data() {
        assert_eq!(
            normalize_path_scope("./data/").unwrap(),
            normalize_path_scope("data").unwrap(),
        );
    }

    #[test]
    fn root_matches_everything() {
        assert!(matches_scope(&gp("data/a.bin"), &PathScope::Root));
        assert!(matches_scope(&gp("anything"), &PathScope::Root));
    }

    #[test]
    fn scope_data_matches_data_and_descendants_but_not_siblings() {
        let scope = PathScope::Path(gp("data"));
        assert!(matches_scope(&gp("data"), &scope));
        assert!(matches_scope(&gp("data/a.bin"), &scope));
        assert!(matches_scope(&gp("data/nested/b.bin"), &scope));
        assert!(!matches_scope(&gp("data.bin"), &scope));
        assert!(!matches_scope(&gp("database/a.bin"), &scope));
    }

    #[test]
    fn has_glob_metacharacters_detects_wildcards() {
        assert!(has_glob_metacharacters("*.bin"));
        assert!(has_glob_metacharacters("file?.bin"));
        assert!(has_glob_metacharacters("file[1].bin"));
        assert!(!has_glob_metacharacters("data/plain.bin"));
    }

    #[test]
    fn dotdot_rejected() {
        assert!(normalize_path_scope("..").is_err());
    }

    #[test]
    fn absolute_path_rejected() {
        assert!(normalize_path_scope("/etc/passwd").is_err());
        assert!(normalize_path_scope("//").is_err());
        assert!(normalize_path_scope("////").is_err());
    }

    #[test]
    fn windows_rooted_and_unc_inputs_are_rejected() {
        for input in ["\\foo", "\\\\server\\share", "//server/share"] {
            assert!(
                normalize_path_scope(input).is_err(),
                "{input:?} should be rejected"
            );
        }
    }

    /// Gat path identity is host-independent; a leading segment that
    /// merely looks like a Windows drive letter is an ordinary path
    /// segment, not a rejected drive-relative spelling. A
    /// `\`-spelled input still normalizes to `/` like any other separator.
    #[test]
    fn windows_drive_like_leading_segments_are_valid_paths() {
        assert_eq!(
            normalize_path_scope("C:foo").unwrap(),
            PathScope::Path(gp("C:foo"))
        );
        assert_eq!(
            normalize_path_scope("C:/foo").unwrap(),
            PathScope::Path(gp("C:/foo"))
        );
        assert_eq!(
            // hygiene-ok: pure string literal exercising backslash normalization; no real Windows path is touched.
            normalize_path_scope("C:\\foo").unwrap(),
            PathScope::Path(gp("C:/foo"))
        );
    }

    #[test]
    fn dotdot_rejection_is_a_parent_traversal_variant() {
        assert!(matches!(
            normalize_path_scope("..").unwrap_err(),
            crate::lexical_path::LexicalPathError::ParentTraversal { .. }
        ));
    }

    #[test]
    fn absolute_path_rejection_is_a_not_relative_variant() {
        assert!(matches!(
            normalize_path_scope("/etc/passwd").unwrap_err(),
            crate::lexical_path::LexicalPathError::NotRelative { .. }
        ));
    }
}
