//! `gat.lock`'s pure semantic model: the row/TSV codec,
//! canonical-path/OID row validation, ordered and unordered row
//! visitors, and the [`Entry`]/[`Lock`] value types themselves, together
//! with every mutation ([`Lock::upsert`], [`Lock::remove_prefix`], ...)
//! that only ever touches already-parsed in-memory state.
//!
//! Nothing here touches a filesystem, `SQLite`, or any other I/O boundary.
//! Flat/sharded persistence, crash-safe reshape, and stat-proven identity
//! observation are owned by `gat-io`.

use crate::lock::error::{
    InvalidOidReason, LockDomainError, LockError, MalformedRowReason, Result,
};
pub const VERSION: &str = "version https://getgat.dev/spec/lock-v1";

/// Canonical RFC 8785 §3.2.2.2 string representation of a validated path.
/// Writes directly to the formatter without an intermediate allocation.
pub struct QuotedPath<'a>(pub &'a crate::lexical_path::GatPath);

impl std::fmt::Display for QuotedPath<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"")?;
        let path = self.0.as_str();
        let mut start = 0;
        for (index, byte) in path.bytes().enumerate() {
            if byte >= 0x20 && byte != b'"' && byte != b'\\' {
                continue;
            }
            // Escape bytes are ASCII, so these offsets are UTF-8 boundaries.
            f.write_str(&path[start..index])?;
            match byte {
                b'"' => f.write_str("\\\"")?,
                b'\\' => f.write_str("\\\\")?,
                8 => f.write_str("\\b")?,
                9 => f.write_str("\\t")?,
                10 => f.write_str("\\n")?,
                12 => f.write_str("\\f")?,
                13 => f.write_str("\\r")?,
                _ => write!(f, "\\u{byte:04x}")?,
            }
            start = index + 1;
        }
        f.write_str(&path[start..])?;
        f.write_str("\"")
    }
}

fn decode_path(field: &str) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    let inner = field.strip_prefix('"')?.strip_suffix('"')?;
    if inner.bytes().any(|byte| byte < 0x20) {
        return None;
    }
    if !inner.contains('\\') {
        return (!inner.contains('"')).then_some(Cow::Borrowed(inner));
    }
    let mut decoded = String::with_capacity(inner.len());
    for ch in decoded_path_chars(inner) {
        decoded.push(ch?);
    }
    Some(Cow::Owned(decoded))
}

// The order scan consumes decoded characters without allocating path buffers.
// Sharing escape validation with decoding keeps canonical spelling checks identical.
fn decoded_path_chars(inner: &str) -> impl Iterator<Item = Option<char>> {
    let mut chars = inner.chars();
    std::iter::from_fn(move || {
        chars.next().map(|ch| match ch {
            '\\' => decode_path_escape(&mut chars),
            '"' | '\u{00}'..='\u{1f}' => None,
            _ => Some(ch),
        })
    })
}

fn decode_path_escape(chars: &mut std::str::Chars<'_>) -> Option<char> {
    Some(match chars.next()? {
        '"' => '"',
        '\\' => '\\',
        'b' => '\u{08}',
        'f' => '\u{0c}',
        't' => '\t',
        'n' => '\n',
        'r' => '\r',
        'u' => {
            // Only controls without a short escape use Unicode notation.
            if chars.next()? != '0' || chars.next()? != '0' {
                return None;
            }
            let high = match chars.next()? {
                '0' => 0,
                '1' => 16,
                _ => return None,
            };
            let low = match chars.next()? {
                ch @ '0'..='9' => ch as u8 - b'0',
                ch @ 'a'..='f' => ch as u8 - b'a' + 10,
                _ => return None,
            };
            let byte = high + low;
            if matches!(byte, 8 | 9 | 10 | 12 | 13) {
                return None;
            }
            char::from(byte)
        }
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: crate::lexical_path::GatPath, // root-relative, '/'-separated
    pub oid: crate::oid::Oid,
}

/// Build an [`Entry`] from a row's already-validated canonical `path` and
/// already-decoded `oid` (both proven by [`parse_row`]: the path via
/// `is_canonical_rel_path`/[`GatPath::normalize`](crate::lexical_path::GatPath::normalize)
/// equality, the oid via
/// `Oid::from_hex` at parse time). No re-validation, no second hex
/// decode. Reuses an owned decoded string; borrowed text needs one allocation
/// for the owned [`crate::lexical_path::GatPath`].
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
fn is_canonical_rel_path(path: &str) -> bool {
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

/// Parse and validate one already-header-stripped `gat.lock` line: field
/// count, canonical path, and OID format, exactly as `Lock::visit_filtered`
/// enforces per row. Returns `Ok(None)` for a blank line (which
/// physical lock persistence's trailing newline can produce), or
/// `Ok(Some((path, oid)))`
/// with `path` borrowed from `line` when no escapes need decoding and `oid`
/// already decoded into its raw 32 bytes -- so no
/// caller that keeps this row ever re-parses its hex text. Shared by the
/// push-based `Lock::visit_filtered` and the pull-based
/// [`FilteredRowCursor`] so the two row-validation rule sets can never
/// drift apart.
pub fn parse_row(
    line: &str,
    line_num: usize,
) -> Result<Option<(std::borrow::Cow<'_, str>, crate::oid::Oid)>> {
    if line.is_empty() {
        return Ok(None);
    }

    let mut parts = line.splitn(3, '\t');
    let path = parts.next().ok_or(LockDomainError::MalformedRow {
        line: line_num,
        reason: MalformedRowReason::MissingPath,
    })?;
    let oid_field = parts.next().ok_or_else(|| LockDomainError::MalformedRow {
        line: line_num,
        reason: MalformedRowReason::MissingOid {
            path: path.to_string(),
        },
    })?;
    if parts.next().is_some() {
        return Err(LockDomainError::MalformedRow {
            line: line_num,
            reason: MalformedRowReason::TooManyFields {
                path: path.to_string(),
            },
        }
        .into());
    }

    let path = decode_path(path).ok_or_else(|| LockDomainError::NonCanonicalPath {
        line: line_num,
        path: path.to_string(),
    })?;

    // Validate that the path is already in canonical form. The common
    // case -- an already-canonical path, which is every path gat itself
    // ever writes -- is proven directly by `is_canonical_rel_path` without
    // allocating; only a path it can't vouch for falls back to the
    // authoritative (but allocating) `GatPath::normalize`, so a scoped read
    // over a huge, mostly-discarded, well-formed shard doesn't pay to
    // re-normalize every row just to throw the result away. This rejects
    // absolute paths, `..` traversals, non-UTF-8 bytes, backslashes,
    // leading `./`, trailing `/`, and empty segments, same as calling
    // `GatPath::normalize` unconditionally would.
    if !is_canonical_rel_path(&path) {
        let canonical =
            crate::lexical_path::GatPath::normalize(path.as_ref()).map_err(|source| {
                LockError::from(LockDomainError::InvalidRowPath {
                    line: line_num,
                    path: path.to_string(),
                    source,
                })
            })?;
        if canonical != path.as_ref() {
            return Err(LockDomainError::NonCanonicalPath {
                line: line_num,
                path: path.to_string(),
            }
            .into());
        }
    }

    // Validate and decode OID in one pass: must be `blake3:` prefix
    // followed by exactly 64 lower-case hex characters, nothing more.
    // `Oid::from_hex` performs the same length/charset check the old
    // manual byte scan did, but also yields the decoded bytes directly,
    // so a kept row's oid never needs a second hex parse later.
    let oid_hex = oid_field
        .strip_prefix("blake3:")
        .ok_or_else(|| LockDomainError::InvalidOid {
            line: line_num,
            path: path.to_string(),
            reason: InvalidOidReason::MissingPrefix,
        })?;
    let oid = crate::oid::Oid::from_hex(oid_hex).map_err(|_| LockDomainError::InvalidOid {
        line: line_num,
        path: path.to_string(),
        reason: InvalidOidReason::NotHexBlake3,
    })?;

    Ok(Some((path, oid)))
}

/// Whether every entry row in `text` (the header line is skipped) is
/// already in non-decreasing decoded `path` order. The scan borrows ordinary
/// paths and compares escaped paths as decoded characters without allocating; it
/// determines whether a shard can be streamed directly as a
/// pull-based row source, or whether it needs the fully-validating,
/// materialize-and-sort compatibility path instead.
///
/// Physical lock persistence always writes shards in canonical
/// `path` order, so this is `true` for the overwhelming majority of real
/// `gat.lock` files; only a hand-edited or externally-generated file can
/// make it `false`. This checks path encoding and ordering, not full row validity.
/// The fully validating parse still runs afterward on whichever path is chosen.
#[must_use]
pub fn is_path_ordered(text: &str) -> bool {
    let mut lines = text.lines();
    lines.next(); // header: its own format is validated elsewhere.
    let mut last: Option<(&str, bool)> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let field = line.split('\t').next().unwrap_or(line);
        let Some(inner) = field.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
            return false;
        };
        let escaped = inner.contains('\\');
        let valid = if escaped {
            decoded_path_chars(inner).all(|ch| ch.is_some())
        } else {
            !inner.bytes().any(|byte| byte < 0x20 || byte == b'"')
        };
        if !valid {
            return false;
        }
        if let Some((prev, prev_escaped)) = last {
            // UTF-8 byte order agrees with scalar order. Both encodings have
            // been fully validated, including suffixes comparison may not visit.
            let order = if escaped || prev_escaped {
                decoded_path_chars(inner).cmp(decoded_path_chars(prev))
            } else {
                inner.cmp(prev)
            };
            if order.is_lt() {
                return false;
            }
        }
        last = Some((inner, escaped));
    }
    true
}

/// Pull-based, ordered row validator: returns one fully-validated
/// `gat.lock` row at a time as decoded paths and OIDs from the original `text`, without deciding yet whether a caller will keep it.
///
/// This is the core ordered fast path behind both the push-based
/// [`visit_rows_validated`] helper and the selection-aware
/// [`FilteredRowCursor`]: one caller wants to inspect every validated row
/// (e.g. source-side cross-shard validation) while another only wants the
/// selected subset as owned [`Entry`] values. Keeping the ordered-path
/// validation logic here means those two consumers cannot drift.
pub(crate) struct ValidatedRowCursor<'a> {
    text: &'a str,
    pos: usize,
    line_num: usize,
    /// Paths seen so far that could still turn out to be a directory
    /// prefix of some row not yet reached -- the same exclusive upper
    /// bound (`path` + `"0"`, per [`validate_no_path_directory_conflicts`]'s
    /// doc comment: `/` sorts immediately below `0`, so `[path+"/",
    /// path+"0")` is exactly the set of strings that are `path` plus a
    /// `/`-prefixed continuation) is tested via
    /// [`exceeds_directory_upper_bound`] against the borrowed `path`
    /// itself, rather than materializing that bound as an owned `String`
    /// per candidate.
    ///
    /// Because `text` is `path`-ordered, a path can only ever be a
    /// directory prefix of a row that comes after it, and once a later
    /// row's path reaches or passes an earlier candidate's upper bound,
    /// that candidate can never match anything again and is popped.
    /// Unlike the earlier candidate remaining exactly the immediately
    /// preceding row, several candidates can be open at once (e.g. `foo`
    /// and `foo.bin` are both still open while scanning past them towards
    /// `foo/bar`), so this holds a small stack rather than one scalar --
    /// but its size tracks live nesting/overlap, not the total row count,
    /// so a narrow selection over a large flat lock avoids one
    /// permanent allocation per row just to validate a few kept ones.
    ///
    /// This bounds *retained state*, not the per-row check itself: each
    /// row still does a linear scan over whatever is currently open (see
    /// [`Self::next_row`]), so a run of many valid sorted siblings sharing a
    /// long common prefix (e.g. `a`, `a.`, `a..`, `a...`, ...) can keep
    /// several candidates open at once and make that scan cost more than
    /// `O(1)` per row -- still far better than one permanent node for
    /// every row in the shard, but not a strict constant-time guarantee.
    ///
    /// The `usize` tag is unused here (a single resident text has only one
    /// "owner") -- it exists so this can share [`validate_ordered_row`]
    /// with the multi-shard merge walk, which does use it.
    open_ancestors: Vec<(std::borrow::Cow<'a, str>, usize)>,
}

/// Yield at most one matching row per [`Self::next`] call from a validated,
/// ordered shard. Callers can merge two cursors while retaining only one
/// pending [`Entry`] per side, without collecting either shard first.
pub struct FilteredRowCursor<'a, F> {
    rows: ValidatedRowCursor<'a>,
    keep: F,
}

/// Whether `ancestor` is a directory prefix of `path`, i.e. `path` is
/// exactly `ancestor` followed by `/` and at least one more character --
/// the same relationship [`validate_no_path_directory_conflicts`]'s range
/// query and [`FilteredRowCursor`]'s upper bound both encode.
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
/// just to compare against it once. Used by both [`FilteredRowCursor`]
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

/// Ordering/duplicate/directory-prefix bookkeeping shared by every consumer
/// of an already-sorted stream of `gat.lock` rows: the resident-text
/// `ValidatedRowCursor` and the streaming, per-shard k-way merge walk in
/// `gat-io` lock persistence. Both need the same "is this path a duplicate of (or a directory-prefix conflict with)
/// something already open ahead of it" rule, so it lives here once instead
/// of being reimplemented per caller and risking the two falling out of
/// sync.
///
/// `open` holds every path still open (i.e. still a candidate to conflict
/// with a not-yet-seen row) alongside an opaque `owner` tag supplied by the
/// caller for that path -- a single resident text has exactly one owner,
/// while the multi-shard merge tags each path with the shard index that
/// produced it, so a caller can tell a same-source duplicate (a shard
/// listing the same path twice) from a cross-source one (two different
/// shards each independently claiming the same path) without this function
/// needing to know what a "source" is.
///
/// Only checks and retires stale candidates; it does *not* push `path` onto
/// `open` itself, since a caller may still need `path` (e.g. to buffer an
/// [`Entry`] or resolve exact-selection state) between confirming it's
/// clear and recording it as open for later rows.
pub fn check_ordered_row_conflict<P: AsRef<str>>(
    open: &mut Vec<(P, usize)>,
    path: &str,
    owner: usize,
    on_duplicate: impl FnOnce(&str, bool) -> LockError,
    on_prefix_conflict: impl FnOnce(&str, &str) -> LockError,
) -> Result<()> {
    while let Some((ancestor, _)) = open.last() {
        if exceeds_directory_upper_bound(path, ancestor.as_ref()) {
            open.pop();
        } else {
            break;
        }
    }
    if let Some((top, top_owner)) = open.last()
        && top.as_ref() == path
    {
        return Err(on_duplicate(path, *top_owner == owner));
    }
    if let Some((ancestor, _)) = open
        .iter()
        .find(|(ancestor, _)| is_directory_prefix(ancestor.as_ref(), path))
    {
        return Err(on_prefix_conflict(path, ancestor.as_ref()));
    }
    Ok(())
}

impl<'a> ValidatedRowCursor<'a> {
    pub(crate) fn new(text: &'a str) -> Result<Self> {
        if text.is_empty() {
            return Err(LockDomainError::Empty.into());
        }
        let (header, pos) = codec_newline::split_line(text, 0);
        if header != VERSION {
            return Err(LockDomainError::UnsupportedVersion {
                expected: VERSION.to_string(),
                got: header.to_string(),
            }
            .into());
        }
        Ok(Self {
            text,
            pos,
            line_num: 1,
            open_ancestors: Vec::new(),
        })
    }

    /// The next validated row, or `None` once every row in `text` has been
    /// visited.
    pub(crate) fn next_row(
        &mut self,
    ) -> Result<Option<(std::borrow::Cow<'a, str>, crate::oid::Oid)>> {
        loop {
            if self.pos >= self.text.len() {
                return Ok(None);
            }
            self.line_num += 1;
            let (line, next_pos) = codec_newline::split_line(self.text, self.pos);
            self.pos = next_pos;

            let Some((path, oid)) = parse_row(line, self.line_num)? else {
                continue;
            };

            let line_num = self.line_num;
            check_ordered_row_conflict(
                &mut self.open_ancestors,
                &path,
                0,
                |path, _same_owner| {
                    LockDomainError::DuplicatePath {
                        path: path.to_string(),
                        line: Some(line_num),
                    }
                    .into()
                },
                |path, ancestor| {
                    LockDomainError::DirectoryPrefixConflict {
                        ancestor: ancestor.to_string(),
                        descendant: path.to_string(),
                    }
                    .into()
                },
            )?;
            self.open_ancestors.push((path.clone(), 0));

            return Ok(Some((path, oid)));
        }
    }
}

impl<'a, F: FnMut(&str) -> bool> FilteredRowCursor<'a, F> {
    pub fn new(text: &'a str, keep: F) -> Result<Self> {
        Ok(Self {
            rows: ValidatedRowCursor::new(text)?,
            keep,
        })
    }

    /// The next kept row, or `None` once every row in `text` has been
    /// visited.
    #[allow(
        clippy::should_implement_trait,
        reason = "fallible cursor, not std::iter::Iterator: propagates a parse Result per row"
    )]
    pub fn next(&mut self) -> Result<Option<Entry>> {
        while let Some((path, oid)) = self.rows.next_row()? {
            if (self.keep)(&path) {
                return Ok(Some(entry_from_validated_parts(path, oid)));
            }
        }
        Ok(None)
    }
}

fn visit_rows_with_btree_validation(
    text: &str,
    mut select: impl FnMut(&str, crate::oid::Oid) -> Result<bool>,
    mut visit: impl FnMut(Entry) -> Result<()>,
) -> Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    test_probes::record_btree_validation_parse();
    let mut lines = text.lines();
    let header = lines.next().ok_or(LockDomainError::Empty)?;
    if header != VERSION {
        return Err(LockDomainError::UnsupportedVersion {
            expected: VERSION.to_string(),
            got: header.to_string(),
        }
        .into());
    }

    let mut seen_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (line_num, line) in lines.enumerate().map(|(i, l)| (i + 2, l)) {
        let Some((path, oid)) = parse_row(line, line_num)? else {
            continue;
        };

        if !seen_paths.insert(path.to_string()) {
            return Err(LockDomainError::DuplicatePath {
                path: path.to_string(),
                line: Some(line_num),
            }
            .into());
        }

        if select(&path, oid)? {
            visit(entry_from_validated_parts(path, oid))?;
        }
    }

    validate_no_path_directory_conflicts(&seen_paths)?;
    Ok(())
}

/// Visit selected rows using the complete unordered-input validator.
///
/// This is the compatibility path for a caller that already established
/// [`is_path_ordered`] is false. It avoids repeating that order scan before
/// building the `BTreeSet` required to validate arbitrary row order.
pub fn visit_filtered_unordered(
    text: &str,
    mut keep: impl FnMut(&str) -> bool,
    visit: impl FnMut(Entry) -> Result<()>,
) -> Result<()> {
    visit_rows_with_btree_validation(text, |path, _oid| Ok(keep(path)), visit)
}

/// Visit every validated row in `text`, selecting which ones become owned
/// [`Entry`] values only after each row's borrowed `path`/`oid` has already
/// been validated. Ordered input takes the bounded
/// `ValidatedRowCursor` fast path; unordered-but-valid input falls back to
/// the whole-shard `Lock::visit_filtered` validator.
pub fn visit_rows_validated(
    text: &str,
    mut select: impl FnMut(&str, crate::oid::Oid) -> Result<bool>,
    mut visit: impl FnMut(Entry) -> Result<()>,
) -> Result<()> {
    if is_path_ordered(text) {
        let mut rows = ValidatedRowCursor::new(text)?;
        while let Some((path, oid)) = rows.next_row()? {
            if select(&path, oid)? {
                visit(entry_from_validated_parts(path, oid))?;
            }
        }
        Ok(())
    } else {
        visit_rows_with_btree_validation(text, select, visit)
    }
}

/// The shared "filter while parsing, but keep the historical validation
/// contract" helper used by persisted-state readers that only need a
/// subset of one shard/blob. Ordered input takes the bounded
/// [`FilteredRowCursor`] fast path; unordered-but-valid input falls back to
/// `Lock::visit_filtered`'s whole-shard `BTreeSet` validator.
pub fn visit_filtered_matching(
    text: &str,
    mut keep: impl FnMut(&str) -> bool,
    visit: impl FnMut(Entry) -> Result<()>,
) -> Result<()> {
    visit_rows_validated(text, |path, _oid| Ok(keep(path)), visit)
}

impl Lock {
    /// Parse `gat.lock` text. Returns an error if the version line doesn't
    /// match, if any entry row is malformed or non-canonical, if the same
    /// path appears more than once, or if one path is both tracked and a
    /// directory prefix of another tracked path -- refusing to silently
    /// accept a usable subset of an untrusted file.
    ///
    /// Invariants enforced on every entry row:
    /// - Path is non-empty, root-relative, `/`-separated, and already in
    ///   canonical form (no `..`, no leading `./`, no trailing `/`, no `\`).
    /// - OID has the `blake3:` prefix followed by exactly 64 lower-case hex
    ///   characters.
    /// - No two rows share the same path.
    /// - No row's path is a directory prefix of another row's path (a real
    ///   tree cannot have a tracked file *and* tracked descendants under
    ///   it at once, so parsing fails closed rather than silently trusting
    ///   that a reader relying on this -- e.g. `LockSnapshot`'s exact-path
    ///   shortcut -- won't miss a descendant).
    pub fn parse(text: &str) -> Result<Self> {
        let entries = Self::parse_filtered(text, |_| true)?;
        Ok(Self { entries })
    }

    /// [`Self::parse`], but only *retains* rows for which `keep` returns
    /// `true`. Every row is still fully parsed and validated exactly the
    /// same way (canonical path, valid oid, no duplicate or
    /// file/directory-conflicting path across the whole text) -- this
    /// only changes which validated rows are copied into the returned
    /// `Vec`, so a caller that only wants a selection-scoped subset of a
    /// large shard (`LockSnapshot::shard_rows_selected`) does
    /// not pay to build, and immediately discard, a `Vec<Entry>` of every
    /// row the shard holds.
    ///
    /// `keep` is applied to the borrowed, already-validated `path` before
    /// an entry is allocated (escaped paths require a decoding allocation): an owned [`Entry`] is only constructed for
    /// rows `keep` accepts, and path validation itself borrows from
    /// `text` rather than cloning every path up front -- so a narrow
    /// scope over a large, mostly-discarded shard costs `O(N)` bytes read
    /// and validated, not `O(N)` heap allocations retained past this
    /// function.
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

    /// [`Self::parse_filtered`], but instead of collecting kept rows into
    /// a returned `Vec`, calls `visit` for each one as it is parsed
    /// (rows are visited in file order, which is `path` order for any
    /// shard the physical persistence owner ever wrote). A caller
    /// merge-walking kept rows against another already-ordered stream
    /// (e.g. a full flat `status`/`diff` comparison) therefore
    /// never buffers this whole shard's share of the output as a `Vec`
    /// just to iterate it once.
    ///
    /// Validation (canonical path, valid oid, no duplicate or
    /// file/directory-conflicting path across the whole text) is
    /// unchanged from [`Self::parse_filtered`]: every row is still fully
    /// validated, and the file/directory-conflict check still runs over
    /// every path before this returns `Ok`. A row already passed to
    /// `visit` before a later row triggers that final check is not
    /// retracted -- exactly as `parse_filtered` already discards its
    /// whole `kept` vec on such an error, a caller here must treat this
    /// returning `Err` as "nothing it did while visiting is valid",
    /// which every current caller already does by propagating the error.
    pub(crate) fn visit_filtered(
        text: &str,
        keep: impl FnMut(&str) -> bool,
        visit: impl FnMut(Entry) -> Result<()>,
    ) -> Result<()> {
        visit_filtered_unordered(text, keep, visit)
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
        for e in &self.entries {
            writeln!(f, "{}\tblake3:{}", QuotedPath(&e.path), e.oid)?;
        }
        Ok(())
    }
}
/// Minimal, allocation-free line splitting used only by this codec's row
/// parser. Terminator handling delegates to the crate-wide newline policy.
mod codec_newline {
    /// Split `text[start..]` at the next line terminator (`"\n"` or
    /// `"\r\n"`), returning the line's content with the terminator
    /// stripped and the byte offset in `text` immediately after the
    /// terminator -- or `text.len()` if `text[start..]` has no terminator
    /// at all (the final, possibly-unterminated line).
    pub(super) fn split_line(text: &str, start: usize) -> (&str, usize) {
        let rest = &text[start..];
        match rest.find('\n') {
            Some(idx) => (
                crate::newline::strip_terminator(&rest[..=idx]),
                start + idx + 1,
            ),
            None => (rest, text.len()),
        }
    }
}

/// Cross-crate test-observability hook: a `#[cfg(test)]`
/// module can't cross the crate boundary, so storage-layer tests
/// in `gat-io` -- which need to prove
/// a scoped shard read took the bounded ordered fast path
/// (`ValidatedRowCursor`/[`FilteredRowCursor`]) rather than this
/// module's whole-shard `BTreeSet` fallback
/// (`visit_rows_with_btree_validation`) -- observe this counter through
/// the same `test-support` Cargo feature `gat_core::oid`'s own call-count
/// instrumentation already uses. Gated on `cfg(any(test, feature =
/// "test-support"))`, never compiled into a release build.
#[cfg(any(test, feature = "test-support"))]
pub mod test_probes {
    use std::cell::Cell;

    thread_local! {
        static BTREE_VALIDATION_PARSES: Cell<usize> = const { Cell::new(0) };
    }

    pub fn record_btree_validation_parse() {
        BTREE_VALIDATION_PARSES.with(|c| c.set(c.get() + 1));
    }

    /// `visit_rows_with_btree_validation` calls on this thread so far.
    pub fn btree_validation_parses() -> usize {
        BTREE_VALIDATION_PARSES.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_path_writes_unescaped_utf8_in_bulk() {
        use std::fmt::Write;
        #[derive(Default)]
        struct Writes(Vec<String>);
        impl std::fmt::Write for Writes {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                self.0.push(text.to_owned());
                Ok(())
            }
        }
        let path = gp(&"日本語/long-file-name".repeat(100));
        let mut writes = Writes::default();
        write!(writes, "{}", QuotedPath(&path)).unwrap();
        assert_eq!(writes.0.concat(), format!("\"{path}\""));
        assert_eq!(writes.0.len(), 3, "writes must not scale with path length");
    }

    #[test]
    fn validated_entry_reuses_decoded_storage() {
        let decoded = decode_path(r#""a\tb""#).unwrap();
        let ptr = decoded.as_ptr();
        let entry = entry_from_validated_parts(decoded, oid(&"a".repeat(64)));
        assert_eq!(entry.path.as_str(), "a\tb");
        assert_eq!(entry.path.as_str().as_ptr(), ptr);
    }

    #[test]
    fn order_scan_compares_decoded_controls_and_unicode() {
        let paths = [
            "a\u{0}", "a\u{8}", "a\t", "a\u{b}", "a\"", "a/é", "aZ", "aé", "a😀",
        ];
        for first in paths {
            for second in paths {
                let text = format!(
                    "{VERSION}\n{}\n{}\n",
                    QuotedPath(&gp(first)),
                    QuotedPath(&gp(second))
                );
                assert_eq!(is_path_ordered(&text), first <= second);
            }
        }
        // An invalid suffix must be rejected even if the first character
        // already determines the ordering relative to the previous path.
        assert!(!is_path_ordered(&format!(
            "{VERSION}\n\"a\"\n\"z\\u0009\"\n"
        )));
    }

    #[test]
    fn canonical_string_controls_roundtrip() {
        for byte in 0..=31u8 {
            let raw = format!("a{}z", char::from(byte));
            let escaped = match byte {
                8 => "\\b".to_owned(),
                9 => "\\t".to_owned(),
                10 => "\\n".to_owned(),
                12 => "\\f".to_owned(),
                13 => "\\r".to_owned(),
                _ => format!("\\u{byte:04x}"),
            };
            let field = format!("\"a{escaped}z\"");
            assert_eq!(QuotedPath(&gp(&raw)).to_string(), field);
            let decoded = decode_path(&field).unwrap();
            assert!(matches!(decoded, std::borrow::Cow::Owned(_)));
            assert_eq!(decoded, raw);
            assert!(decode_path(&format!("\"{raw}\"")).is_none());
            let text = format!("{VERSION}\n{field}\tblake3:{}\n", "a".repeat(64));
            assert_eq!(Lock::parse(&text).unwrap().entries[0].path.as_str(), raw);
        }
    }

    #[test]
    fn canonical_string_literals_stay_borrowed() {
        let raw = "a/é😀\u{7f}\u{85}\u{2028}\u{2029}e\u{301}";
        let field = format!("\"{raw}\"");
        assert_eq!(QuotedPath(&gp(raw)).to_string(), field);
        let decoded = decode_path(&field).unwrap();
        assert!(matches!(decoded, std::borrow::Cow::Borrowed(_)));
        assert_eq!(decoded.as_ptr(), field[1..].as_ptr());
        assert_eq!(decoded, raw);
        assert_eq!(decode_path(r#""a\"b""#).unwrap(), "a\"b");
        assert_eq!(decode_path(r#""a\\b""#).unwrap(), "a\\b");
    }

    #[test]
    fn canonical_string_rejects_noncanonical_unicode_escapes() {
        for escape in [
            "\\u0008",
            "\\u0009",
            "\\u000a",
            "\\u000c",
            "\\u000d",
            "\\u000B",
            "\\u001F",
            "\\u0020",
            "\\u0022",
            "\\u005c",
            "\\u007f",
            "\\u00e9",
            "\\u2028",
            "\\ud800",
            "\\udc00",
            "\\ud83d\\ude00",
            "\\u",
            "\\u0",
            "\\u00",
            "\\u000",
            "\\u00gg",
        ] {
            assert!(decode_path(&format!("\"a{escape}\"")).is_none(), "{escape}");
        }
    }

    fn gp(path: &str) -> crate::lexical_path::GatPath {
        crate::lexical_path::GatPath::parse_canonical(path.trim_end_matches('/')).unwrap()
    }

    fn oid(hex: &str) -> crate::oid::Oid {
        crate::oid::Oid::from_hex(hex).unwrap()
    }

    /// `is_canonical_rel_path` must agree with `GatPath::normalize` on
    /// whether a path is already canonical for every case
    /// `Lock::visit_filtered` can see, since a `true` result skips the
    /// authoritative check entirely.
    #[test]
    fn is_canonical_rel_path_agrees_with_typed_normalization() {
        let cases = [
            "a.bin",
            "data/b.bin",
            "deep/nested/dir/file.bin",
            "",
            "/a.bin",
            "a.bin/",
            "./a.bin",
            "a.bin/.",
            "a/../b.bin",
            "a//b.bin",
            "a\\b.bin",
            "a:b.bin",
            "dir/a:b.bin",
            "dir/C:foo",
            "a.bin\tx",
            ".",
            "..",
        ];
        for case in cases {
            let fast = is_canonical_rel_path(case);
            if fast {
                // A `true` result must be a hard guarantee: normalization
                // must succeed and reproduce `case` exactly.
                assert_eq!(
                    crate::lexical_path::GatPath::normalize(case).unwrap(),
                    case,
                    "is_canonical_rel_path said {case:?} is canonical, but GatPath::normalize disagrees"
                );
            }
        }
        // And every one of the deliberately-canonical cases above must
        // actually be recognized as such by the fast path, or the
        // optimization silently never fires for the common case.
        assert!(is_canonical_rel_path("a.bin"));
        assert!(is_canonical_rel_path("data/b.bin"));
        assert!(is_canonical_rel_path("deep/nested/dir/file.bin"));
    }

    /// `is_path_ordered` must recognize both a canonically-`path`-ordered
    /// lock (the only shape physical lock persistence ever writes)
    /// and detect a hand-edited, out-of-order one -- the pre-scan a
    /// streaming shard read relies on to decide between the bounded
    /// pull-based path and the materialize-and-sort compatibility path.
    #[test]
    fn is_path_ordered_detects_canonical_and_out_of_order_locks() {
        let ordered = format!(
            "{VERSION}\n\"a.bin\"\tblake3:{}\n\"data/b.bin\"\tblake3:{}\n\"z.bin\"\tblake3:{}\n",
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64)
        );
        assert!(is_path_ordered(&ordered));

        let unordered = format!(
            "{VERSION}\n\"z.bin\"\tblake3:{}\n\"a.bin\"\tblake3:{}\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert!(!is_path_ordered(&unordered));

        // A single row (or none at all) is trivially ordered.
        let single = format!("{VERSION}\n\"a.bin\"\tblake3:{}\n", "a".repeat(64));
        assert!(is_path_ordered(&single));
        assert!(is_path_ordered(VERSION));
    }

    /// A `CRLF`-terminated ordered lock must parse identically to its
    /// `LF` counterpart through the same `visit_rows_validated` ordered
    /// fast path (`ValidatedRowCursor`) that `is_path_ordered` selects for
    /// it -- the two must never disagree just because an optimization
    /// was chosen.
    #[test]
    fn crlf_ordered_lock_parses_the_same_as_its_lf_counterpart() {
        let lf = format!(
            "{VERSION}\n\"a.bin\"\tblake3:{}\n\"data/b.bin\"\tblake3:{}\n\"z.bin\"\tblake3:{}\n",
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64)
        );
        let crlf = lf.replace('\n', "\r\n");
        assert!(
            is_path_ordered(&crlf),
            "CRLF input must still be recognized as ordered"
        );

        let mut cursor = ValidatedRowCursor::new(&crlf).unwrap();
        let mut rows = Vec::new();
        while let Some((path, row_oid)) = cursor.next_row().unwrap() {
            rows.push((gp(&path), row_oid));
        }
        let expected: Vec<_> = Lock::parse(&lf)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| (e.path, e.oid))
            .collect();
        assert_eq!(rows, expected);
    }

    /// A `CRLF` lock whose last row has no final terminator at all must
    /// still parse (the compatibility contract only requires *some* line
    /// terminator between rows, not a trailing one after the last row).
    #[test]
    fn crlf_lock_without_a_final_terminator_on_the_last_row_parses() {
        let text = format!(
            "{VERSION}\r\n\"a.bin\"\tblake3:{}\r\n\"data/b.bin\"\tblake3:{}",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert!(is_path_ordered(&text));
        let mut cursor = ValidatedRowCursor::new(&text).unwrap();
        let mut rows = Vec::new();
        while let Some((path, _oid)) = cursor.next_row().unwrap() {
            rows.push(path.to_string());
        }
        assert_eq!(rows, vec!["a.bin".to_string(), "data/b.bin".to_string()]);
    }

    /// An unordered lock (BTreeSet-validator fallback path) must accept
    /// `CRLF` exactly as its ordered counterpart does -- neither parser
    /// family may have different newline semantics from the other.
    #[test]
    fn crlf_unordered_lock_falls_back_but_still_parses() {
        let lf = format!(
            "{VERSION}\n\"z.bin\"\tblake3:{}\n\"a.bin\"\tblake3:{}\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let crlf = lf.replace('\n', "\r\n");
        assert!(!is_path_ordered(&crlf));
        let lock = Lock::parse(&crlf).unwrap();
        assert_eq!(lock.entries.len(), 2);
    }

    /// A bare trailing `CR` that is *not* part of a `CRLF` terminator
    /// becomes part of the oid field and must be rejected, since `\r` is
    /// not a valid hex character -- a bare `CR` must never be silently
    /// trimmed away by an over-broad `trim`.
    #[test]
    fn bare_trailing_cr_on_the_last_row_is_rejected() {
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{}\r", "a".repeat(64));
        assert!(Lock::parse(&text).is_err());
    }

    /// `FilteredRowCursor` must agree with `Lock::parse_filtered` on which
    /// rows a selection keeps, whether pulled one at a time or collected
    /// in one call -- the pull-based streaming path must never diverge
    /// from the push-based one it is meant to be equivalent to.
    #[test]
    fn filtered_row_cursor_agrees_with_parse_filtered() {
        let text = format!(
            "{VERSION}\n\"a.bin\"\tblake3:{}\n\"data/b.bin\"\tblake3:{}\n\"z.bin\"\tblake3:{}\n",
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64)
        );
        let expected = Lock::parse_filtered(&text, |path| path.starts_with("data/")).unwrap();

        let mut cursor =
            FilteredRowCursor::new(&text, |path: &str| path.starts_with("data/")).unwrap();
        let mut pulled = Vec::new();
        while let Some(entry) = cursor.next().unwrap() {
            pulled.push(entry);
        }
        assert_eq!(pulled, expected);
    }

    #[test]
    fn filtered_row_cursor_rejects_a_directory_prefix_conflict_past_an_interleaved_sibling() {
        // "foo!" (0x21) sorts between "foo" and "foo/bar" (0x2F), so a
        // path-ordered shard can have "foo" retired from the top of the
        // stack by an unrelated sibling before "foo/bar" arrives; the
        // conflict must still be caught by scanning the remaining open
        // ancestors, not just the immediately preceding row.
        let oid = "a".repeat(64);
        let text = format!(
            "{VERSION}\n\"foo\"\tblake3:{oid}\n\"foo!\"\tblake3:{oid}\n\"foo/bar\"\tblake3:{oid}\n"
        );
        assert!(is_path_ordered(&text));
        let mut cursor = FilteredRowCursor::new(&text, |_| true).unwrap();
        let err = loop {
            match cursor.next() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("expected the directory-prefix conflict to be detected"),
                Err(e) => break e,
            }
        };
        assert!(err.to_string().contains("directory prefix"));
    }

    #[test]
    fn filtered_row_cursor_accepts_a_sibling_that_sorts_between_a_path_and_its_would_be_descendant()
    {
        let oid = "a".repeat(64);
        let text = format!("{VERSION}\n\"foo\"\tblake3:{oid}\n\"foo!\"\tblake3:{oid}\n");
        assert!(is_path_ordered(&text));
        let mut cursor = FilteredRowCursor::new(&text, |_| true).unwrap();
        let mut pulled = Vec::new();
        while let Some(entry) = cursor.next().unwrap() {
            pulled.push(entry);
        }
        assert_eq!(pulled.len(), 2);
    }

    #[test]
    fn filtered_row_cursor_rejects_duplicate_adjacent_paths() {
        let oid = "a".repeat(64);
        let text = format!("{VERSION}\n\"dup.bin\"\tblake3:{oid}\n\"dup.bin\"\tblake3:{oid}\n");
        assert!(is_path_ordered(&text));
        let mut cursor = FilteredRowCursor::new(&text, |_| true).unwrap();
        let err = loop {
            match cursor.next() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("expected the duplicate to be detected"),
                Err(e) => break e,
            }
        };
        assert!(err.to_string().contains("more than once"));
    }

    #[test]
    fn display_then_parse_roundtrips() {
        let mut lock = Lock::default();
        lock.upsert(gp("a.bin"), oid(&"a".repeat(64)));
        lock.upsert(gp("data/b.bin"), oid(&"b".repeat(64)));
        assert_eq!(Lock::parse(&lock.to_string()).unwrap(), lock);
    }

    #[test]
    fn display_format_is_stable() {
        let mut lock = Lock::default();
        lock.upsert(gp("a.bin"), oid(&"d".repeat(64)));
        assert_eq!(
            lock.to_string(),
            format!(
                // hygiene-ok: fixed, human-readable spec-URL header compared byte-for-byte; never dereferenced as a network address.
                "version https://getgat.dev/spec/lock-v1\n\"a.bin\"\tblake3:{}\n",
                "d".repeat(64)
            )
        );
    }

    /// Accepting `CRLF` input must never make canonical output
    /// platform-dependent: re-serializing an accepted non-canonical
    /// `CRLF` lock must always produce the same canonical `LF`-only
    /// output as the `LF` original.
    #[test]
    fn crlf_input_reserializes_to_canonical_lf_output() {
        let lf = format!(
            "{VERSION}\n\"a.bin\"\tblake3:{}\n\"data/b.bin\"\tblake3:{}\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let crlf = lf.replace('\n', "\r\n");
        let from_crlf = Lock::parse(&crlf).unwrap();
        let from_lf = Lock::parse(&lf).unwrap();
        assert_eq!(from_crlf, from_lf);
        let rendered = from_crlf.to_string();
        assert_eq!(rendered, lf, "canonical output must always be LF-only");
        assert!(!rendered.contains('\r'));
    }

    #[test]
    fn parse_rejects_wrong_version() {
        assert!(Lock::parse("not a lock\n\"a.bin\"\tblake3:ab\n").is_err());
    }

    #[test]
    fn parse_rejects_empty_string() {
        assert!(Lock::parse("").is_err());
    }

    #[test]
    fn parse_rejects_malformed_lines() {
        let text = format!(
            "{VERSION}\n\"a.bin\"\tblake3:{}\t1\nmalformed line\n",
            "a".repeat(64)
        );
        assert!(Lock::parse(&text).is_err());
    }

    // --- Regression tests for strict parse validation ---

    #[test]
    fn parse_rejects_non_canonical_path_leading_dot_slash() {
        let text = format!("{VERSION}\n\"./data/a.bin\"\tblake3:{}\n", "a".repeat(64));
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_non_canonical_path_parent_traversal() {
        let text = format!("{VERSION}\n\"../secret\"\tblake3:{}\n", "a".repeat(64));
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_absolute_path() {
        let text = format!("{VERSION}\n\"/etc/passwd\"\tblake3:{}\n", "a".repeat(64));
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_path_with_trailing_slash() {
        let text = format!("{VERSION}\n\"data/a.bin/\"\tblake3:{}\n", "a".repeat(64));
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_path_with_backslash() {
        let text = format!("{VERSION}\n\"data\\a.bin\"\tblake3:{}\n", "a".repeat(64));
        assert!(Lock::parse(&text).is_err());
    }

    /// A `CR` embedded in the middle of a path field (not part of a
    /// `CRLF` line terminator) must remain invalid: it's not a valid
    /// canonical path character, so it must be rejected rather than
    /// silently accepted or trimmed away.
    #[test]
    fn parse_rejects_path_with_embedded_cr() {
        let text = format!(
            "{VERSION}\r\n\"data\ra.bin\"\tblake3:{}\r\n",
            "a".repeat(64)
        );
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_oid_without_blake3_prefix() {
        let text = format!("{VERSION}\na.bin\t{}\n", "a".repeat(64));
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_oid_that_is_too_short() {
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{}\n", "a".repeat(32));
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_oid_that_is_too_long() {
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{}\n", "a".repeat(65));
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_oid_with_uppercase_hex() {
        // Blake3 OIDs must be lower-case hex.
        let oid = "A".repeat(64);
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{oid}\n");
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_oid_with_non_hex_characters() {
        let oid = "z".repeat(64);
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{oid}\n");
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_extra_field() {
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{}\textra\n", "a".repeat(64));
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_duplicate_paths() {
        let oid = "a".repeat(64);
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{oid}\n\"a.bin\"\tblake3:{oid}\n");
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_rejects_a_path_that_is_also_a_directory_prefix_of_another() {
        let oid = "a".repeat(64);
        let text = format!("{VERSION}\n\"foo\"\tblake3:{oid}\n\"foo/bar\"\tblake3:{oid}\n");
        let err = Lock::parse(&text).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("foo"),
            "expected the conflict named, got: {msg}"
        );
    }

    #[test]
    fn parse_rejects_a_directory_prefix_conflict_regardless_of_row_order() {
        let oid = "a".repeat(64);
        let text = format!("{VERSION}\n\"foo/bar\"\tblake3:{oid}\n\"foo\"\tblake3:{oid}\n");
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_accepts_sibling_paths_that_merely_share_a_prefix_string() {
        // "foo2" is not a descendant of "foo" (no "/" boundary), so this
        // must not be rejected as a directory-prefix conflict.
        let oid = "a".repeat(64);
        let text = format!("{VERSION}\n\"foo\"\tblake3:{oid}\n\"foo2\"\tblake3:{oid}\n");
        assert!(Lock::parse(&text).is_ok());
    }

    #[test]
    fn parse_accepts_a_sibling_that_sorts_between_a_path_and_its_would_be_descendant() {
        // '!' (0x21) sorts before '/' (0x2F), so "foo!" sits lexically
        // between "foo" and "foo/bar" -- this must not confuse the
        // directory-prefix range check into flagging "foo!" as a
        // descendant of "foo".
        let oid = "a".repeat(64);
        let text = format!("{VERSION}\n\"foo\"\tblake3:{oid}\n\"foo!\"\tblake3:{oid}\n");
        assert!(Lock::parse(&text).is_ok());
    }

    #[test]
    fn parse_filtered_only_retains_rows_keep_accepts_but_validates_every_row() {
        let oid = "a".repeat(64);
        let text = format!("{VERSION}\n\"keep.bin\"\tblake3:{oid}\n\"drop.bin\"\tblake3:{oid}\n");
        let kept = Lock::parse_filtered(&text, |path| path == "keep.bin").unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, "keep.bin");

        // A row rejected by `keep` is still validated: a duplicate that
        // would never be kept is still an error.
        let text = format!("{VERSION}\n\"dup.bin\"\tblake3:{oid}\n\"dup.bin\"\tblake3:{oid}\n");
        assert!(Lock::parse_filtered(&text, |_| false).is_err());
    }

    #[test]
    fn parse_accepts_valid_entry() {
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{}\n", "a".repeat(64));
        let lock = Lock::parse(&text).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].oid, oid(&"a".repeat(64)));
    }

    #[test]
    fn parse_accepts_valid_entry_with_deep_path() {
        let text = format!(
            "{VERSION}\n\"deep/nested/dir/file.bin\"\tblake3:{}\n",
            "a".repeat(64)
        );
        let lock = Lock::parse(&text).unwrap();
        assert_eq!(lock.entries[0].path, "deep/nested/dir/file.bin");
    }

    /// Gat path identity is host-independent: a leading segment
    /// that merely looks like a Windows drive letter (`C:foo`) is an
    /// ordinary lock-v1 path segment, not a rejected drive-relative
    /// spelling. Whether it can actually be materialized on a given host
    /// is a separate, filesystem-boundary concern.
    #[test]
    fn parse_accepts_a_leading_segment_containing_a_colon() {
        let text = format!("{VERSION}\n\"C:foo\"\tblake3:{}\n", "a".repeat(64));
        let lock = Lock::parse(&text).unwrap();
        assert_eq!(lock.entries[0].path, "C:foo");
    }

    #[test]
    fn parse_accepts_a_nested_segment_containing_a_colon() {
        let text = format!("{VERSION}\n\"dir/C:foo\"\tblake3:{}\n", "a".repeat(64));
        let lock = Lock::parse(&text).unwrap();
        assert_eq!(lock.entries[0].path, "dir/C:foo");
    }

    #[test]
    fn parse_rejects_missing_oid_field() {
        let text = format!("{VERSION}\na.bin\n");
        assert!(Lock::parse(&text).is_err());
    }

    #[test]
    fn parse_accepts_two_column_rows() {
        let text = format!("{VERSION}\n\"a.bin\"\tblake3:{}\n", "a".repeat(64));
        assert!(Lock::parse(&text).is_ok());
    }

    #[test]
    fn upsert_replaces_existing_entry_for_same_path() {
        let mut lock = Lock::default();
        lock.upsert(
            gp("a.bin"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab"),
        );
        lock.upsert(
            gp("a.bin"),
            oid("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(
            lock.entries[0].oid,
            oid("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn remove_prefix_normalizes_trailing_slash_and_matches_nested_paths_only() {
        let mut lock = Lock::default();
        lock.upsert_many([
            Entry {
                path: gp("data/a.bin"),
                oid: oid(&"a".repeat(64)),
            },
            Entry {
                path: gp("data/nested/b.bin"),
                oid: oid(&"b".repeat(64)),
            },
            Entry {
                path: gp("data.bin"),
                oid: oid(&"c".repeat(64)),
            },
            Entry {
                path: gp("other.bin"),
                oid: oid(&"d".repeat(64)),
            },
        ]);
        let removed = lock.remove_prefix(&gp("data/"));
        let remaining: Vec<_> = lock.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(remaining, vec![gp("data.bin"), gp("other.bin")]);
        assert_eq!(removed, vec![gp("data/a.bin"), gp("data/nested/b.bin")]);
    }

    #[test]
    fn path_matches_scope_accepts_exact_and_nested_paths_only() {
        assert!(path_matches_scope(&gp("data"), &gp("data")));
        assert!(path_matches_scope(&gp("data/nested/a.bin"), &gp("data")));
        assert!(!path_matches_scope(&gp("data.bin"), &gp("data")));
    }
}
