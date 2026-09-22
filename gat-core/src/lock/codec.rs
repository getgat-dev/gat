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
/// The borrowed path is copied only at this boundary. Kept inlinable for
/// storage readers that materialize each selected row across the crate boundary.
///
/// ```compile_fail
/// use gat_core::lock::validated::entry_from_validated_parts;
/// use gat_core::oid::Oid;
/// entry_from_validated_parts("../outside", Oid::from_bytes([0; 32]));
/// ```
#[must_use]
#[inline]
pub fn entry_from_validated_parts(
    path: crate::lexical_path::GatPathRef<'_>,
    oid: crate::oid::Oid,
) -> Entry {
    Entry {
        path: path.to_owned(),
        oid,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lock {
    pub entries: Vec<Entry>,
}

/// Validate canonical components without allocating on successful input.
pub(super) fn validate_path(path: &str, line: usize) -> Result<()> {
    if crate::lexical_path::validate_canonical_str(path).is_ok() {
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

/// Fails if any path in `paths` is a `/`-delimited directory prefix of
/// another path in the set (e.g. `"foo"` and `"foo/bar"` both present) --
/// a real tree can't have a tracked file *and* tracked descendants under
/// it at once.
///
/// Paths are already ordered by the set. The shared resident-lock validator
/// scans them once, retaining possible ancestors across intervening siblings.
/// If several conflicts exist, any conflicting pair may be reported.
pub fn validate_no_path_directory_conflicts<T>(paths: &std::collections::BTreeSet<T>) -> Result<()>
where
    T: Ord + std::borrow::Borrow<str>,
{
    validate_paths_if_ordered(paths.iter().map(std::borrow::Borrow::borrow))?;
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
            if (self.keep)(path.as_str()) {
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
        if select(path.as_str(), oid)? {
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

// Retain string-prefix candidates, including non-adjacent ancestors such as
// `a`, `a-`, `a/b`. Each candidate is pushed and popped at most once.
fn validate_paths_if_ordered<'a>(paths: impl Iterator<Item = &'a str>) -> Result<bool> {
    let mut previous: Option<&str> = None;
    let mut candidates: Vec<&str> = Vec::new();
    for path in paths {
        if let Some(previous) = previous {
            match previous.cmp(path) {
                std::cmp::Ordering::Equal => {
                    return Err(LockDomainError::DuplicatePath {
                        path: path.to_owned(),
                        line: None,
                    }
                    .into());
                }
                std::cmp::Ordering::Greater => return Ok(false),
                std::cmp::Ordering::Less => {}
            }
            while candidates
                .last()
                .is_some_and(|p| p.len() >= path.len() || !path.starts_with(p))
            {
                candidates.pop();
            }
            if previous.len() < path.len() && path.starts_with(previous) {
                candidates.push(previous);
            }
            if let Some(&ancestor) = candidates.last()
                && path.as_bytes().get(ancestor.len()) == Some(&b'/')
            {
                return Err(LockDomainError::DirectoryPrefixConflict {
                    ancestor: ancestor.to_owned(),
                    descendant: path.to_owned(),
                }
                .into());
            }
        }
        previous = Some(path);
    }
    Ok(true)
}

impl Lock {
    /// Validate uniqueness and file/directory consistency of resident entries.
    /// Sorted inputs borrow entries directly; unordered inputs sort references,
    /// without cloning paths. Validation is linear in path bytes after ordering.
    pub fn validate(&self) -> Result<()> {
        if !validate_paths_if_ordered(self.entries.iter().map(|entry| entry.path.as_str()))? {
            let mut entries: Vec<_> = self.entries.iter().collect();
            entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
            validate_paths_if_ordered(entries.into_iter().map(|entry| entry.path.as_str()))?;
        }
        Ok(())
    }

    /// Parse and validate a complete lock-v1 file before materializing entries.
    ///
    /// Rows contain 64 lowercase hex digest bytes, TAB, and an unquoted path
    /// with control-only `\xhh` escaping. LF and CRLF may be mixed; every line
    /// requires a terminator. Decoded paths must be canonical and strictly
    /// increasing, with no duplicates or file/directory-prefix conflicts.
    pub fn parse(text: &str) -> Result<Self> {
        let view = super::reader::ValidatedLockFile::parse(text)?;
        Ok(Self {
            entries: view
                .rows()
                .map(|(path, oid)| entry_from_validated_parts(path, oid))
                .collect(),
        })
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
    /// Returns the paths of the rows
    /// that were removed, so callers can report the real number of tracked
    /// files affected (a directory may hold any number of tracked files)
    /// and know exactly which files to delete on disk.
    pub fn remove_prefix(
        &mut self,
        path: &crate::lexical_path::GatPath,
    ) -> Vec<crate::lexical_path::GatPath> {
        self.entries
            .extract_if(.., |entry| entry.path.is_or_under(path))
            .map(|entry| entry.path)
            .collect()
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
