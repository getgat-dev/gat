//! Host-independent lexical parsing for Gat's canonical relative paths and
//! glob patterns.

use std::path::Path;

/// Syntax errors from host-independent path and glob normalization.
/// Validation does not access the filesystem.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LexicalPathError {
    /// The input normalized to the repository root itself (`.`, `./`, or
    /// empty) where the caller requires a non-root, concrete path (e.g. a
    /// route's `path` must name something more specific than "everything").
    #[error("`{input}` must not be the repository root")]
    EmptyPath { input: String },
    /// The input's raw bytes are not valid UTF-8, so it cannot even be
    /// examined by Gat's lexical parser (which operates on `&str`).
    #[error("`{display}` contains non-UTF-8 characters")]
    NonUtf8 { display: String },
    /// A rooted or UNC-style spelling was supplied where Gat requires a
    /// relative path/pattern.
    #[error("`{input}` must be a relative path")]
    NotRelative { input: String },
    /// A `..` component would escape the repository root.
    #[error("`{input}` escapes the repository root (contains `..`)")]
    ParentTraversal { input: String },
}

pub type Result<T> = std::result::Result<T, LexicalPathError>;

/// A validated, canonical, root-relative, `/`-separated Gat tracked-path.
///
/// `GatPath` is the sole representation of a canonical Gat path: it is never
/// empty, never rooted, never contains `.`/`..` components, and never uses
/// `\` as a separator. It is deliberately *not* `AsRef<Path>` or otherwise
/// implicitly convertible to a host filesystem path -- Gat path identity is
/// host-independent, and converting a `GatPath` into a concrete
/// filesystem location must go through an explicit resolver (see
/// `gat_io::WorktreeClient`) rather than an implicit `Path`/`PathBuf`
/// conversion.
///
/// There are exactly two *kinds* of construction: lenient normalization of
/// user/CLI/config-supplied input, and strict validation of text already
/// claimed to be canonical (persisted `gat.lock` rows, config file values,
/// SQLite/JSON rows) -- which comes in two forms depending on whether the
/// caller holds a borrowed `&str` or an owned `String` it can reuse:
/// - [`GatPath::normalize`] for user/CLI/config-supplied [`Path`] input that
///   may need lexical normalization (`./`, `\`, redundant separators, etc.).
/// - [`GatPath::parse_canonical`] for borrowed text that is already claimed
///   to be canonical -- a strict parse that rejects anything not already
///   canonical rather than re-normalizing it, allocating exactly once.
/// - [`GatPath::from_canonical_string`] for an owned `String` that is
///   already claimed to be canonical (e.g. read back from SQLite/JSON) --
///   the same strict validation as `parse_canonical`, but retaining the
///   caller's existing allocation instead of copying it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GatPath(String);

impl GatPath {
    /// Normalize a user-supplied path into a canonical [`GatPath`], using
    /// Gat's own lexical parser rather than host-dependent `std::path`
    /// component classification. Returns [`LexicalPathError::EmptyPath`] if
    /// the input normalizes to the repository root itself (`.`, `./`, or
    /// empty); callers that need to allow the root (e.g. path-scope
    /// normalization) should use `normalize_relative_path` directly and
    /// handle `LexicalPath::Empty` themselves.
    pub fn normalize<P: AsRef<Path>>(path: P) -> Result<Self> {
        match normalize_relative_path(&path)? {
            LexicalPath::Path(path) => Ok(path),
            LexicalPath::Empty => {
                let display = path.as_ref().to_string_lossy().into_owned();
                Err(LexicalPathError::EmptyPath { input: display })
            }
        }
    }

    /// Strictly parse text that is already claimed to be canonical (e.g. a
    /// persisted `gat.lock` row, a config file value, or a `SQLite` row) into
    /// a [`GatPath`]. Unlike [`GatPath::normalize`], this does not
    /// re-normalize `./`, `\`, or redundant separators -- any such spelling
    /// is rejected, since persisted/internal text is expected to already be
    /// in exactly the canonical form `GatPath` produces. Validates without
    /// building a second temporary string (see `validate_canonical_str`),
    /// then performs exactly one allocation for the owned value.
    pub fn parse_canonical(raw: &str) -> Result<Self> {
        validate_canonical_str(raw)?;
        Ok(Self(raw.to_string()))
    }

    /// Strictly validate an owned, already-canonical `String` (e.g. read
    /// back from `SQLite`, JSON, or YAML) and retain that exact allocation as
    /// the resulting [`GatPath`] on success -- no replacement allocation.
    /// Rejects the same non-canonical spellings [`GatPath::parse_canonical`]
    /// does.
    pub fn from_canonical_string(raw: String) -> Result<Self> {
        validate_canonical_str(&raw)?;
        Ok(Self(raw))
    }

    /// Wraps `raw` as a [`GatPath`] without re-validating it, for callers
    /// that have already established canonicality by some other proof
    /// (e.g. `gat.lock` row parsing's non-allocating fast-path check, or a
    /// prior successful [`GatPath::normalize`]/[`GatPath::parse_canonical`]
    /// whose exact output text is being re-wrapped). Debug-asserts the
    /// invariant so a caller that gets the proof wrong is still caught in
    /// tests/debug builds, without paying for a second full validation
    /// scan in release. Crate-private: this trades validation for
    /// allocation-count, so it must never be reachable from a genuinely
    /// untrusted or unvalidated string.
    pub(crate) fn from_validated_canonical(raw: String) -> Self {
        debug_assert!(
            validate_canonical_str(&raw).is_ok(),
            "from_validated_canonical called with a non-canonical path: {raw:?}"
        );
        Self(raw)
    }

    /// The canonical `/`-separated path text. There is no implicit
    /// conversion to a host [`Path`]/[`std::path::PathBuf`] -- see
    /// `gat_io::WorktreeClient` for the explicit resolver boundary.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `self` is exactly `prefix` or a directory descendant of it
    /// (`prefix/...`) -- the same lexical rule
    /// `crate::lock::path_matches_scope` uses for scope matching,
    /// exposed here for internal call sites (e.g. `gat mv`'s desired/
    /// materialized-state prefix rewrite) that need it without holding a
    /// full `PathScope`.
    #[must_use]
    pub fn is_or_under(&self, prefix: &Self) -> bool {
        self.0 == prefix.0
            || self
                .0
                .strip_prefix(prefix.0.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Rewrites the leading `old_prefix` component of `self` to
    /// `new_prefix`. Only valid where the caller has already established
    /// `self.is_or_under(old_prefix)` (debug-asserted, and re-checked in
    /// release builds by requiring the stripped remainder to land exactly
    /// on a `self == old_prefix` or `/`-separated boundary, so a release
    /// build can never silently slice an unrelated path) -- e.g. `gat mv`
    /// remapping every desired/materialized row under a moved directory.
    /// This is a narrow semantic rebase primitive for state owners that have
    /// already established the prefix relationship, not a general-purpose
    /// path-editing API.
    #[must_use]
    pub fn with_replaced_prefix(&self, old_prefix: &Self, new_prefix: &Self) -> Self {
        debug_assert!(
            self.is_or_under(old_prefix),
            "with_replaced_prefix requires self to be old_prefix or nested under it"
        );
        let rest = match self.0.strip_prefix(old_prefix.0.as_str()) {
            Some("") => "",
            Some(rest) if rest.starts_with('/') => rest,
            _ => {
                // Release-build guard for the debug_assert above: refuse to
                // slice a path that isn't actually old_prefix or nested
                // under it, rather than producing a corrupted path.
                return self.clone();
            }
        };
        let mut joined = String::with_capacity(new_prefix.0.len() + rest.len());
        joined.push_str(&new_prefix.0);
        joined.push_str(rest);
        Self(joined)
    }

    /// Joins `self` with an already-canonical, non-empty relative path
    /// suffix (e.g. the borrowed remainder `crate::selection::Selection::reparent_relative`
    /// returns -- a substring of an already-validated [`GatPath`] taken
    /// after a `/` boundary, so it is structurally canonical without
    /// re-validation) into a single canonical `GatPath` (`self/rel`).
    /// Canonicality follows structurally from both operands already being
    /// canonical, so the result is built with a single pre-sized
    /// allocation and is never reparsed. This is a narrow semantic primitive
    /// for owners (e.g. mount-journal staging) that already hold a validated
    /// canonical suffix, not a general string-concatenation API.
    #[must_use]
    pub fn join_rel(&self, rel: &str) -> Self {
        let mut joined = String::with_capacity(self.0.len() + 1 + rel.len());
        joined.push_str(&self.0);
        joined.push('/');
        joined.push_str(rel);
        Self(joined)
    }
}

/// A canonical Gat subpath: either the repository/mount-source root
/// (spelled `"."` wherever it is persisted or displayed), or a non-root
/// [`GatPath`]. This is the shared vocabulary for path-shaped config/domain
/// fields that must be able to name "everything under here" as well as a
/// concrete nested path (e.g. `MountConfig.path`, the source subpath a
/// mount pulls from) -- the same root/non-root distinction
/// [`crate::path_scope::PathScope`] already draws for scope
/// arguments, but for a persisted, serialized field rather than a
/// selection-time argument.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GatSubpath {
    /// The root itself -- persisted/displayed as `"."`.
    #[default]
    Root,
    /// A canonical, non-root subpath.
    Path(GatPath),
}

impl GatSubpath {
    /// Normalize user/CLI/config-supplied [`Path`] input into a
    /// [`GatSubpath`], mapping `.`, `./`, and the empty string to
    /// [`GatSubpath::Root`] and everything else through
    /// [`GatPath::normalize`]'s lexical rules.
    pub fn normalize<P: AsRef<Path>>(path: P) -> Result<Self> {
        match normalize_relative_path(path)? {
            LexicalPath::Empty => Ok(Self::Root),
            LexicalPath::Path(path) => Ok(Self::Path(path)),
        }
    }

    /// Strictly parse text that is already claimed to be canonical (e.g. a
    /// config file value): accepts exactly `"."` for root, and otherwise
    /// delegates to [`GatPath::parse_canonical`].
    pub fn parse_canonical(raw: &str) -> Result<Self> {
        if raw == "." {
            return Ok(Self::Root);
        }
        GatPath::parse_canonical(raw).map(GatSubpath::Path)
    }

    /// Strictly parse an owned, already-canonical `String`, retaining its
    /// allocation for the non-root case instead of copying it. Accepts
    /// exactly `"."` for root, and otherwise delegates to
    /// [`GatPath::from_canonical_string`].
    pub fn from_canonical_string(raw: String) -> Result<Self> {
        if raw == "." {
            return Ok(Self::Root);
        }
        GatPath::from_canonical_string(raw).map(GatSubpath::Path)
    }

    /// Borrows the non-root [`GatPath`], or `None` for [`GatSubpath::Root`].
    #[must_use]
    pub const fn as_path(&self) -> Option<&GatPath> {
        match self {
            Self::Root => None,
            Self::Path(path) => Some(path),
        }
    }

    /// Whether `self` is [`GatSubpath::Root`].
    #[must_use]
    pub const fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }

    /// Converts into the existing selection/path-scope representation
    /// ([`crate::path_scope::PathScope`]) by moving the owned
    /// [`GatPath`] for the non-root case -- no allocation, no reparsing.
    #[must_use]
    pub fn into_path_scope(self) -> crate::path_scope::PathScope {
        match self {
            Self::Root => crate::path_scope::PathScope::Root,
            Self::Path(path) => crate::path_scope::PathScope::Path(path),
        }
    }
}

impl std::fmt::Display for GatSubpath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Root => f.write_str("."),
            Self::Path(path) => f.write_str(path.as_str()),
        }
    }
}

/// Serializes [`GatSubpath::Root`] as `"."` and [`GatSubpath::Path`] as its
/// canonical path text required for `MountConfig.path`.
impl serde::Serialize for GatSubpath {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Root => serializer.serialize_str("."),
            Self::Path(path) => serializer.serialize_str(path.as_str()),
        }
    }
}

/// Deserializes via [`GatSubpath::from_canonical_string`] (accepting exactly
/// `"."` for root), retaining the deserialized `String`'s own allocation for
/// the non-root case.
impl<'de> serde::Deserialize<'de> for GatSubpath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::from_canonical_string(raw).map_err(serde::de::Error::custom)
    }
}

/// Non-allocating validation that `raw` is already in `GatPath`'s exact
/// canonical form: non-empty, not rooted, `/`-separated only (no `\`), no
/// `.`/`..` components, and no redundant/leading/trailing separators.
/// TAB/LF/CR are valid component characters. Used by [`GatPath::parse_canonical`] and
/// [`GatPath::from_canonical_string`] so validating already-canonical,
/// machine-owned text (a persisted `gat.lock` row, a SQLite/JSON value)
/// never has to build a second temporary string just to prove the
/// invariant it already satisfies.
pub(crate) fn validate_canonical_str(raw: &str) -> Result<()> {
    if raw.is_empty() {
        return Err(LexicalPathError::EmptyPath {
            input: raw.to_string(),
        });
    }
    if raw.starts_with('/') {
        return Err(LexicalPathError::NotRelative {
            input: raw.to_string(),
        });
    }
    for component in raw.split('/') {
        match component {
            "" => {
                return Err(LexicalPathError::NotRelative {
                    input: raw.to_string(),
                });
            }
            "." => {
                return Err(LexicalPathError::NotRelative {
                    input: raw.to_string(),
                });
            }
            ".." => {
                return Err(LexicalPathError::ParentTraversal {
                    input: raw.to_string(),
                });
            }
            _ => {}
        }
        if component.contains('\\') {
            return Err(LexicalPathError::NotRelative {
                input: raw.to_string(),
            });
        }
    }
    Ok(())
}

impl PartialEq<str> for GatPath {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for GatPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for GatPath {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<&String> for GatPath {
    fn eq(&self, other: &&String) -> bool {
        self.0 == **other
    }
}

impl PartialEq<GatPath> for str {
    fn eq(&self, other: &GatPath) -> bool {
        self == other.0
    }
}

impl PartialEq<GatPath> for &str {
    fn eq(&self, other: &GatPath) -> bool {
        *self == other.0
    }
}

impl PartialEq<GatPath> for String {
    fn eq(&self, other: &GatPath) -> bool {
        *self == other.0
    }
}

impl PartialEq<GatPath> for &String {
    fn eq(&self, other: &GatPath) -> bool {
        **self == other.0
    }
}

impl std::fmt::Display for GatPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Serializes as its canonical path text -- used by configuration types
/// (e.g. `RouteConfig.path`) that store a [`GatPath`] directly.
impl serde::Serialize for GatPath {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

/// Deserializes via [`GatPath::from_canonical_string`] (the strict,
/// machine-owned-text constructor), retaining the deserialized `String`'s
/// own allocation rather than reparsing/renormalizing it -- generic
/// `GatPath` deserialization is for internal/persisted call sites that
/// already produce canonical text (e.g. the mount-transaction journal),
/// not hand-authored `gat.yaml`, which normalizes its lenient path
/// spellings explicitly (see `gat_io::ConfigStore`'s input conversion)
/// before a `GatPath` is ever constructed.
impl<'de> serde::Deserialize<'de> for GatPath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::from_canonical_string(raw).map_err(serde::de::Error::custom)
    }
}

impl std::borrow::Borrow<str> for GatPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Structural result of Gat's lexical relative-path normalization: either a
/// canonical [`GatPath`], or no components at all after collapsing `.` and
/// redundant separators (the repository root itself).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LexicalPath {
    Empty,
    Path(GatPath),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NormalizedGlobPattern {
    pub(crate) pattern: String,
    pub(crate) first_meta: Option<usize>,
}

/// Normalize a user-supplied path into Gat's canonical root-relative form,
/// using Gat's own lexical parser rather than host-dependent `std::path`
/// component classification.
pub(crate) fn normalize_relative_path<P: AsRef<Path>>(path: P) -> Result<LexicalPath> {
    let path = path.as_ref();
    let display = path.to_string_lossy();
    let raw = path.to_str().ok_or_else(|| LexicalPathError::NonUtf8 {
        display: display.to_string(),
    })?;
    normalize_relative_str(raw, &display)
}

/// Normalize a user-supplied glob pattern into Gat's canonical portable
/// pattern language: `/`-separated, relative, and lexical-only. Path
/// identity is host-independent -- a leading segment that merely looks
/// like a Windows drive letter (`C:foo`) is an ordinary pattern segment,
/// not a rejected drive-relative spelling; only genuinely
/// rooted/prefixed and `..`-escaping spellings are rejected.
#[cfg(test)]
pub(crate) fn normalize_glob_pattern(pattern: &str) -> Result<String> {
    Ok(normalize_glob_pattern_with_meta(pattern)?.pattern)
}

fn normalize_relative_str(raw: &str, display: &str) -> Result<LexicalPath> {
    let normalized = normalize_lexical(raw, display, false)?;
    Ok(if normalized.pattern.is_empty() {
        LexicalPath::Empty
    } else {
        LexicalPath::Path(GatPath(normalized.pattern))
    })
}

pub(crate) fn normalize_glob_pattern_with_meta(pattern: &str) -> Result<NormalizedGlobPattern> {
    normalize_lexical(pattern, pattern, true)
}

fn normalize_lexical(
    raw: &str,
    display: &str,
    track_glob_meta: bool,
) -> Result<NormalizedGlobPattern> {
    if raw.starts_with(['/', '\\']) {
        return Err(LexicalPathError::NotRelative {
            input: display.to_string(),
        });
    }

    let mut normalized = String::with_capacity(raw.len());
    let mut first_meta = None;
    let mut component_start = 0usize;
    // Offset of the first glob metacharacter seen so far *within the
    // current raw component*, tracked inline during the single primary
    // scan below rather than by re-scanning each component's text once
    // it's pushed -- `component`'s bytes land in `normalized` unchanged
    // (component boundaries only ever collapse separators or drop `.`
    // components entirely), so an offset found here is still valid
    // relative to that component's eventual start in `normalized`.
    let mut pending_meta: Option<usize> = None;

    for (idx, ch) in raw.char_indices() {
        match ch {
            '/' | '\\' => {
                push_component(
                    &raw[component_start..idx],
                    display,
                    pending_meta,
                    &mut normalized,
                    &mut first_meta,
                )?;
                component_start = idx + ch.len_utf8();
                pending_meta = None;
            }
            '*' | '?' | '['
                if track_glob_meta && first_meta.is_none() && pending_meta.is_none() =>
            {
                pending_meta = Some(idx - component_start);
            }
            _ => {}
        }
    }
    push_component(
        &raw[component_start..],
        display,
        pending_meta,
        &mut normalized,
        &mut first_meta,
    )?;

    Ok(NormalizedGlobPattern {
        pattern: normalized,
        first_meta,
    })
}

fn push_component(
    component: &str,
    display: &str,
    pending_meta: Option<usize>,
    normalized: &mut String,
    first_meta: &mut Option<usize>,
) -> Result<()> {
    if component.is_empty() || component == "." {
        return Ok(());
    }
    if component == ".." {
        return Err(LexicalPathError::ParentTraversal {
            input: display.to_string(),
        });
    }
    if !normalized.is_empty() {
        normalized.push('/');
    }
    let start = normalized.len();
    normalized.push_str(component);
    if first_meta.is_none()
        && let Some(offset) = pending_meta
    {
        *first_meta = Some(start + offset);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_scope::{PathScope, normalize_path_scope};

    fn normalized(input: &str) -> String {
        match normalize_relative_str(input, input).unwrap() {
            LexicalPath::Path(path) => path.as_str().to_string(),
            LexicalPath::Empty => panic!("expected a canonical path"),
        }
    }

    #[test]
    fn canonical_equivalent_spellings_normalize_identically() {
        for input in ["data/a.bin", "./data/a.bin", "data//a.bin", "data\\a.bin"] {
            assert_eq!(normalized(input), "data/a.bin");
        }
    }

    #[test]
    fn path_scope_root_aliases_collapse_to_root() {
        assert_eq!(normalize_path_scope(".").unwrap(), PathScope::Root);
        assert_eq!(normalize_path_scope("./").unwrap(), PathScope::Root);
        assert_eq!(normalize_path_scope("././").unwrap(), PathScope::Root);
    }

    #[test]
    fn repeated_root_forms_are_rejected_instead_of_collapsing_to_root() {
        for input in ["/", "//", "////"] {
            assert!(
                normalize_path_scope(input).is_err(),
                "{input:?} should be rejected"
            );
        }
    }

    #[test]
    fn rooted_and_unc_forms_are_rejected_on_every_host() {
        for input in ["//server/share", "\\\\server\\share", "\\foo", "/foo"] {
            assert!(
                normalize_relative_str(input, input).is_err(),
                "{input:?} should be rejected"
            );
        }
    }

    /// Gat path identity is host-independent; path *materializability* is
    /// host-dependent. A leading segment that merely looks
    /// like a Windows drive letter is not given special lexical meaning:
    /// `C:foo` and canonical `C:/foo` are ordinary, valid Gat paths, and a
    /// `C:\foo` spelling normalizes to `C:/foo` like any other backslash
    /// input.
    #[test]
    fn windows_drive_like_segments_are_ordinary_path_segments() {
        for (input, expected) in [
            ("C:foo", "C:foo"),
            ("C:/foo", "C:/foo"),
            // hygiene-ok: pure string literal exercising backslash normalization; no real Windows path is touched.
            ("C:\\foo", "C:/foo"),
            ("dir/C:foo", "dir/C:foo"),
            ("dir/a:b.txt", "dir/a:b.txt"),
            ("dir\\C:foo", "dir/C:foo"),
        ] {
            assert_eq!(normalized(input), expected);
        }
    }

    #[test]
    fn parent_traversal_is_rejected_with_any_separator_spelling() {
        for input in [
            "../secret",
            "data/../../secret",
            "data\\..\\..\\secret",
            "data/..\\secret",
        ] {
            assert!(
                normalize_relative_str(input, input).is_err(),
                "{input:?} should be rejected"
            );
        }
    }

    #[test]
    fn normalization_is_idempotent_for_canonical_paths() {
        for input in ["a.bin", "data/a.bin", "deep/nested/file.onnx"] {
            assert_eq!(normalized(input), input);
        }
    }

    #[test]
    fn path_scope_paths_are_always_canonical_and_non_empty() {
        for (input, expected) in [
            ("data", "data"),
            ("data/", "data"),
            ("./data/a.bin", "data/a.bin"),
            ("data\\nested\\model.onnx", "data/nested/model.onnx"),
        ] {
            match normalize_path_scope(input).unwrap() {
                PathScope::Path(path) => {
                    let path = path.as_str();
                    assert_eq!(path, expected);
                    assert!(!path.is_empty());
                    assert!(!path.starts_with('/'));
                    assert!(!path.ends_with('/'));
                    assert!(!path.contains('\\'));
                }
                PathScope::Root => panic!("expected a canonical path for {input:?}"),
            }
        }
    }

    #[test]
    fn normalize_glob_pattern_canonicalizes_separators_and_dot_segments() {
        assert_eq!(
            normalize_glob_pattern("./data\\**\\*.bin").unwrap(),
            "data/**/*.bin"
        );
    }

    #[test]
    fn normalize_glob_pattern_rejects_rooted_and_parent_patterns() {
        for input in ["/**/*.bin", "\\**\\*.bin", "../*.bin"] {
            assert!(
                normalize_glob_pattern(input).is_err(),
                "{input:?} should be rejected"
            );
        }
    }

    #[test]
    fn normalize_glob_pattern_accepts_a_windows_drive_like_leading_segment() {
        assert_eq!(normalize_glob_pattern("C:*.bin").unwrap(), "C:*.bin");
    }

    #[test]
    fn normalize_glob_pattern_reports_first_metacharacter_offset() {
        let cases = [
            ("*.bin", Some(0)),
            ("**/*.bin", Some(0)),
            ("data/*.bin", Some("data/".len())),
            ("data/models/model-?.onnx", Some("data/models/model-".len())),
            ("data/models/a.onnx", None),
        ];
        for (pattern, expected) in cases {
            let normalized = normalize_glob_pattern_with_meta(pattern).unwrap();
            assert_eq!(normalized.first_meta, expected, "{pattern:?}");
        }
    }

    // Variant-level assertions.

    #[test]
    fn errors_are_distinguishable_by_variant() {
        assert!(matches!(
            normalize_relative_str("/etc/passwd", "/etc/passwd").unwrap_err(),
            LexicalPathError::NotRelative { .. }
        ));
        assert!(matches!(
            normalize_relative_str("../secret", "../secret").unwrap_err(),
            LexicalPathError::ParentTraversal { .. }
        ));
    }

    // A Failure-mapping test for `LexicalPathError::EmptyPath` lives in
    // `src/error/map/lexical_path.rs` instead of here, since `Failure` is
    // a root-crate-only concept that this pure gat-core module
    // intentionally does not depend on.
}
