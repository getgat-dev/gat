//! `gat.lock`'s pure semantic model: the digest-first lock-v1 codec,
//! canonical-path/OID row validation, strictly ordered row
//! visitors, and the [`Entry`]/[`Lock`] value types themselves, together
//! with every mutation ([`Lock::upsert`], [`Lock::remove_prefix`], ...)
//! that only ever touches already-parsed in-memory state.
//!
//! Nothing here touches a filesystem, `SQLite`, or any other I/O boundary.
//! Flat/sharded persistence, crash-safe reshape, and stat-proven identity
//! observation are owned by `gat-io`.

use crate::lock::error::{LockDomainError, Result};
pub const VERSION: &str = "version https://getgat.dev/spec/lock-v1";

/// Canonical control-only escaping for an unquoted lock path.
pub struct EscapedPath<'a>(pub &'a crate::lexical_path::GatPath);

impl std::fmt::Display for EscapedPath<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.0.as_str();
        let mut start = 0;
        for (i, byte) in path.bytes().enumerate() {
            if byte < 32 {
                f.write_str(&path[start..i])?;
                write!(f, "\\x{byte:02x}")?;
                start = i + 1;
            }
        }
        f.write_str(&path[start..])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: crate::lexical_path::GatPath, // root-relative, '/'-separated
    pub oid: crate::oid::Oid,
}

/// Materialize an owned entry from a certified canonical path and decoded OID.
/// An owned path is moved; a borrowed path is copied only at this boundary.
#[must_use]
pub fn entry_from_validated_parts<'a>(
    path: impl Into<std::borrow::Cow<'a, str>>,
    oid: crate::oid::Oid,
) -> Entry {
    Entry {
        path: crate::lexical_path::GatPath::from_validated_canonical(path.into().into_owned()),
        oid,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lock {
    pub entries: Vec<Entry>,
}

/// Cheap, allocation-free check that `path` is already in the exact
/// canonical form [`GatPath::normalize`](crate::lexical_path::GatPath::normalize)
/// would produce for it. Used by
/// `Lock::visit_filtered`'s per-row validation, where the overwhelming
/// majority of rows in a real `gat.lock` are already canonical (gat
/// itself only ever writes canonical paths): `GatPath::normalize` builds an
/// owned `Vec<String>` of path components plus a joined `String` just to
/// compare the result back against the original, so calling it for
/// every parsed row -- including rows a scoped read discards -- costs
/// `O(row count)` heap churn even when nothing is actually malformed.
///
/// A `false` result does **not** mean `path` is invalid, only that this
/// cheap check couldn't prove it canonical either way; the caller must
/// fall back to `GatPath::normalize` to find out for sure. A `true` result is
/// a hard guarantee: every condition `GatPath::normalize` would otherwise
/// need to fix up (a `.`/`..`/empty path segment, a leading/trailing/
/// doubled `/`, a `\`) is absent here, so the component walk it performs
/// is a no-op and its result is `path` itself.
pub(super) fn is_canonical_rel_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') {
        return false;
    }

    let mut segment_len = 0usize;
    let mut segment_all_dots = true;

    for b in path.bytes() {
        match b {
            b'/' => {
                if segment_len == 0 || (segment_all_dots && (segment_len == 1 || segment_len == 2))
                {
                    return false;
                }
                segment_len = 0;
                segment_all_dots = true;
            }
            b'\\' => return false,
            b'.' => {
                segment_len += 1;
            }
            _ => {
                segment_len += 1;
                segment_all_dots = false;
            }
        }
    }

    !(segment_len == 0 || (segment_all_dots && (segment_len == 1 || segment_len == 2)))
}

/// Validate canonical components without allocating on successful input.
pub(super) fn validate_path(path: &str, line: usize) -> Result<()> {
    if is_canonical_rel_path(path) {
        return Ok(());
    }
    crate::lexical_path::GatPath::normalize(path).map_err(|source| {
        LockDomainError::InvalidRowPath {
            line,
            path: path.to_owned(),
            source,
        }
    })?;
    Err(LockDomainError::NonCanonicalPath {
        line,
        path: path.to_owned(),
    }
    .into())
}

/// Whether a canonical tracked `path` falls under the canonical `scope`:
/// either the exact tracked file, or anything nested below that directory
/// prefix. Sibling prefixes don't match (`data` must not match
/// `data.bin`).
#[must_use]
pub fn path_matches_scope(
    path: &crate::lexical_path::GatPath,
    scope: &crate::lexical_path::GatPath,
) -> bool {
    path.is_or_under(scope)
}

/// Fails if any path in `paths` is a `/`-delimited directory prefix of
/// another path in the set (e.g. `"foo"` and `"foo/bar"` both present) --
/// a real tree can't have a tracked file *and* tracked descendants under
/// it at once.
///
/// For each `path`, its descendants are exactly the strings in the
/// exclusive range `["{path}/", "{path}0")`: `'/'` (0x2F) and `'0'`
/// (0x30) are adjacent ASCII code points, so this range captures every
/// string with `path` as a `/`-delimited prefix and nothing else, no
/// matter what characters sibling paths contain. `BTreeSet::range` makes
/// each check `O(log n)`, so validating the whole set is `O(n log n)`,
/// not quadratic.
pub fn validate_no_path_directory_conflicts<T>(paths: &std::collections::BTreeSet<T>) -> Result<()>
where
    T: Ord + std::borrow::Borrow<str> + std::fmt::Debug,
{
    for path in paths {
        let path: &str = path.borrow();
        let lower = format!("{path}/");
        let upper = format!("{path}0");
        let bounds = (
            std::ops::Bound::Included(lower.as_str()),
            std::ops::Bound::Excluded(upper.as_str()),
        );
        if let Some(descendant) = paths.range::<str, _>(bounds).next() {
            let descendant: &str = descendant.borrow();
            return Err(LockDomainError::DirectoryPrefixConflict {
                ancestor: path.to_string(),
                descendant: descendant.to_string(),
            }
            .into());
        }
    }
    Ok(())
}

/// Pull selected owned rows from a completely certified file.
pub struct FilteredRowCursor<'a, F> {
    view: super::reader::ValidatedLockFile<'a>,
    next: usize,
    keep: F,
}

/// Whether `ancestor` is a directory prefix of `path`, i.e. `path` is
/// exactly `ancestor` followed by `/` and at least one more character --
/// the same relationship [`validate_no_path_directory_conflicts`]'s range
/// query and the cross-file ordered merge's upper bound both encode.
#[must_use]
pub fn is_directory_prefix(ancestor: &str, path: &str) -> bool {
    path.len() > ancestor.len()
        && path.as_bytes()[ancestor.len()] == b'/'
        && path.starts_with(ancestor)
}

/// Whether `path >= format!("{ancestor}0")` -- i.e. `path` has reached or
/// passed `ancestor`'s exclusive directory-descendant upper bound (see
/// [`validate_no_path_directory_conflicts`]) -- computed by comparing
/// bytes directly instead of allocating that bound as an owned `String`
/// just to compare against it once. Used by the cross-file ordered merge
/// and `gat_io::StateStore`'s bulk
/// directory-conflict merge-walk to retire an open candidate without a
/// per-candidate allocation.
#[must_use]
pub fn exceeds_directory_upper_bound(path: &str, ancestor: &str) -> bool {
    let a = ancestor.as_bytes();
    let p = path.as_bytes();
    let common = a.len().min(p.len());
    for i in 0..common {
        match p[i].cmp(&a[i]) {
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Equal => {}
        }
    }
    match p.len().cmp(&a.len()) {
        // `path` is a strict prefix of (i.e. shorter than) `ancestor`, or
        // exactly equal to it: either way it's below `ancestor + "0"`.
        std::cmp::Ordering::Less | std::cmp::Ordering::Equal => false,
        // `path` extends past `ancestor`; whether it's still below the
        // bound depends only on the very next byte, since `ancestor`'s
        // own bytes already matched in full above.
        std::cmp::Ordering::Greater => p[a.len()] >= b'0',
    }
}

impl<'a, F: FnMut(&str) -> bool> FilteredRowCursor<'a, F> {
    /// Validate the complete source before invoking selection or returning rows.
    pub fn new(text: &'a str, keep: F) -> Result<Self> {
        Ok(Self {
            view: super::reader::ValidatedLockFile::parse(text)?,
            next: 0,
            keep,
        })
    }
}

impl<F: FnMut(&str) -> bool> Iterator for FilteredRowCursor<'_, F> {
    type Item = Entry;

    /// Return the next selected row, allocating only its retained path.
    fn next(&mut self) -> Option<Entry> {
        while let Some((path, oid)) = self.view.row(self.next) {
            self.next += 1;
            if (self.keep)(path) {
                return Some(entry_from_validated_parts(path, oid));
            }
        }
        None
    }
}

/// Certify every row and whole-file invariant before invoking either callback.
pub fn visit_rows_validated(
    text: &str,
    mut select: impl FnMut(&str, crate::oid::Oid) -> Result<bool>,
    mut visit: impl FnMut(Entry) -> Result<()>,
) -> Result<()> {
    let view = super::reader::ValidatedLockFile::parse(text)?;
    for (path, oid) in view.rows() {
        if select(path, oid)? {
            visit(entry_from_validated_parts(path, oid))?;
        }
    }
    Ok(())
}

/// Validate the complete file, then visit matching rows in path order.
pub fn visit_filtered_matching(
    text: &str,
    mut keep: impl FnMut(&str) -> bool,
    visit: impl FnMut(Entry) -> Result<()>,
) -> Result<()> {
    visit_rows_validated(text, |path, _| Ok(keep(path)), visit)
}

impl Lock {
    /// Parse and validate a complete lock-v1 file before materializing entries.
    ///
    /// Rows contain 64 lowercase hex digest bytes, TAB, and an unquoted path
    /// with control-only `\xhh` escaping. LF and CRLF may be mixed; every line
    /// requires a terminator. Decoded paths must be canonical and strictly
    /// increasing, with no duplicates or file/directory-prefix conflicts.
    pub fn parse(text: &str) -> Result<Self> {
        let entries = Self::parse_filtered(text, |_| true)?;
        Ok(Self { entries })
    }

    /// Certify the whole file, then materialize only selected rows.
    pub(crate) fn parse_filtered(
        text: &str,
        mut keep: impl FnMut(&str) -> bool,
    ) -> Result<Vec<Entry>> {
        let mut kept = Vec::new();
        Self::visit_filtered(text, &mut keep, |entry| {
            kept.push(entry);
            Ok(())
        })?;
        Ok(kept)
    }

    /// Certify the whole file before invoking selection or emission callbacks.
    pub(crate) fn visit_filtered(
        text: &str,
        keep: impl FnMut(&str) -> bool,
        visit: impl FnMut(Entry) -> Result<()>,
    ) -> Result<()> {
        visit_filtered_matching(text, keep, visit)
    }

    /// Insert or update the row for `path`, given as already-typed
    /// [`crate::lexical_path::GatPath`]/[`crate::oid::Oid`]
    /// values. This is the panic-free typed mutation primitive production
    /// code should use; it never needs to parse or validate its inputs
    /// because a `GatPath`/`Oid` value is already known-canonical/valid.
    pub fn upsert(&mut self, path: crate::lexical_path::GatPath, oid: crate::oid::Oid) {
        match self.entries.iter_mut().find(|e| e.path == path) {
            Some(e) => {
                e.oid = oid;
            }
            None => self.entries.push(Entry { path, oid }),
        }
    }

    /// Insert or update rows for many `(path, oid)` pairs at once.
    /// Same result as calling [`Self::upsert`] in a loop, but O(1) amortized
    /// per entry instead of O(n) (`upsert` re-scans the whole vec every
    /// call, making a loop of it O(n²) — the entire vec is only worth
    /// re-scanning once, not once per new entry).
    pub fn upsert_many(&mut self, new_entries: impl IntoIterator<Item = Entry>) {
        let mut index: std::collections::HashMap<crate::lexical_path::GatPath, usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.path.clone(), i))
            .collect();
        for e in new_entries {
            if let Some(&i) = index.get(&e.path) {
                self.entries[i] = e;
            } else {
                index.insert(e.path.clone(), self.entries.len());
                self.entries.push(e);
            }
        }
    }

    /// Remove the row for `path` and any row nested under it (`path/...`),
    /// covering both single-file and whole-directory untrack in one call.
    /// A trailing slash on `path` (e.g. `data/`) is ignored so it matches
    /// the same rows as the slash-less form. Returns the paths of the rows
    /// that were removed, so callers can report the real number of tracked
    /// files affected (a directory may hold any number of tracked files)
    /// and know exactly which files to delete on disk.
    pub fn remove_prefix(
        &mut self,
        path: &crate::lexical_path::GatPath,
    ) -> Vec<crate::lexical_path::GatPath> {
        let mut removed = Vec::new();
        self.entries.retain(|e| {
            let matches = path_matches_scope(&e.path, path);
            if matches {
                removed.push(e.path.clone());
            }
            !matches
        });
        removed
    }
}

impl std::fmt::Display for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{VERSION}")?;
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        for entry in entries {
            writeln!(f, "{}\t{}", entry.oid, EscapedPath(&entry.path))?;
        }
        Ok(())
    }
}
