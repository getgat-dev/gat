//! Shared include/exclude glob-pattern matching, used everywhere a
//! `gat.lock`-style root-relative path needs to be selected by a set of
//! user-supplied globs: `gat mount add`'s `--include`/`--exclude`, and
//! the shared `selections.<name>.include`/`selections.<name>.exclude` defaults.

use crate::lexical_path::LexicalPathError;
use crate::lexical_path::normalize_glob_pattern_with_meta;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::{Arc, OnceLock};

/// A user-supplied `--include`/`--exclude` glob pattern is not usable,
/// either because it fails the same lexical checks as any other Gat path
/// (see [`LexicalPathError`]) or because the glob syntax itself (the part
/// after lexical normalization -- `*`, `?`, `[...]`, ...) is malformed.
///
/// Deliberately keeps the offending `pattern` alongside each variant (not
/// just the underlying parser error) so a caller/diagnostic can name
/// *which* of possibly several `--include`/`--exclude` values was
/// rejected, without re-deriving it from the technical source.
#[derive(Debug, thiserror::Error)]
pub enum GlobError {
    /// The pattern fails Gat's lexical path rules (rooted, contains `..`,
    /// non-UTF-8, ...), checked before glob syntax is even considered.
    #[error("`{pattern}` is not a valid path pattern")]
    InvalidLexicalSyntax {
        pattern: String,
        #[source]
        source: LexicalPathError,
    },
    /// The pattern is lexically fine but its glob syntax (metacharacters,
    /// character classes, ...) does not parse.
    #[error("`{pattern}` is not a valid glob pattern")]
    InvalidGlobSyntax {
        pattern: String,
        #[source]
        source: glob::PatternError,
    },
}

pub type Result<T> = std::result::Result<T, GlobError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobBound<'a> {
    Any,
    Prefix(&'a str),
    Exact(&'a str),
}

/// A normalized, validated, and compiled Gat `--include`/`--exclude` glob
/// pattern: the same lexical checks any other Gat path goes through
/// (rooted, `..`, non-UTF-8, ...), the glob syntax itself (`*`, `?`,
/// `[...]`, ...), and the `glob::Pattern` match engine compiled from it
/// are all produced exactly once here -- `raw text -> normalize -> compile
/// once -> reuse matcher` -- rather than validated and discarded only to
/// be recompiled later, or recompiled again on every clone.
///
/// The heavier state (normalized text, first-meta index, compiled
/// matcher) lives behind an `Arc`, so [`Clone`] is a cheap refcount bump
/// -- config structs, `Selection`, and `GlobFilter` can all hold/clone
/// this type freely without recompiling or re-validating anything.
/// `PartialEq`/`Eq`/`PartialOrd`/`Ord`/`Hash` are implemented over the
/// normalized text alone, independent of compiled matcher state and lazy
/// directory-prefix caches.
///
/// Serialized as a scalar YAML string (the normalized text) wherever it's
/// persisted in `gat.yaml` (`selections.<name>.include`/`selections.<name>.exclude`,
/// `mounts.<name>.include`/`exclude`); deserializing re-validates and
/// recompiles, the same as [`Self::parse`].
#[derive(Clone, Debug)]
pub struct GatGlobPattern(Arc<GatGlobPatternInner>);

#[derive(Debug)]
struct GatGlobPatternInner {
    normalized: String,
    first_meta: Option<usize>,
    compiled: glob::Pattern,
    directory_prefixes: OnceLock<Vec<glob::Pattern>>,
}

impl GatGlobPattern {
    /// Normalize, validate, and compile one user-supplied pattern in a
    /// single pass: lexical normalization, then a glob-syntax check, then
    /// the `glob::Pattern` matcher itself -- so an invalid pattern is
    /// rejected here, once, and the compiled matcher is never rebuilt
    /// later.
    pub fn parse(pattern: &str) -> Result<Self> {
        let normalized = normalize_glob_pattern_with_meta(pattern).map_err(|source| {
            GlobError::InvalidLexicalSyntax {
                pattern: pattern.to_string(),
                source,
            }
        })?;
        Self::from_normalized(normalized.pattern, normalized.first_meta)
    }

    /// Compile glob syntax from a validated canonical path without repeating
    /// lexical normalization. Wildcards in `path` are interpreted as glob syntax.
    /// Invalid glob syntax still returns an error.
    ///
    /// Raw strings must go through [`Self::parse`] instead:
    ///
    /// ```compile_fail
    /// use gat_core::globs::GatGlobPattern;
    /// GatGlobPattern::from_path(&"../*.bin".to_string());
    /// ```
    pub fn from_path(path: &crate::lexical_path::GatPath) -> Result<Self> {
        let first_meta = first_meta_index(path.as_str());
        Self::from_normalized(path.to_string(), first_meta)
    }

    fn from_normalized(normalized: String, first_meta: Option<usize>) -> Result<Self> {
        let compiled =
            glob::Pattern::new(&normalized).map_err(|source| GlobError::InvalidGlobSyntax {
                pattern: normalized.clone(),
                source,
            })?;
        Ok(Self(Arc::new(GatGlobPatternInner {
            normalized,
            first_meta,
            compiled,
            directory_prefixes: OnceLock::new(),
        })))
    }

    /// The normalized Gat pattern text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0.normalized
    }

    /// Whether this pattern matches the already-canonical Gat path `path`.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.0.compiled.matches_with(path, match_options())
    }

    /// Whether a descendant of this canonical directory could match the glob.
    /// Unlike the lexical candidate bound, this matches complete directory
    /// components, including recursive `**`, without inspecting their contents.
    #[must_use]
    pub fn can_match_descendant(&self, directory: &str) -> bool {
        if directory.is_empty() {
            return true;
        }
        let prefixes = self.0.directory_prefixes.get_or_init(|| {
            self.as_str()
                .match_indices('/')
                // A separator inside a character class is not a component
                // boundary; its incomplete prefix will not compile.
                .filter_map(|(index, _)| glob::Pattern::new(&self.as_str()[..index]).ok())
                .collect()
        });
        prefixes
            .iter()
            .any(|prefix| prefix.matches_with(directory, match_options()))
            || (self.as_str().rsplit('/').next() == Some("**") && self.matches(directory))
    }

    /// The safe lexical candidate bound for this pattern:
    /// - `Any`: first character is a glob metacharacter;
    /// - `Prefix`: fixed text before the first metacharacter;
    /// - `Exact`: no metacharacter at all.
    #[must_use]
    pub fn bound(&self) -> GlobBound<'_> {
        match self.0.first_meta {
            None => GlobBound::Exact(self.0.normalized.as_str()),
            Some(0) => GlobBound::Any,
            Some(n) => GlobBound::Prefix(&self.0.normalized[..n]),
        }
    }
}

impl PartialEq for GatGlobPattern {
    fn eq(&self, other: &Self) -> bool {
        self.0.normalized == other.0.normalized
    }
}

impl Eq for GatGlobPattern {}

impl PartialOrd for GatGlobPattern {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GatGlobPattern {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.normalized.cmp(&other.0.normalized)
    }
}

impl std::hash::Hash for GatGlobPattern {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.normalized.hash(state);
    }
}

impl std::fmt::Display for GatGlobPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.normalized)
    }
}

impl AsRef<str> for GatGlobPattern {
    fn as_ref(&self) -> &str {
        &self.0.normalized
    }
}

impl Serialize for GatGlobPattern {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.normalized)
    }
}

impl<'de> Deserialize<'de> for GatGlobPattern {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Shared match options for every Gat glob engine: case-sensitive and
/// component-aware (`*`, `?`, and character classes never cross `/`, while
/// `**` still spans zero or more path components).
const fn match_options() -> glob::MatchOptions {
    glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    }
}

/// A compiled set of include/exclude glob patterns, ready to be matched
/// against many paths without recompiling a pattern per path/call --
/// [`glob::Pattern::new`] parses the pattern text once here, so
/// [`Self::matches`] only ever does the (cheap) match itself.
#[derive(Clone, Debug, Default)]
pub(crate) struct GlobFilter {
    include: Vec<GatGlobPattern>,
    exclude: Vec<GatGlobPattern>,
}

impl GlobFilter {
    pub(crate) const fn from_patterns(
        include: Vec<GatGlobPattern>,
        exclude: Vec<GatGlobPattern>,
    ) -> Self {
        Self { include, exclude }
    }

    /// Whether `rel_path` is selected: everything matches when `include`
    /// is empty, otherwise only paths matching some `include` pattern;
    /// `exclude` always wins over `include`.
    pub fn matches(&self, rel_path: &str) -> bool {
        if !self.include.is_empty() && !any_match(&self.include, rel_path) {
            return false;
        }
        !any_match(&self.exclude, rel_path)
    }

    /// Whether this filter selects everything unconditionally (no
    /// `include`/`exclude` patterns at all) -- lets a caller skip building
    /// a [`GlobFilter`] (or calling [`Self::matches`] per path) entirely
    /// on the common, unrestricted path.
    pub const fn is_unrestricted(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    pub(crate) fn include_globs(&self) -> &[GatGlobPattern] {
        &self.include
    }
}

fn any_match(patterns: &[GatGlobPattern], rel_path: &str) -> bool {
    patterns.iter().any(|pattern| pattern.matches(rel_path))
}

fn first_meta_index(pattern: &str) -> Option<usize> {
    pattern
        .char_indices()
        .find_map(|(i, ch)| matches!(ch, '*' | '?' | '[').then_some(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_selects_everything() {
        let filter = GlobFilter::from_patterns(Vec::new(), Vec::new());
        assert!(filter.is_unrestricted());
        assert!(filter.matches("anything/at/all.bin"));
    }

    #[test]
    fn include_restricts_to_matching_paths_only() {
        let filter = GlobFilter::from_patterns(
            vec![GatGlobPattern::parse("**/*.onnx").unwrap()],
            Vec::new(),
        );
        assert!(!filter.is_unrestricted());
        assert!(filter.matches("models/a.onnx"));
        assert!(!filter.matches("models/a.bin"));
    }

    #[test]
    fn exclude_always_wins_over_include() {
        let filter = GlobFilter::from_patterns(
            vec![GatGlobPattern::parse("**/*.onnx").unwrap()],
            vec![GatGlobPattern::parse("tests/**").unwrap()],
        );
        assert!(filter.matches("models/a.onnx"));
        assert!(!filter.matches("tests/a.onnx"));
    }

    #[test]
    fn exclude_with_no_include_excludes_from_everything() {
        let filter =
            GlobFilter::from_patterns(Vec::new(), vec![GatGlobPattern::parse("tests/**").unwrap()]);
        assert!(filter.matches("models/a.onnx"));
        assert!(!filter.matches("tests/a.onnx"));
    }

    #[test]
    fn rooted_or_parent_traversing_pattern_is_a_lexical_syntax_variant() {
        let rooted = GatGlobPattern::parse("/etc/passwd").unwrap_err();
        assert!(matches!(rooted, GlobError::InvalidLexicalSyntax { .. }));

        let traversal = GatGlobPattern::parse("../*.bin").unwrap_err();
        assert!(matches!(traversal, GlobError::InvalidLexicalSyntax { .. }));
    }

    // Failure-mapping tests ensure parser-library wording never leaks into
    // rendered failures. They live at the root-crate boundary because
    // `Failure` is not a `gat-core` concept.

    #[test]
    fn compile_from_path_matches_normalized_text() {
        let via_compile = GatGlobPattern::parse("data/**/*.bin").unwrap();
        let via_normalized = GatGlobPattern::from_path(
            &crate::lexical_path::GatPath::parse_canonical("data/**/*.bin").unwrap(),
        )
        .unwrap();
        assert_eq!(via_compile.as_str(), via_normalized.as_str());
        assert!(via_normalized.matches("data/deep/a.bin"));
        assert_eq!(via_compile.bound(), via_normalized.bound());
    }

    #[test]
    fn compile_from_path_still_rejects_invalid_glob_syntax() {
        let path = crate::lexical_path::GatPath::parse_canonical("data/[").unwrap();
        assert!(matches!(
            GatGlobPattern::from_path(&path),
            Err(GlobError::InvalidGlobSyntax { .. })
        ));
    }

    #[test]
    fn single_star_is_non_recursive_but_double_star_is_recursive() {
        let flat = GatGlobPattern::parse("*.bin").unwrap();
        let recursive = GatGlobPattern::parse("**/*.bin").unwrap();

        assert!(flat.matches("a.bin"));
        assert!(!flat.matches("data/a.bin"));
        assert!(recursive.matches("a.bin"));
        assert!(recursive.matches("data/a.bin"));
        assert!(recursive.matches("data/deep/a.bin"));
    }

    #[test]
    fn question_mark_and_character_classes_are_component_aware() {
        let question = GatGlobPattern::parse("data/file-?.bin").unwrap();
        let classes = GatGlobPattern::parse("data/file-[ab].bin").unwrap();

        assert!(question.matches("data/file-a.bin"));
        assert!(!question.matches("data/nested/file-a.bin"));
        assert!(classes.matches("data/file-a.bin"));
        assert!(classes.matches("data/file-b.bin"));
        assert!(!classes.matches("data/file-c.bin"));
        assert!(!classes.matches("data/nested/file-a.bin"));
    }

    #[test]
    fn matching_is_case_sensitive() {
        let pattern = GatGlobPattern::parse("*.bin").unwrap();
        assert!(pattern.matches("model.bin"));
        assert!(!pattern.matches("MODEL.BIN"));
    }

    #[test]
    fn patterns_normalize_backslashes_and_mixed_separators_once() {
        let pattern = GatGlobPattern::parse(r".\data\**\*.bin").unwrap();
        assert_eq!(pattern.as_str(), "data/**/*.bin");
        assert!(pattern.matches("data/a.bin"));
        assert!(pattern.matches("data/nested/a.bin"));
    }

    #[test]
    fn descendant_matching_respects_complete_components_and_recursive_globs() {
        for (pattern, directory, expected) in [
            ("d?/*.bin", "data", false),
            ("d?/*.bin", "da", true),
            ("d?/*.bin", "da/nested", false),
            ("d[ab]/*.bin", "data", false),
            ("d[ab]/*.bin", "da", true),
            ("d[[]/*.bin", "d[", true),
            ("d*/models/*.bin", "data/other", false),
            ("d*/models/*.bin", "data", true),
            ("d*/models/*.bin", "data/models", true),
            ("*.bin", "data", false),
            ("**/*.bin", "data/nested", true),
            ("d?/**/*.bin", "data", false),
            ("d?/**/*.bin", "da/nested", true),
            ("da/**/models/*.bin", "da/other", true),
            ("da/**", "da", true),
            ("da/**", "da/nested", true),
            ("**", "data/nested", true),
            ("data", "data", false),
            ("*.bin", "", true),
            ("D?/*.bin", "da", false),
            ("d?/*.bin", "dé", true),
        ] {
            let glob = GatGlobPattern::parse(pattern).unwrap();
            assert_eq!(
                glob.can_match_descendant(directory),
                expected,
                "{pattern:?} below {directory:?}"
            );
        }
    }

    #[test]
    fn glob_bound_uses_tight_lexical_prefixes() {
        assert_eq!(
            GatGlobPattern::parse("*.bin").unwrap().bound(),
            GlobBound::Any
        );
        assert_eq!(
            GatGlobPattern::parse("**/*.bin").unwrap().bound(),
            GlobBound::Any
        );
        assert_eq!(
            GatGlobPattern::parse("model-*.bin").unwrap().bound(),
            GlobBound::Prefix("model-")
        );
        assert_eq!(
            GatGlobPattern::parse("data/*.bin").unwrap().bound(),
            GlobBound::Prefix("data/")
        );
        assert_eq!(
            GatGlobPattern::parse("data/models/**/*.bin")
                .unwrap()
                .bound(),
            GlobBound::Prefix("data/models/")
        );
        assert_eq!(
            GatGlobPattern::parse("data/models/model-?.onnx")
                .unwrap()
                .bound(),
            GlobBound::Prefix("data/models/model-")
        );
        assert_eq!(
            GatGlobPattern::parse("data/a.bin").unwrap().bound(),
            GlobBound::Exact("data/a.bin")
        );
    }

    #[test]
    fn bound_prefix_is_borrowed_from_normalized_text() {
        let glob = GatGlobPattern::parse("data/models/model-?.onnx").unwrap();
        let GlobBound::Prefix(prefix) = glob.bound() else {
            panic!("expected prefix bound");
        };
        let offset = prefix.as_ptr() as usize - glob.as_str().as_ptr() as usize;
        assert_eq!(offset, 0);
        assert_eq!(prefix.len(), "data/models/model-".len());
    }

    #[test]
    fn compile_and_normalize_glob_pattern_match() {
        assert_eq!(
            crate::lexical_path::normalize_glob_pattern(r".\data\**\*.bin").unwrap(),
            GatGlobPattern::parse(r".\data\**\*.bin").unwrap().as_str()
        );
    }

    #[test]
    fn gat_glob_pattern_parse_normalizes_and_validates() {
        let pattern = GatGlobPattern::parse(r".\data\**\*.bin").unwrap();
        assert_eq!(pattern.as_str(), "data/**/*.bin");
        assert_eq!(pattern.to_string(), "data/**/*.bin");

        let err = GatGlobPattern::parse("[").unwrap_err();
        assert!(matches!(err, GlobError::InvalidGlobSyntax { .. }));

        let err = GatGlobPattern::parse("../escape.bin").unwrap_err();
        assert!(matches!(err, GlobError::InvalidLexicalSyntax { .. }));
    }

    #[test]
    fn cloning_a_pattern_is_a_cheap_arc_bump_not_a_recompile() {
        // Collapsing `GatGlob` into `GatGlobPattern` means the compiled
        // matcher now lives behind an `Arc` inside the cloned value
        // itself: a clone must match immediately, without re-parsing or
        // recompiling anything.
        let pattern = GatGlobPattern::parse("**/*.bin").unwrap();
        let cloned = pattern.clone();
        assert_eq!(pattern, cloned);
        assert!(cloned.matches("data/a.bin"));
    }

    #[test]
    fn glob_filter_reuses_compiled_patterns() {
        let include = vec![GatGlobPattern::parse("**/*.onnx").unwrap()];
        let exclude = vec![GatGlobPattern::parse("tests/**").unwrap()];
        let filter = GlobFilter::from_patterns(include, exclude);
        assert!(filter.matches("models/a.onnx"));
        assert!(!filter.matches("tests/a.onnx"));
    }

    #[test]
    fn gat_glob_pattern_serde_round_trips_through_a_plain_yaml_string() {
        let pattern = GatGlobPattern::parse("**/*.bin").unwrap();
        let yaml = yaml_serde::to_string(&pattern).unwrap();
        assert_eq!(yaml.trim(), "'**/*.bin'");
        let back: GatGlobPattern = yaml_serde::from_str(&yaml).unwrap();
        assert_eq!(back, pattern);
    }

    #[test]
    fn gat_glob_pattern_deserialize_rejects_invalid_glob_syntax() {
        let err = yaml_serde::from_str::<GatGlobPattern>("\"[\"").unwrap_err();
        assert!(err.to_string().contains("not a valid glob pattern"));
    }
}
