//! The one desired-state query layer: how a command *describes* the rows
//! it needs ([`DesiredQuery`]), and how this store turns that description
//! into the cheapest correct `SQLite` access path.
//!
//! Commands never write SQL and never decide between "scan everything and
//! filter in Rust" and "ask an index": they state their intent as one of
//! exact paths, lexical scopes, unions of ranges, or all rows, optionally narrowed to one
//! logical shard, optionally paired with a residual [`Selection`] whose
//! `matches()` remains the semantic authority. This module owns the
//! `IN (...)` chunking, the lexical range bounds, the ordering guarantee,
//! and whether a caller streams rows through a cursor or collects them
//! into a `Vec` containing all matches. Path-only exclusion scans reuse these range
//! predicates to enumerate the complement of [`DesiredPathExclusions`].
//!
//! Two rules keep that split honest:
//!
//! - **Narrow first, filter second.** [`Selection::scope_path`] is only a
//!   storage-narrowing hint; storage may return a superset of what the
//!   selection ultimately matches, never a subset. Every row a cursor
//!   yields has already passed the residual `Selection::matches` check, so
//!   a caller cannot forget to apply it.
//! - **One implementation, two consumption modes.**
//!   [`StateStore::with_desired_rows`] is the cursor-oriented core;
//!   [`StateStore::desired_rows`] and
//!   [`StateStore::desired_any`] are thin wrappers over it for
//!   callers that need all matching rows or a mere existence answer.

use super::{
    Connection, DesiredRow, DesiredStateWrite, Entry, Result, StateResultExt, StateStore,
    StateStoreError, decode_desired_row_raw, decode_shard_id, descendant_range, params_from_iter,
    sql_chunk_size, sql_placeholders,
};
use gat_core::globs::GlobBound;
use gat_core::selection::Selection;

/// Which candidate rows the storage layer should retrieve, before any
/// residual [`Selection`] filtering. This is a *storage* concept: it says
/// how to find candidates cheaply, not what the user asked for.
#[derive(Clone, Debug)]
pub(crate) enum DesiredSpan<'a> {
    /// Every desired row, in `path` order.
    All,
    /// No candidate row can match.
    Empty,
    /// Exactly `scope` plus everything nested under it (`scope/...`),
    /// resolved as an exact point plus a descendant range (see [`descendant_range`]).
    Scope(&'a gat_core::lexical_path::GatPath),
    /// Exactly the named paths (chunked `IN (...)` lookups, never a scan).
    Exact(&'a [gat_core::lexical_path::GatPath]),
    /// One coalesced union of disjoint lexical ranges and exact points.
    Union(UnionPlan),
}

#[derive(Clone, Debug)]
pub enum CandidateBound<'a> {
    Any,
    Scope(std::borrow::Cow<'a, str>),
    Prefix(std::borrow::Cow<'a, str>),
    Exact(std::borrow::Cow<'a, str>),
}

impl<'a> CandidateBound<'a> {
    pub const fn scope(scope: &'a str) -> Self {
        Self::Scope(std::borrow::Cow::Borrowed(scope))
    }

    pub const fn prefix(prefix: &'a str) -> Self {
        Self::Prefix(std::borrow::Cow::Borrowed(prefix))
    }

    pub const fn exact(path: &'a str) -> Self {
        Self::Exact(std::borrow::Cow::Borrowed(path))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UnionPlan {
    ranges: Vec<LexicalRange>,
    exact: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LexicalRange {
    lower: String,
    upper: Option<String>,
}

/// Canonical descendant ranges to omit from desired-path enumeration.
/// The exact directory name remains eligible as a file path.
#[derive(Default)]
pub struct DesiredPathExclusions {
    ranges: Vec<LexicalRange>,
}

impl DesiredPathExclusions {
    #[must_use]
    pub fn new(directories: impl IntoIterator<Item = gat_core::lexical_path::GatPath>) -> Self {
        let mut ranges = directories
            .into_iter()
            .map(|path| {
                let (lower, upper) = descendant_range(path.as_str());
                LexicalRange {
                    lower,
                    upper: Some(upper),
                }
            })
            .collect();
        coalesce_ranges(&mut ranges);
        Self { ranges }
    }

    #[must_use]
    pub fn contains(&self, path: &gat_core::lexical_path::GatPath) -> bool {
        let index = self
            .ranges
            .partition_point(|range| range.lower.as_str() <= path.as_str());
        index > 0
            && self.ranges[index - 1]
                .upper
                .as_deref()
                .is_none_or(|upper| path.as_str() < upper)
    }
}

/// A complete desired-state read request: a candidate `DesiredSpan`, an
/// optional logical-shard restriction, and an optional residual
/// [`Selection`] that is applied to every candidate row before it reaches
/// the caller.
#[derive(Clone, Debug)]
pub struct DesiredQuery<'a> {
    span: DesiredSpan<'a>,
    shard_id: Option<&'a str>,
    residual: Option<&'a Selection>,
}

impl<'a> DesiredQuery<'a> {
    /// Every desired row, in `path` order.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            span: DesiredSpan::All,
            shard_id: None,
            residual: None,
        }
    }

    /// Exactly `scope` plus everything nested under it.
    #[must_use]
    pub const fn scope(scope: &'a gat_core::lexical_path::GatPath) -> Self {
        Self {
            span: DesiredSpan::Scope(scope),
            shard_id: None,
            residual: None,
        }
    }

    /// [`Self::scope`] for `Some`, [`Self::all`] for `None` -- the shape
    /// [`Selection::scope_path`] already returns.
    #[must_use]
    pub const fn in_scope(scope: Option<&'a gat_core::lexical_path::GatPath>) -> Self {
        match scope {
            Some(scope) => Self::scope(scope),
            None => Self::all(),
        }
    }

    /// Exactly `paths`, resolved by chunked primary-key lookups. Never
    /// broadened into a scan, however many paths are named.
    #[must_use]
    pub const fn exact(paths: &'a [gat_core::lexical_path::GatPath]) -> Self {
        Self {
            span: DesiredSpan::Exact(paths),
            shard_id: None,
            residual: None,
        }
    }

    pub fn from_candidate_bounds(bounds: impl IntoIterator<Item = CandidateBound<'a>>) -> Self {
        Self {
            span: span_from_bounds(bounds),
            shard_id: None,
            residual: None,
        }
    }

    /// The query a selection-aware command should run: narrow storage by
    /// the selection's scope, then let `Selection::matches` remain the
    /// final authority over every candidate row. An unrestricted
    /// selection skips per-row matching entirely.
    #[must_use]
    pub fn for_selection(selection: &'a Selection) -> Self {
        let query = query_for_selection(selection);
        if selection.is_unrestricted() {
            query
        } else {
            query.with_residual(selection)
        }
    }

    /// Apply `selection` as the semantic authority over every candidate
    /// row this query retrieves.
    #[must_use]
    pub const fn with_residual(mut self, selection: &'a Selection) -> Self {
        self.residual = Some(selection);
        self
    }
}

fn query_for_selection(selection: &Selection) -> DesiredQuery<'_> {
    let scope = selection.scope_path();
    let includes = selection.include_globs();
    if includes.is_empty() {
        return DesiredQuery::in_scope(scope);
    }
    let mut bounds = Vec::with_capacity(includes.len().max(1));
    for include in includes {
        match include.bound() {
            GlobBound::Any => {
                return DesiredQuery::in_scope(scope);
            }
            GlobBound::Exact(path) => {
                if let Some(scope) = scope {
                    bounds.push(CandidateBound::Exact(std::borrow::Cow::Owned(join_scope(
                        scope.as_str(),
                        path,
                    ))));
                } else {
                    bounds.push(CandidateBound::exact(path));
                }
            }
            GlobBound::Prefix(prefix) => {
                if let Some(scope) = scope {
                    bounds.push(CandidateBound::Prefix(std::borrow::Cow::Owned(join_scope(
                        scope.as_str(),
                        prefix,
                    ))));
                } else {
                    bounds.push(CandidateBound::prefix(prefix));
                }
            }
        }
    }
    if let Some(scope) = scope {
        if bounds.is_empty() {
            DesiredQuery::scope(scope)
        } else {
            DesiredQuery::from_candidate_bounds(bounds)
        }
    } else {
        DesiredQuery::from_candidate_bounds(bounds)
    }
}

fn join_scope(scope: &str, rel: &str) -> String {
    if rel.is_empty() {
        scope.to_string()
    } else {
        format!("{scope}/{rel}")
    }
}

fn span_from_bounds<'a>(bounds: impl IntoIterator<Item = CandidateBound<'a>>) -> DesiredSpan<'a> {
    let mut ranges = Vec::new();
    let mut exact = Vec::new();
    for bound in bounds {
        match bound {
            CandidateBound::Any => return DesiredSpan::All,
            CandidateBound::Scope(scope) => {
                let scope = scope.as_ref();
                let scope = scope.strip_suffix('/').unwrap_or(scope);
                exact.push(scope.to_string());
                ranges.push(LexicalRange::for_prefix(&format!("{scope}/")));
            }
            CandidateBound::Prefix(prefix) => {
                if prefix.is_empty() {
                    return DesiredSpan::All;
                }
                ranges.push(LexicalRange::for_prefix(prefix.as_ref()));
            }
            CandidateBound::Exact(path) => exact.push(path.to_string()),
        }
    }

    if ranges.is_empty() && exact.is_empty() {
        return DesiredSpan::Empty;
    }
    coalesce_ranges(&mut ranges);
    exact.sort_unstable();
    exact.dedup();
    retain_uncovered_exacts(&mut exact, &ranges);
    DesiredSpan::Union(UnionPlan { ranges, exact })
}

impl LexicalRange {
    fn for_prefix(prefix: &str) -> Self {
        Self {
            lower: prefix.to_string(),
            upper: lexical_prefix_upper(prefix),
        }
    }
}

/// Exclusive upper bound for a UTF-8 prefix, or none when it is unbounded.
/// Only trailing maximum scalars require examining an earlier character.
fn lexical_prefix_upper(prefix: &str) -> Option<String> {
    for (index, last) in prefix.char_indices().rev() {
        if let Some(next) = next_scalar(last) {
            let mut upper = String::with_capacity(index + next.len_utf8());
            upper.push_str(&prefix[..index]);
            upper.push(next);
            return Some(upper);
        }
    }
    None
}

const fn next_scalar(ch: char) -> Option<char> {
    // Unicode's surrogate range contains no scalar values.
    if ch == '\u{d7ff}' {
        Some('\u{e000}')
    } else {
        char::from_u32(ch as u32 + 1)
    }
}

fn coalesce_ranges(ranges: &mut Vec<LexicalRange>) {
    ranges.sort_by(|a, b| {
        a.lower
            .cmp(&b.lower)
            .then_with(|| cmp_upper(a.upper.as_deref(), b.upper.as_deref()))
    });
    ranges.dedup_by(|range, previous| {
        if !ranges_overlap_or_touch(previous, range) {
            return false;
        }
        if cmp_upper(previous.upper.as_deref(), range.upper.as_deref()).is_lt() {
            previous.upper = range.upper.take();
        }
        true
    });
}

fn ranges_overlap_or_touch(a: &LexicalRange, b: &LexicalRange) -> bool {
    match &a.upper {
        None => true,
        Some(upper) => b.lower.as_str() <= upper.as_str(),
    }
}

fn cmp_upper(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    }
}

/// Drop every `exact` path already covered by a (sorted, coalesced)
/// `ranges` set, in place. Both sequences are already sorted ascending
/// by `path`, so a single forward two-pointer sweep suffices: `range_idx`
/// only ever advances, giving `O(ranges.len()` + `exact.len()`) overall
/// rather than re-scanning every range for every exact path.
fn retain_uncovered_exacts(exact: &mut Vec<String>, ranges: &[LexicalRange]) {
    let mut range_idx = 0;
    exact.retain(|path| {
        while range_idx < ranges.len() {
            let past = match &ranges[range_idx].upper {
                Some(upper) => path.as_str() >= upper.as_str(),
                None => false,
            };
            if !past {
                break;
            }
            range_idx += 1;
        }
        match ranges.get(range_idx) {
            Some(range) => {
                let covered = path.as_str() >= range.lower.as_str()
                    && match &range.upper {
                        None => true,
                        Some(upper) => path.as_str() < upper.as_str(),
                    };
                !covered
            }
            None => true,
        }
    });
}

/// The `WHERE`/`ORDER BY` tail plus bound parameters shared by every
/// non-`Exact` span, so the scope range and the shard restriction are
/// composed in exactly one place.
fn range_sql(
    scope: Option<&gat_core::lexical_path::GatPath>,
    shard_id: Option<&str>,
) -> (String, Vec<rusqlite::types::Value>) {
    let mut sql = String::from("SELECT path, desired_oid FROM state WHERE desired_oid IS NOT NULL");
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(shard_id) = shard_id {
        sql.push_str(" AND desired_shard_id = ?");
        params.push(shard_id.to_string().into());
    }
    if let Some(scope) = scope {
        let (lower, upper) = descendant_range(scope.as_str());
        sql.push_str(" AND (path = ? OR (path >= ? AND path < ?))");
        params.push(scope.as_str().to_string().into());
        params.push(lower.into());
        params.push(upper.into());
    }
    sql.push_str(" ORDER BY path");
    (sql, params)
}

/// One bounded, keyset-paginated fetch of a [`LexicalRange`]: `path`
/// strictly after `frontier` (or `>= lower` for the first fetch), below
/// `upper` if bounded, at most `limit` rows. A union plan opens one such
/// chunk at a time per range rather than folding every range into one
/// `OR`-of-ranges clause, so a candidate set with many disjoint ranges
/// never grows a single statement's parameter/clause count unbounded.
fn range_chunk_sql(
    lower: &str,
    upper: Option<&str>,
    frontier: Option<&str>,
    shard_id: Option<&str>,
    limit: usize,
) -> (String, Vec<rusqlite::types::Value>) {
    lexical_range_sql(
        DesiredProjection::PathAndOid,
        lower,
        upper,
        frontier,
        shard_id,
        Some(limit),
    )
}

#[derive(Clone, Copy)]
enum DesiredProjection {
    Path,
    PathAndOid,
}

/// Shared index-range predicates for paged desired rows and path-only scans.
fn lexical_range_sql(
    projection: DesiredProjection,
    lower: &str,
    upper: Option<&str>,
    frontier: Option<&str>,
    shard_id: Option<&str>,
    limit: Option<usize>,
) -> (String, Vec<rusqlite::types::Value>) {
    let columns = match projection {
        DesiredProjection::Path => "path",
        DesiredProjection::PathAndOid => "path, desired_oid",
    };
    let mut sql = format!("SELECT {columns} FROM state WHERE desired_oid IS NOT NULL");
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(shard_id) = shard_id {
        sql.push_str(" AND desired_shard_id = ?");
        params.push(shard_id.to_string().into());
    }
    if let Some(after) = frontier {
        sql.push_str(" AND path > ?");
        params.push(after.to_string().into());
    } else {
        sql.push_str(" AND path >= ?");
        params.push(lower.to_string().into());
    }
    if let Some(upper) = upper {
        sql.push_str(" AND path < ?");
        params.push(upper.to_string().into());
    }
    sql.push_str(" ORDER BY path");
    if let Some(limit) = limit {
        sql.push_str(" LIMIT ?");
        params.push((limit as i64).into());
    }
    (sql, params)
}

/// One chunk of an [`DesiredSpan::Exact`] lookup: `path IN (...)`,
/// optionally shard-restricted, ordered so the concatenation of chunks
/// over a globally sorted path list stays globally sorted.
fn exact_chunk_sql(
    chunk: &[&str],
    shard_id: Option<&str>,
) -> (String, Vec<rusqlite::types::Value>) {
    let placeholders = sql_placeholders("?", chunk.len());
    let mut sql = format!(
        "SELECT path, desired_oid FROM state
         WHERE desired_oid IS NOT NULL AND path IN ({placeholders})"
    );
    let mut params: Vec<rusqlite::types::Value> =
        chunk.iter().map(|p| (*p).to_string().into()).collect();
    if let Some(shard_id) = shard_id {
        sql.push_str(" AND desired_shard_id = ?");
        params.push(shard_id.to_string().into());
    }
    sql.push_str(" ORDER BY path");
    (sql, params)
}

/// A `path`-ordered cursor over desired-state rows, produced by
/// [`StateStore::with_desired_rows`]. Rows are decoded one at a
/// time and already residual-filtered; nothing beyond one chunk of an
/// exact lookup is ever held in memory, so a full traversal streams
/// rather than materializing a `Vec<Entry>`/[`gat_core::lock::Lock`] first.
pub struct DesiredRows<'a> {
    source: RowSource<'a>,
    residual: Option<&'a Selection>,
}

enum RowSource<'a> {
    /// A single prepared statement's live result set (`All`/`Scope`).
    Range(rusqlite::Rows<'a>),
    /// A chunked exact lookup: `SQLite`'s bind-variable limit forces more
    /// than one statement past [`sql_chunk_size`] paths, and a `rusqlite::Rows`
    /// borrows the statement that produced it, so each chunk is drained
    /// into a bounded buffer before the next one is prepared. Peak memory
    /// stays at one chunk regardless of how many paths were requested.
    Exact(ExactCursor<'a>),
    /// A [`DesiredSpan::Union`]'s coalesced ranges and exact chunks,
    /// merged across multiple bounded statements (see [`UnionCursor`]).
    Union(Box<UnionCursor<'a>>),
}

struct ExactCursor<'a> {
    conn: &'a Connection,
    shard_id: Option<&'a str>,
    /// Requested paths, sorted and deduplicated, so chunk-by-chunk
    /// draining yields globally `path`-ordered rows exactly as the
    /// single-statement spans do.
    paths: Vec<&'a str>,
    next: usize,
    buffer: std::vec::IntoIter<DesiredRow>,
}

impl ExactCursor<'_> {
    fn next_row(&mut self) -> Result<Option<DesiredRow>> {
        loop {
            if let Some(row) = self.buffer.next() {
                return Ok(Some(row));
            }
            if self.next >= self.paths.len() {
                return Ok(None);
            }
            let end = (self.next + sql_chunk_size(1)).min(self.paths.len());
            let chunk = &self.paths[self.next..end];
            self.next = end;
            let (sql, params) = exact_chunk_sql(chunk, self.shard_id);
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_sql_statement_prepared();
            let mut stmt = self
                .conn
                .prepare(&sql)
                .state_context("preparing desired-state exact cursor")?;
            let rows = stmt
                .query_map(params_from_iter(params.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .state_context("querying desired-state exact rows")?;
            let mut decoded = Vec::with_capacity(chunk.len());
            for row in rows {
                let (path, oid) = row.state_context("reading desired-state row")?;
                #[cfg(any(test, feature = "test-support"))]
                test_support::record_candidate_row_visited();
                decoded.push(decode_desired_row_raw(path, oid)?);
            }
            self.buffer = decoded.into_iter();
        }
    }
}

/// Page size for one bounded fetch of a union plan's range stream --
/// deliberately independent of [`sql_chunk_size`]'s bind-variable
/// transport budget. A range fetch always binds a small, fixed number
/// of parameters (lower/upper/frontier plus an optional shard id) no
/// matter how many rows `LIMIT` returns, so the bind budget gives no
/// reason to buffer tens of thousands of rows per page; this is purely
/// a memory/laziness knob bounding how much of the *current* range
/// [`RangeStream::advance`] may hold before the caller (or the union
/// cursor's other stream) is given a chance to run.
const RANGE_STREAM_PAGE_ROWS: usize = 512;

/// The range half of a [`DesiredSpan::Union`] plan: [`span_from_bounds`]
/// has already sorted and coalesced `ranges` into a globally ordered,
/// disjoint sequence, so walking them in order and keyset-paginating
/// only the *current* range yields a globally `path`-ordered stream
/// without ever holding more than one range's current page in memory.
/// The next range is only opened once the current one is confirmed
/// exhausted (an empty fetch), so a plan with many populated ranges
/// never touches a later range before the caller has drained the
/// earlier ones.
struct RangeStream<'a> {
    conn: &'a Connection,
    shard_id: Option<&'a str>,
    ranges: Vec<LexicalRange>,
    range_idx: usize,
    /// The last path yielded from the current range (`ranges[range_idx]`),
    /// so the next fetch resumes strictly after it. Reset to `None` when
    /// moving to a new range, so that range's first fetch uses its
    /// `lower` bound inclusively.
    frontier: Option<gat_core::lexical_path::GatPath>,
    buffer: std::vec::IntoIter<DesiredRow>,
    peeked: Option<DesiredRow>,
}

impl RangeStream<'_> {
    /// Make sure `self.peeked` holds the next row (fetching more,
    /// possibly through a fresh statement over the current or a
    /// subsequent range, as needed). A no-op once every range is
    /// exhausted or a row is already peeked.
    fn advance(&mut self) -> Result<()> {
        if self.peeked.is_some() {
            return Ok(());
        }
        loop {
            if let Some(row) = self.buffer.next() {
                self.peeked = Some(row);
                return Ok(());
            }
            if self.range_idx >= self.ranges.len() {
                return Ok(());
            }
            let range = &self.ranges[self.range_idx];
            let (sql, params) = range_chunk_sql(
                &range.lower,
                range.upper.as_deref(),
                self.frontier
                    .as_ref()
                    .map(gat_core::lexical_path::GatPath::as_str),
                self.shard_id,
                RANGE_STREAM_PAGE_ROWS,
            );
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_sql_statement_prepared();
            let mut stmt = self
                .conn
                .prepare(&sql)
                .state_context("preparing desired-state union range cursor")?;
            let rows = stmt
                .query_map(params_from_iter(params.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .state_context("querying desired-state union range rows")?;
            let mut decoded = Vec::new();
            for row in rows {
                let (path, oid) = row.state_context("reading desired-state row")?;
                #[cfg(any(test, feature = "test-support"))]
                test_support::record_candidate_row_visited();
                decoded.push(decode_desired_row_raw(path, oid)?);
            }
            if decoded.is_empty() {
                // This range is exhausted: move on to the next one
                // without ever having opened a statement for it early.
                self.range_idx += 1;
                self.frontier = None;
                continue;
            }
            self.frontier = Some(decoded[decoded.len() - 1].path.clone());
            self.buffer = decoded.into_iter();
        }
    }

    /// A cheap lower bound on the path this stream's next row (if any)
    /// could have, without preparing or advancing any statement: the
    /// already-peeked row's path if one is buffered, the current
    /// in-memory page's next row if one is still queued, otherwise the
    /// current range's own `lower` bound (the real next row, once
    /// fetched, can only be `>=` it). Returns `None` once every range is
    /// confirmed exhausted. Used by [`UnionCursor::next_row`] to decide
    /// which side could possibly hold the next logical row without
    /// forcing this stream to fetch just to find out.
    fn lower_bound(&self) -> Option<&str> {
        if let Some(row) = &self.peeked {
            return Some(row.path.as_str());
        }
        if let Some(row) = self.buffer.as_slice().first() {
            return Some(row.path.as_str());
        }
        self.ranges.get(self.range_idx).map(|r| r.lower.as_str())
    }
}

/// The exact half of a [`DesiredSpan::Union`] plan: [`span_from_bounds`]
/// has already sorted `exact` (and dropped points a range already
/// covers), so draining it one bounded `IN (...)` chunk at a time keeps
/// the stream globally `path`-ordered exactly like [`ExactCursor`],
/// without cloning `plan.exact`'s strings into a fresh owned chunk --
/// only borrowed `&str` slices are built per chunk for binding.
struct ExactStream<'a> {
    conn: &'a Connection,
    shard_id: Option<&'a str>,
    paths: Vec<String>,
    next: usize,
    buffer: std::vec::IntoIter<DesiredRow>,
    peeked: Option<DesiredRow>,
}

impl ExactStream<'_> {
    fn advance(&mut self) -> Result<()> {
        if self.peeked.is_some() {
            return Ok(());
        }
        loop {
            if let Some(row) = self.buffer.next() {
                self.peeked = Some(row);
                return Ok(());
            }
            if self.next >= self.paths.len() {
                return Ok(());
            }
            let end = (self.next + sql_chunk_size(1)).min(self.paths.len());
            let refs: Vec<&str> = self.paths[self.next..end]
                .iter()
                .map(String::as_str)
                .collect();
            self.next = end;
            let (sql, params) = exact_chunk_sql(&refs, self.shard_id);
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_sql_statement_prepared();
            let mut stmt = self
                .conn
                .prepare(&sql)
                .state_context("preparing desired-state union exact cursor")?;
            let rows = stmt
                .query_map(params_from_iter(params.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .state_context("querying desired-state union exact rows")?;
            let mut decoded = Vec::new();
            for row in rows {
                let (path, oid) = row.state_context("reading desired-state row")?;
                #[cfg(any(test, feature = "test-support"))]
                test_support::record_candidate_row_visited();
                decoded.push(decode_desired_row_raw(path, oid)?);
            }
            self.buffer = decoded.into_iter();
        }
    }

    /// A cheap lower bound on the path this stream's next row (if any)
    /// could have, without preparing or advancing any statement -- see
    /// [`RangeStream::lower_bound`] for why this lets
    /// [`UnionCursor::next_row`] stay lazy.
    fn lower_bound(&self) -> Option<&str> {
        if let Some(row) = &self.peeked {
            return Some(row.path.as_str());
        }
        if let Some(row) = self.buffer.as_slice().first() {
            return Some(row.path.as_str());
        }
        self.paths.get(self.next).map(String::as_str)
    }
}

/// A [`DesiredSpan::Union`] plan's candidate rows, merged in global
/// `path` order across exactly two ordered streams -- [`RangeStream`]
/// and [`ExactStream`] -- rather than one source per bound. Because
/// [`span_from_bounds`] already sorted/coalesced the ranges and sorted
/// the exacts, comparing just these two current heads is enough to
/// preserve global order and uniqueness (ranges and exacts are
/// disjoint by construction). This keeps the cursor lazy and its memory
/// independent of how many disjoint ranges or exact points a plan has:
/// [`Self::open`] does not touch either stream at all, and
/// [`Self::next_row`] only advances a stream once its own cheap
/// [`RangeStream::lower_bound`]/[`ExactStream::lower_bound`] shows it
/// could actually hold the next logical row -- never unconditionally --
/// so a sparse side (an early exact hit followed by many empty ranges,
/// or an early range hit followed by many empty exact chunks) never
/// forces the other, irrelevant side to prepare a statement just to be
/// compared against. Comparing by reference (rather than a
/// `BinaryHeap<(String, usize)>` keyed on cloned paths) also avoids an
/// extra path allocation per candidate.
pub(crate) struct UnionCursor<'a> {
    ranges: RangeStream<'a>,
    exact: ExactStream<'a>,
}

impl<'a> UnionCursor<'a> {
    fn open(conn: &'a Connection, shard_id: Option<&'a str>, plan: UnionPlan) -> Self {
        let ranges = RangeStream {
            conn,
            shard_id,
            ranges: plan.ranges,
            range_idx: 0,
            frontier: None,
            buffer: Vec::new().into_iter(),
            peeked: None,
        };
        let exact = ExactStream {
            conn,
            shard_id,
            paths: plan.exact,
            next: 0,
            buffer: Vec::new().into_iter(),
            peeked: None,
        };
        Self { ranges, exact }
    }

    /// Yield the next logical row in global `path` order, advancing only
    /// whichever stream could actually contain it. Each iteration reads
    /// both streams' cheap [`RangeStream::lower_bound`]/
    /// [`ExactStream::lower_bound`] first (no statement preparation, no
    /// fetch); if one side already has a peeked row that is proven
    /// minimal (`<=` the other side's lower bound, actual or estimated),
    /// it is returned immediately without ever touching the other
    /// stream. Otherwise the side whose bound is smaller is advanced --
    /// turning its estimate into an actual row -- and the loop
    /// re-evaluates; this converges in at most one extra iteration per
    /// side, since each iteration either returns or moves exactly one
    /// stream from "not yet peeked" to "peeked".
    fn next_row(&mut self) -> Result<Option<DesiredRow>> {
        loop {
            let ranges_bound = self.ranges.lower_bound();
            let exact_bound = self.exact.lower_bound();
            match (ranges_bound, exact_bound) {
                (None, None) => return Ok(None),
                (Some(_), None) => {
                    self.ranges.advance()?;
                    return Ok(self.ranges.peeked.take());
                }
                (None, Some(_)) => {
                    self.exact.advance()?;
                    return Ok(self.exact.peeked.take());
                }
                (Some(rb), Some(eb)) => {
                    if self.ranges.peeked.is_some() && rb <= eb {
                        return Ok(self.ranges.peeked.take());
                    }
                    if self.exact.peeked.is_some() && eb <= rb {
                        return Ok(self.exact.peeked.take());
                    }
                    if rb <= eb {
                        self.ranges.advance()?;
                    } else {
                        self.exact.advance()?;
                    }
                }
            }
        }
    }
}

impl DesiredRows<'_> {
    /// Decode and return the next selected desired row in `path` order, or
    /// `None` once the cursor is exhausted. Rows the residual
    /// [`Selection`] rejects are skipped here rather than being handed to
    /// the caller, so storage narrowing can safely return a superset.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<DesiredRow>> {
        loop {
            let row = match &mut self.source {
                RowSource::Range(rows) => {
                    match rows.next().state_context("reading desired-state row")? {
                        None => return Ok(None),
                        Some(row) => {
                            let path: String =
                                row.get(0).state_context("reading desired-state row")?;
                            let oid: Vec<u8> =
                                row.get(1).state_context("reading desired-state row")?;
                            #[cfg(any(test, feature = "test-support"))]
                            test_support::record_candidate_row_visited();
                            decode_desired_row_raw(path, oid)?
                        }
                    }
                }
                // `ExactCursor`/`RangeStream`/`ExactStream` already record
                // each raw SQLite row as it is decoded off a statement, at
                // actual storage-row consumption time rather than here --
                // buffering a whole chunk/page ahead of yielding it as a
                // logical row would otherwise let the counter understate
                // (or mistime) how much storage work an early exit like
                // `desired_any()` actually did.
                RowSource::Exact(cursor) => match cursor.next_row()? {
                    None => return Ok(None),
                    Some(row) => row,
                },
                RowSource::Union(cursor) => match cursor.next_row()? {
                    None => return Ok(None),
                    Some(row) => row,
                },
            };
            match self.residual {
                Some(selection) if !selection.matches(&row.path) => {}
                _ => return Ok(Some(row)),
            }
        }
    }
}

/// Run `f` against a `path`-ordered, residual-filtered cursor over the
/// rows `query` describes, against any connection (a plain store
/// connection or an open desired-state transaction).
pub(super) fn with_desired_rows_on<T, E>(
    conn: &Connection,
    query: DesiredQuery<'_>,
    f: impl FnOnce(DesiredRows<'_>) -> std::result::Result<T, E>,
) -> std::result::Result<T, E>
where
    E: From<StateStoreError>,
{
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_desired_rows_call();
    match query.span {
        DesiredSpan::Empty => f(DesiredRows {
            source: RowSource::Exact(ExactCursor {
                conn,
                shard_id: query.shard_id,
                paths: Vec::new(),
                next: 0,
                buffer: Vec::new().into_iter(),
            }),
            residual: query.residual,
        }),
        DesiredSpan::Exact(paths) => {
            let mut sorted: Vec<&str> = paths
                .iter()
                .map(gat_core::lexical_path::GatPath::as_str)
                .collect();
            sorted.sort_unstable();
            sorted.dedup();
            f(DesiredRows {
                source: RowSource::Exact(ExactCursor {
                    conn,
                    shard_id: query.shard_id,
                    paths: sorted,
                    next: 0,
                    buffer: Vec::new().into_iter(),
                }),
                residual: query.residual,
            })
        }
        DesiredSpan::All | DesiredSpan::Scope(_) => {
            let (sql, params) = match &query.span {
                DesiredSpan::Scope(scope) => range_sql(Some(scope), query.shard_id),
                _ => range_sql(None, query.shard_id),
            };
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_sql_statement_prepared();
            let mut stmt = conn
                .prepare(&sql)
                .state_context("preparing desired-state cursor")
                .map_err(E::from)?;
            let rows = stmt
                .query(params_from_iter(params.iter()))
                .state_context("opening desired-state cursor")
                .map_err(E::from)?;
            f(DesiredRows {
                source: RowSource::Range(rows),
                residual: query.residual,
            })
        }
        DesiredSpan::Union(plan) => {
            let cursor = UnionCursor::open(conn, query.shard_id, plan);
            f(DesiredRows {
                source: RowSource::Union(Box::new(cursor)),
                residual: query.residual,
            })
        }
    }
}

/// Collect the entries `query` describes into a `Vec`, in `path` order --
/// the bounded-set consumption mode over [`with_desired_rows_on`].
pub(super) fn desired_rows_on(conn: &Connection, query: DesiredQuery<'_>) -> Result<Vec<Entry>> {
    with_desired_rows_on(conn, query, |mut rows| {
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            entries.push(row.into_entry());
        }
        Ok(entries)
    })
}

/// Whether *any* row matches `query`, stopping at the first hit -- the
/// existence-check consumption mode (e.g. `gat rm`'s glob-vs-literal
/// classification, `gat mount add`'s prefix-occupancy preflight), which
/// must never degrade into loading a matching row set just to ask
/// `is_empty()`.
pub(super) fn desired_any_on(conn: &Connection, query: DesiredQuery<'_>) -> Result<bool> {
    with_desired_rows_on(conn, query, |mut rows| Ok(rows.next()?.is_some()))
}

/// The `SELECT COUNT(*)` counterpart of [`range_sql`]: same predicate, no
/// `path`/row projection or `ORDER BY`, so a caller that only needs *how
/// many* rows match never pays for decoding any of them.
fn count_sql(
    scope: Option<&gat_core::lexical_path::GatPath>,
    shard_id: Option<&str>,
) -> (String, Vec<rusqlite::types::Value>) {
    let mut sql = String::from("SELECT COUNT(*) FROM state WHERE desired_oid IS NOT NULL");
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(shard_id) = shard_id {
        sql.push_str(" AND desired_shard_id = ?");
        params.push(shard_id.to_string().into());
    }
    if let Some(scope) = scope {
        let (lower, upper) = descendant_range(scope.as_str());
        sql.push_str(" AND (path = ? OR (path >= ? AND path < ?))");
        params.push(scope.as_str().to_string().into());
        params.push(lower.into());
        params.push(upper.into());
    }
    (sql, params)
}

/// The number of desired rows `query` describes, without materializing or
/// decoding any of them (e.g. `gat mount show`'s tracked-row count over a
/// target scope, which can be arbitrarily large). `Scope`/`All` spans with
/// no residual [`Selection`] run one `COUNT(*)`; every other shape (an
/// `Exact`/`Union` span, or a residual filter that can only be evaluated in
/// Rust) falls back to advancing the same cursor `desired_any_on` uses,
/// still without collecting a `Vec<Entry>`.
pub(super) fn desired_count_on(conn: &Connection, query: DesiredQuery<'_>) -> Result<u64> {
    match (&query.span, query.residual) {
        (DesiredSpan::Scope(scope), None) => {
            let (sql, params) = count_sql(Some(scope), query.shard_id);
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_sql_statement_prepared();
            let n: u64 = conn
                .query_row(&sql, params_from_iter(params.iter()), |row| {
                    let count = row.get::<_, i64>(0)?;
                    u64::try_from(count)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, count))
                })
                .state_context("counting desired-state scope rows")?;
            Ok(n)
        }
        (DesiredSpan::All, None) => {
            let (sql, params) = count_sql(None, query.shard_id);
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_sql_statement_prepared();
            let n: u64 = conn
                .query_row(&sql, params_from_iter(params.iter()), |row| {
                    let count = row.get::<_, i64>(0)?;
                    u64::try_from(count)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, count))
                })
                .state_context("counting desired-state rows")?;
            Ok(n)
        }
        (DesiredSpan::Empty, _) => Ok(0),
        _ => with_desired_rows_on(conn, query, |mut rows| {
            let mut n: u64 = 0;
            while rows.next()?.is_some() {
                n += 1;
            }
            Ok(n)
        }),
    }
}

impl StateStore {
    /// Visit desired paths in lexical order, seeking past excluded subtrees.
    /// Complement intervals share the desired query layer's range predicates.
    /// One read transaction keeps all interval scans on a coherent snapshot.
    /// The callback must not write through this store or open another transaction.
    /// Callback failures stop enumeration and release the snapshot.
    pub fn visit_desired_paths_excluding<E>(
        &self,
        excluded: &DesiredPathExclusions,
        mut visit: impl FnMut(&gat_core::lexical_path::GatPath) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError>,
    {
        if excluded.ranges.is_empty() {
            return self.with_desired_paths(|mut paths| {
                while let Some(path) = paths.next().map_err(E::from)? {
                    visit(&path)?;
                }
                Ok(())
            });
        }
        let snapshot = self
            .conn
            .unchecked_transaction()
            .state_context("opening pruned desired-path snapshot")
            .map_err(E::from)?;
        let mut lower = "";
        for next in excluded
            .ranges
            .iter()
            .map(Some)
            .chain(std::iter::once(None))
        {
            let (sql, params) = lexical_range_sql(
                DesiredProjection::Path,
                lower,
                next.map(|range| range.lower.as_str()),
                None,
                None,
                None,
            );
            let mut statement = snapshot
                .prepare_cached(&sql)
                .state_context("preparing pruned desired-path interval")
                .map_err(E::from)?;
            let rows = statement
                .query(params_from_iter(params.iter()))
                .state_context("opening pruned desired-path interval")
                .map_err(E::from)?;
            let mut paths = super::DesiredPaths { rows };
            while let Some(path) = paths.next().map_err(E::from)? {
                visit(&path)?;
            }
            if let Some(range) = next {
                match range.upper.as_deref() {
                    Some(upper) => lower = upper,
                    None => break,
                }
            }
        }
        snapshot
            .commit()
            .state_context("closing pruned desired-path snapshot")
            .map_err(E::from)
    }

    /// The cursor-oriented core of desired-state reads: run `f` against a
    /// `path`-ordered, residual-filtered cursor over exactly the rows
    /// `query` describes. Full traversals stream; nothing here builds a
    /// whole-repo `Vec<Entry>`/[`gat_core::lock::Lock`] on the caller's behalf.
    pub fn with_desired_rows<T, E>(
        &self,
        query: DesiredQuery<'_>,
        f: impl FnOnce(DesiredRows<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StateStoreError>,
    {
        with_desired_rows_on(&self.conn, query, f)
    }

    /// The collecting wrapper over [`Self::with_desired_rows`], for
    /// callers that genuinely need a concrete, bounded set (a mutation's
    /// touched-path accounting, a preflight check, a test).
    pub fn desired_rows(&self, query: DesiredQuery<'_>) -> Result<Vec<Entry>> {
        desired_rows_on(&self.conn, query)
    }

    /// Whether any desired row matches `query`, without collecting the
    /// matching rows first.
    pub fn desired_any(&self, query: DesiredQuery<'_>) -> Result<bool> {
        desired_any_on(&self.conn, query)
    }

    /// The number of desired rows matching `query`, without collecting or
    /// decoding them first. See `desired_count_on` for the SQL-`COUNT(*)`
    /// fast path.
    pub fn desired_count(&self, query: DesiredQuery<'_>) -> Result<u64> {
        desired_count_on(&self.conn, query)
    }
}

impl DesiredStateWrite<'_> {
    /// The transaction-scoped counterpart of
    /// [`StateStore::desired_rows`]: same query model, same access
    /// plans, but reading this transaction's uncommitted view.
    pub fn desired_rows(&self, query: DesiredQuery<'_>) -> Result<Vec<Entry>> {
        desired_rows_on(&self.tx, query)
    }

    /// The transaction-scoped counterpart of
    /// [`StateStore::with_desired_rows`]: streams this
    /// transaction's uncommitted view through `f` without collecting a
    /// `Vec` first. Used by the flat-shape batched mount replay
    /// (`publish_desired_flat_shard_streaming`) so a large first mount's
    /// destination-side publish stays bounded by an output buffer instead
    /// of materializing the complete post-mutation row set in memory.
    pub(crate) fn with_desired_rows<T, E>(
        &self,
        query: DesiredQuery<'_>,
        f: impl FnOnce(DesiredRows<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StateStoreError>,
    {
        with_desired_rows_on(&self.tx, query, f)
    }
}

/// One current-state row decoded during a full, shard-grouped traversal:
/// the *stored* `desired_shard_id` a writer already placed this row
/// under, next to its native-oid [`DesiredRow`]. Carrying the stored id
/// (rather than recomputing it from `row.path` and a shard-depth
/// argument) lets a full comparison group rows by shard without knowing
/// or assuming the current shard depth. Kept as a native `DesiredRow`
/// rather than an already hex-encoded `Entry` so a row dropped by the
/// comparison above it (e.g. an unchanged row `gat diff` doesn't keep)
/// never pays for `Oid::to_hex()` at all.
struct ShardedDesiredRow {
    shard_id: crate::lock::LockShardId,
    row: DesiredRow,
}

/// Decode `value` (the `desired_shard_id` column) into a
/// [`crate::lock::LockShardId`], reusing `last_shard`'s already-
/// parsed id when this row's raw spelling matches it -- an allocation-
/// free `LockShardId: PartialEq<str>` comparison -- instead of running
/// `LockShardId::parse_canonical` on every row. Since the traversal is
/// ordered by `desired_shard_id`, a mismatch only ever happens on the
/// first row of a new group, so a malformed spelling is caught
/// immediately rather than silently reused across the group.
fn decode_or_reuse_shard_id(
    last_shard: &mut Option<(String, crate::lock::LockShardId)>,
    value: rusqlite::types::ValueRef<'_>,
    path: &str,
) -> Result<crate::lock::LockShardId> {
    let raw = match value {
        rusqlite::types::ValueRef::Null => {
            return Err(StateStoreError::InvalidRow {
                detail: format!("desired-state row {path:?} has no desired_shard_id"),
            });
        }
        other => other
            .as_str()
            .map_err(rusqlite::Error::from)
            .state_context("reading desired-state row")?,
    };
    if let Some((last_raw, id)) = last_shard.as_ref()
        && last_raw == raw
    {
        return Ok(*id);
    }
    let id = decode_shard_id(raw, "desired_shard_id")?;
    *last_shard = Some((raw.to_string(), id));
    Ok(id)
}

/// A full desired-state traversal, ordered by `(desired_shard_id, path)`
/// using each row's already-stored shard id, produced by
/// [`StateStore::with_current_shard_groups`]. Rows are decoded and
/// residual-filtered one at a time; [`Self::next_group`] then drains
/// exactly one shard-id's worth of rows into a `Vec`, so a full
/// current-vs-persisted comparison consumes one logical shard at a time
/// instead of collecting every current row into a whole-state map first.
pub struct CurrentShardGroups<'a> {
    rows: rusqlite::Rows<'a>,
    residual: Option<&'a Selection>,
    pending: Option<ShardedDesiredRow>,
    /// The most recently decoded `(raw spelling, typed id)` pair -- see
    /// [`decode_or_reuse_shard_id`].
    last_shard: Option<(String, crate::lock::LockShardId)>,
}

impl CurrentShardGroups<'_> {
    fn next_row(&mut self) -> Result<Option<ShardedDesiredRow>> {
        loop {
            let Some(row) = self
                .rows
                .next()
                .state_context("reading desired-state row")?
            else {
                return Ok(None);
            };
            let shard_value = row.get_ref(0).state_context("reading desired-state row")?;
            let path: String = row.get(1).state_context("reading desired-state row")?;
            let oid: Vec<u8> = row.get(2).state_context("reading desired-state row")?;
            let shard_id = decode_or_reuse_shard_id(&mut self.last_shard, shard_value, &path)?;
            let row = ShardedDesiredRow {
                shard_id,
                row: decode_desired_row_raw(path, oid)?,
            };
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_candidate_row_visited();
            match self.residual {
                Some(selection) if !selection.matches(&row.row.path) => {}
                _ => return Ok(Some(row)),
            }
        }
    }

    /// The next shard-id group in `(shard_id, path)` order: every
    /// consecutive row sharing one `shard_id`, or `None` once the
    /// traversal is exhausted. Only that one group's rows are buffered;
    /// the first row of the next group is held in `pending` rather than
    /// discarded, so calling this repeatedly drains the whole traversal
    /// without ever holding two groups' rows at once.
    pub fn next_group(&mut self) -> Result<Option<(crate::lock::LockShardId, Vec<DesiredRow>)>> {
        let first = match self.pending.take() {
            Some(row) => row,
            None => match self.next_row()? {
                None => return Ok(None),
                Some(row) => row,
            },
        };
        let shard_id = first.shard_id;
        let mut entries = vec![first.row];
        loop {
            match self.next_row()? {
                None => break,
                Some(row) if row.shard_id == shard_id => entries.push(row.row),
                Some(row) => {
                    self.pending = Some(row);
                    break;
                }
            }
        }
        Ok(Some((shard_id, entries)))
    }
}

impl StateStore {
    /// Open a full, shard-grouped current-state traversal (see
    /// `CurrentShardGroups`), optionally residual-filtered by
    /// `selection`. Used only for an unscoped comparison: a scoped read
    /// already narrows through [`Self::with_desired_rows`] and stays
    /// bounded by the scope, so it has no need for this shard-ordered
    /// full scan.
    pub fn with_current_shard_groups<T, E>(
        &self,
        residual: Option<&Selection>,
        f: impl FnOnce(CurrentShardGroups<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StateStoreError>,
    {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_current_shard_groups_call();
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_sql_statement_prepared();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT desired_shard_id, path, desired_oid FROM state
                 WHERE desired_oid IS NOT NULL ORDER BY desired_shard_id, path",
            )
            .state_context("preparing full shard-grouped desired-state cursor")
            .map_err(E::from)?;
        let rows = stmt
            .query([])
            .state_context("opening full shard-grouped desired-state cursor")
            .map_err(E::from)?;
        f(CurrentShardGroups {
            rows,
            residual,
            pending: None,
            last_shard: None,
        })
    }
}

/// Test-only structural instrumentation for the desired-state access
/// plans this module chooses between: not timing benchmarks,
/// but counters a test can assert against to catch a regression back
/// into a quadratic or whole-state access pattern, the way plain
/// input/output equivalence tests cannot. Thread-local because
/// `cargo test` runs tests concurrently on separate threads; a global
/// counter would be racy across tests.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;
    use std::path::Path;

    use rusqlite::Connection;

    thread_local! {
        static SQL_STATEMENTS_PREPARED: Cell<usize> = const { Cell::new(0) };
        static DESIRED_ROWS_CALLS: Cell<usize> = const { Cell::new(0) };
        static CURRENT_SHARD_GROUPS_CALLS: Cell<usize> = const { Cell::new(0) };
        static CANDIDATE_ROWS_VISITED: Cell<usize> = const { Cell::new(0) };
        static EXISTING_ROWS_VISITED: Cell<usize> = const { Cell::new(0) };
        static REMOVE_PATHS_CALLS: Cell<usize> = const { Cell::new(0) };
        static SCOPED_SHARD_IDENTITIES_CALLS: Cell<usize> = const { Cell::new(0) };
        static FULL_SHARD_IDENTITIES_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    /// Opaque test capability holding an immediate `SQLite` transaction so
    /// callers can prove a read-only fast path does not attempt a write.
    pub struct WriteBlocker(Connection);

    impl Drop for WriteBlocker {
        fn drop(&mut self) {
            let _ = self.0.execute_batch("ROLLBACK;");
        }
    }

    /// # Panics
    /// Panics if the test database cannot be opened or the write lock cannot be acquired.
    #[must_use]
    pub fn block_writes(path: &Path) -> WriteBlocker {
        let connection = Connection::open(path).expect("open state database");
        connection
            .pragma_update(None, "busy_timeout", 50)
            .expect("configure state database busy timeout");
        connection
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("acquire state database write blocker");
        WriteBlocker(connection)
    }

    /// # Panics
    /// Panics if the test database cannot be opened or updated.
    pub fn clear_materialized_proof(path: &Path, gat_path: &str) {
        let connection = Connection::open(path).expect("open state database");
        connection
            .execute(
                "UPDATE state SET materialized_proof = NULL WHERE path = ?1",
                [gat_path],
            )
            .expect("clear materialized proof");
    }

    /// # Panics
    /// Panics if the test database cannot be opened or updated.
    pub fn insert_raw_materialized_row(path: &Path, gat_path: &str, oid: &[u8]) {
        let connection = Connection::open(path).expect("open state database");
        connection
            .execute(
                "INSERT INTO state(path, materialized_oid) VALUES (?1, ?2)",
                rusqlite::params![gat_path, oid],
            )
            .expect("insert raw materialized row");
    }

    /// # Panics
    /// Panics if the test database cannot be opened or updated.
    pub fn replace_raw_materialized_oid(path: &Path, gat_path: &str, oid: &[u8]) {
        let connection = Connection::open(path).expect("open state database");
        connection
            .execute(
                "UPDATE state SET materialized_oid = ?1 WHERE path = ?2",
                rusqlite::params![oid, gat_path],
            )
            .expect("replace raw materialized oid");
    }

    pub fn record_sql_statement_prepared() {
        SQL_STATEMENTS_PREPARED.with(|c| c.set(c.get() + 1));
    }

    pub fn record_desired_rows_call() {
        DESIRED_ROWS_CALLS.with(|c| c.set(c.get() + 1));
    }

    pub fn record_current_shard_groups_call() {
        CURRENT_SHARD_GROUPS_CALLS.with(|c| c.set(c.get() + 1));
    }

    pub fn record_candidate_row_visited() {
        CANDIDATE_ROWS_VISITED.with(|c| c.set(c.get() + 1));
    }

    /// Record one already-persisted `state` row read by
    /// [`crate::StateStore::find_directory_conflict`]
    /// (either its sparse per-path candidate lookups or its bulk ordered
    /// merge-walk) -- distinct from [`record_sql_statement_prepared`],
    /// since a single statement can still stream an unbounded number of
    /// rows: a test asserting "sparse for a small changed set" must
    /// count rows actually visited, not just statements prepared.
    pub fn record_existing_row_visited() {
        EXISTING_ROWS_VISITED.with(|c| c.set(c.get() + 1));
    }

    /// Record one call to `DesiredStateWrite::remove_paths`
    /// -- distinct from [`snapshot`]'s tuple since it's mutation-side (not
    /// read-side) instrumentation: a multi-glob `rm` must apply the merged
    /// claimed-set deletion exactly once, never once per glob argument.
    pub fn record_remove_paths_call() {
        REMOVE_PATHS_CALLS.with(|c| c.set(c.get() + 1));
    }

    /// Total `DesiredStateWrite::remove_paths` calls on this thread so
    /// far; see [`record_remove_paths_call`].
    ///
    pub fn remove_paths_calls() -> usize {
        REMOVE_PATHS_CALLS.with(Cell::get)
    }

    /// Record one touched-shard-scoped `shard_identities_query`
    /// call (`shard_ids: Some(..)`) -- distinct from
    /// [`record_full_shard_identities_call`] so a sparse mutation test can
    /// prove it only ever performs the scoped variant, never the
    /// whole-catalog one.
    pub fn record_scoped_shard_identities_call() {
        SCOPED_SHARD_IDENTITIES_CALLS.with(|c| c.set(c.get() + 1));
    }

    /// Record one whole-catalog `shard_identities_query`
    /// call (`shard_ids: None`); see [`record_scoped_shard_identities_call`].
    pub fn record_full_shard_identities_call() {
        FULL_SHARD_IDENTITIES_CALLS.with(|c| c.set(c.get() + 1));
    }

    /// `(scoped shard_identities calls, whole-catalog shard_identities
    /// calls)` on this thread so far.
    pub fn shard_identities_call_counts() -> (usize, usize) {
        (
            SCOPED_SHARD_IDENTITIES_CALLS.with(Cell::get),
            FULL_SHARD_IDENTITIES_CALLS.with(Cell::get),
        )
    }

    /// `(sql_statements_prepared, with_desired_rows calls,
    /// with_current_shard_groups calls, desired candidate rows visited,
    /// existing rows visited by
    /// ``find_directory_conflict``)` on this thread so far -- a test takes
    /// a reading before and after the call under test and asserts on the
    /// delta, rather than on an absolute count shared across the whole
    /// test binary.
    pub fn snapshot() -> (usize, usize, usize, usize, usize) {
        (
            SQL_STATEMENTS_PREPARED.with(Cell::get),
            DESIRED_ROWS_CALLS.with(Cell::get),
            CURRENT_SHARD_GROUPS_CALLS.with(Cell::get),
            CANDIDATE_ROWS_VISITED.with(Cell::get),
            EXISTING_ROWS_VISITED.with(Cell::get),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_layout::RepositoryLayout as Repo;
    use crate::state::shard_id_for_path;
    use gat_core::globs::GatGlobPattern;
    use gat_core::lock::LockShardLevels;
    use gat_core::oid::Oid;
    use gat_core::path_scope::{PathScope, normalize_path_scope};
    use gat_core::selection::Selection;

    fn state_directory() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path.trim_end_matches('/')).unwrap()
    }

    fn entry(path: &str, byte: u8) -> Entry {
        Entry {
            path: gp(path),
            oid: Oid::from_bytes([byte; 32]),
        }
    }

    fn store_with(paths: &[&str]) -> (tempfile::TempDir, StateStore) {
        let tmp = state_directory();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let entries: Vec<Entry> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| entry(p, (i % 251).to_le_bytes()[0] + 1))
            .collect();
        store
            .desired_write(|write| {
                write.upsert_entries(&entries, crate::lock::LockShardLevels::new(0).unwrap())
            })
            .unwrap();
        (tmp, store)
    }

    fn paths(rows: Vec<Entry>) -> Vec<String> {
        rows.into_iter().map(|e| e.path.to_string()).collect()
    }

    fn selection(path: Option<&str>, include: &[&str], exclude: &[&str]) -> Selection {
        let scope = path
            .map(std::path::Path::new)
            .map(normalize_path_scope)
            .transpose()
            .unwrap()
            .unwrap_or(PathScope::Root);
        let include = include
            .iter()
            .map(|pattern| GatGlobPattern::parse(pattern))
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let exclude = exclude
            .iter()
            .map(|pattern| GatGlobPattern::parse(pattern))
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        Selection::from_scope_patterns(scope, include, exclude)
    }

    fn candidate_rows_visited_delta(run: impl FnOnce()) -> usize {
        let before = test_support::snapshot();
        run();
        let after = test_support::snapshot();
        after.3 - before.3
    }

    #[test]
    fn pruned_path_callback_failure_releases_the_read_snapshot() {
        let (_tmp, store) = store_with(&["a", "covered/x", "z"]);
        let excluded = DesiredPathExclusions::new([gp("covered")]);
        let mut visited = 0;
        let result: std::result::Result<(), Box<dyn std::error::Error>> = store
            .visit_desired_paths_excluding(&excluded, |_| {
                visited += 1;
                Err("stop enumeration".into())
            });
        assert!(result.is_err());
        assert_eq!(visited, 1);
        assert!(store.conn.is_autocommit());
        let mut paths = Vec::new();
        store
            .visit_desired_paths_excluding(&excluded, |path| -> Result<()> {
                paths.push(path.as_str().to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(paths, ["a", "z"]);
    }

    #[test]
    fn pruned_desired_paths_seek_past_covered_and_materialized_only_rows() {
        let (_tmp, store) = store_with(&["a", "data", "data0", "z"]);
        store.conn.execute_batch("WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<9999)
            INSERT INTO state(path, desired_oid) SELECT printf('data/%05d', i), zeroblob(32) FROM n;
            WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<9999)
            INSERT INTO state(path, materialized_oid) SELECT printf('old/%05d', i), zeroblob(32) FROM n;").unwrap();
        let excluded = DesiredPathExclusions::new([gp("data/sub"), gp("data"), gp("data")]);
        assert_eq!(excluded.ranges.len(), 1);
        let mut visited = Vec::new();
        store
            .visit_desired_paths_excluding(&excluded, |path| -> Result<()> {
                visited.push(path.as_str().to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(visited, ["a", "data", "data0", "z"]);
        for (lower, upper) in [("", Some("data/")), ("data0", None)] {
            let (sql, params) =
                lexical_range_sql(DesiredProjection::Path, lower, upper, None, None, None);
            let mut explain = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let plan: Vec<String> = explain
                .query_map(params_from_iter(params.iter()), |row| row.get(3))
                .unwrap()
                .map(std::result::Result::unwrap)
                .collect();
            assert!(
                plan.iter()
                    .any(|line| line.contains("SEARCH") && line.contains("state_desired_path")),
                "{plan:?}"
            );
            let mut statement = store.conn.prepare(&sql).unwrap();
            statement
                .query_map(params_from_iter(params.iter()), |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert!(
                statement.get_status(rusqlite::StatementStatus::VmStep) < 1000,
                "query must seek past both large trees, not scan and filter them"
            );
        }
    }

    #[test]
    fn desired_path_index_is_added_to_existing_version_one_stores() {
        let (tmp, store) = store_with(&["keep"]);
        store
            .conn
            .execute_batch("DROP INDEX state_desired_path")
            .unwrap();
        drop(store);
        let repo = Repo::at(tmp.path().to_path_buf());
        let store = StateStore::open(&repo).unwrap();
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'state_desired_path'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            paths(store.desired_rows(DesiredQuery::all()).unwrap()),
            ["keep"]
        );
    }

    #[test]
    fn pruned_desired_paths_preserve_order_across_multiple_ranges() {
        let (_tmp, store) = store_with(&["a", "b/a", "c", "d/a", "e", "é/a", "ê"]);
        let excluded = DesiredPathExclusions::new([gp("é"), gp("b"), gp("d")]);
        let mut visited = Vec::new();
        store
            .visit_desired_paths_excluding(&excluded, |path| -> Result<()> {
                visited.push(path.as_str().to_owned());
                Ok(())
            })
            .unwrap();
        assert_eq!(visited, ["a", "c", "e", "ê"]);
    }

    #[test]
    fn scope_span_returns_the_exact_path_and_its_descendants_only() {
        let (_tmp, store) =
            store_with(&["data", "data.bin", "data/a.bin", "data/n/b.bin", "z.bin"]);
        assert_eq!(
            paths(
                store
                    .desired_rows(DesiredQuery::scope(&gp("data")))
                    .unwrap()
            ),
            vec!["data", "data/a.bin", "data/n/b.bin"]
        );
        // A trailing slash is a scope spelling, not a different scope.
        assert_eq!(
            paths(
                store
                    .desired_rows(DesiredQuery::scope(&gp("data/")))
                    .unwrap()
            ),
            vec!["data", "data/a.bin", "data/n/b.bin"]
        );
    }

    #[test]
    fn exact_span_never_broadens_into_a_descendant_scan() {
        let (_tmp, store) = store_with(&["data/a.bin", "data/n/b.bin"]);
        let requested = vec![gp("data"), gp("data/a.bin")];
        assert_eq!(
            paths(store.desired_rows(DesiredQuery::exact(&requested)).unwrap()),
            vec!["data/a.bin"]
        );
    }

    /// An exact lookup longer than one `IN (...)` chunk must still yield
    /// every requested row exactly once, in global `path` order -- chunking
    /// is an implementation detail of the access path, not a visible
    /// ordering or de-duplication change.
    #[test]
    fn exact_span_spans_chunks_in_global_path_order() {
        let all: Vec<String> = (0..sql_chunk_size(1) * 2 + 7)
            .map(|i| format!("data/{i:05}.bin"))
            .collect();
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        let (_tmp, store) = store_with(&refs);

        // Requested out of order and with duplicates: the cursor sorts and
        // dedups its own key list.
        let mut requested: Vec<_> = all.iter().rev().map(|p| gp(p)).collect();
        requested.push(gp(&all[3]));
        let got = paths(store.desired_rows(DesiredQuery::exact(&requested)).unwrap());
        assert_eq!(got, all);
    }

    #[test]
    fn residual_selection_filters_candidate_rows() {
        let (_tmp, store) = store_with(&["data/a.bin", "data/b.txt", "other/c.bin"]);
        let selection = selection(Some("data"), &["**/*.bin"], &[]);
        let query = DesiredQuery::for_selection(&selection);
        assert_eq!(
            paths(store.desired_rows(query).unwrap()),
            vec!["data/a.bin"]
        );
    }

    #[test]
    fn coalescing_extends_touching_ranges_and_preserves_unbounded_tails() {
        let range = |lower: &str, upper: Option<&str>| LexicalRange {
            lower: lower.to_owned(),
            upper: upper.map(str::to_owned),
        };
        let mut ranges = vec![
            range("z", Some("zz")),
            range("b", Some("d")),
            range("f", None),
            range("a", Some("b")),
            range("c", Some("e")),
            range("a", Some("a1")),
        ];
        coalesce_ranges(&mut ranges);
        assert_eq!(ranges, vec![range("a", Some("e")), range("f", None)]);
    }

    #[test]
    fn union_plan_coalesces_nested_ranges_and_drops_covered_exacts() {
        let span = span_from_bounds([
            CandidateBound::scope("data"),
            CandidateBound::prefix("data/models/model-"),
            CandidateBound::prefix("assets/"),
            CandidateBound::exact("README.bin"),
            CandidateBound::exact("data"),
            CandidateBound::exact("data/models/model-a.onnx"),
            CandidateBound::prefix("assets/"),
        ]);
        let DesiredSpan::Union(plan) = span else {
            panic!("expected a union span");
        };
        assert_eq!(
            plan.ranges,
            vec![
                LexicalRange::for_prefix("assets/"),
                LexicalRange::for_prefix("data/"),
            ]
        );
        assert_eq!(
            plan.exact,
            vec!["README.bin".to_string(), "data".to_string()]
        );
    }

    #[test]
    fn union_query_stream_is_path_ordered_and_unique() {
        let (_tmp, store) = store_with(&[
            "README.bin",
            "assets/a.bin",
            "assets/nested/b.bin",
            "data",
            "data/a.bin",
            "data/models/model-a.onnx",
            "other/z.bin",
        ]);
        let query = DesiredQuery::from_candidate_bounds([
            CandidateBound::scope("data"),
            CandidateBound::prefix("data/models/model-"),
            CandidateBound::prefix("assets/"),
            CandidateBound::exact("README.bin"),
        ]);
        assert_eq!(
            paths(store.desired_rows(query).unwrap()),
            vec![
                "README.bin",
                "assets/a.bin",
                "assets/nested/b.bin",
                "data",
                "data/a.bin",
                "data/models/model-a.onnx",
            ]
        );
    }

    /// A union plan with more disjoint exact bounds than one `IN (...)`
    /// chunk (plus a range bound) must still stream every candidate row
    /// exactly once, in global `path` order -- the whole point of
    /// merging across several small statements instead of folding every
    /// bound into one `OR` expression is that neither the bind-variable
    /// budget nor the clause count of any *single* statement grows with
    /// the number of bounds. This deliberately does not assert how many
    /// statements the cursor opens, only that the logical stream stays
    /// correct once it must open more than one.
    #[test]
    fn union_stream_spans_multiple_bounded_statements_and_stays_ordered_and_unique() {
        let exact_count = sql_chunk_size(1) + 5;
        let exact_paths: Vec<_> = (0..exact_count)
            .map(|i| format!("exact/{i:06}.bin"))
            .collect();
        let mut store_paths: Vec<&str> = exact_paths.iter().map(String::as_str).collect();
        store_paths.push("ranged/a.bin");
        store_paths.push("ranged/nested/b.bin");
        let (_tmp, store) = store_with(&store_paths);

        let mut bounds: Vec<CandidateBound<'_>> = exact_paths
            .iter()
            .map(|p| CandidateBound::exact(p))
            .collect();
        bounds.push(CandidateBound::prefix("ranged/"));
        let query = DesiredQuery::from_candidate_bounds(bounds);

        let DesiredSpan::Union(plan) = &query.span else {
            panic!("expected a union plan");
        };
        assert!(
            plan.exact.len() > sql_chunk_size(1),
            "test must exceed one exact chunk to be a meaningful regression"
        );

        let got = paths(store.desired_rows(query).unwrap());

        let mut expected = exact_paths;
        expected.push("ranged/a.bin".to_string());
        expected.push("ranged/nested/b.bin".to_string());
        expected.sort();
        assert_eq!(got, expected, "logical stream must stay path-ordered");

        let mut deduped = got.clone();
        deduped.dedup();
        assert_eq!(
            got.len(),
            deduped.len(),
            "logical stream must stay duplicate-free"
        );
    }

    /// `desired_any()` over a union plan with many disjoint, *all
    /// populated*, lexically later ranges must still stop at the first
    /// range's first row: it must neither prepare a statement for any
    /// later range nor decode any row beyond the one it reports. This is
    /// the structural counterpart of the ordering/uniqueness regression
    /// above -- it protects the cursor's laziness (a union plan's memory
    /// and statement count at `desired_any()` time must not scale with
    /// how many disjoint ranges the plan has), not just its correctness.
    #[test]
    fn desired_any_over_a_union_plan_does_not_touch_later_ranges() {
        let range_count = 50;
        let prefixes: Vec<String> = (0..range_count).map(|i| format!("range-{i:04}/")).collect();
        let store_paths: Vec<String> = prefixes.iter().map(|p| format!("{p}a.bin")).collect();
        let store_path_refs: Vec<&str> = store_paths.iter().map(String::as_str).collect();
        let (_tmp, store) = store_with(&store_path_refs);

        let bounds: Vec<CandidateBound<'_>> =
            prefixes.iter().map(|p| CandidateBound::prefix(p)).collect();
        let query = DesiredQuery::from_candidate_bounds(bounds);
        let DesiredSpan::Union(plan) = &query.span else {
            panic!("expected a union plan");
        };
        assert_eq!(plan.ranges.len(), range_count, "ranges must stay disjoint");

        let before = test_support::snapshot();
        assert!(store.desired_any(query).unwrap());
        let after = test_support::snapshot();
        assert_eq!(
            after.0 - before.0,
            1,
            "desired_any must open exactly one statement (the first range's), not one per range"
        );
        assert_eq!(
            after.3 - before.3,
            1,
            "desired_any must decode exactly the one row it reports, not every range's rows"
        );
    }

    /// An early exact hit must not force the range side to prepare a
    /// statement for any of many later, lexically-greater, entirely
    /// empty ranges: `UnionCursor::next_row` must compare cheap lower
    /// bounds and see that the exact stream's actual row already sorts
    /// before every range's own `lower` bound, so it can return without
    /// ever calling `RangeStream::advance`.
    #[test]
    fn desired_any_does_not_touch_many_later_empty_ranges_after_an_early_exact_hit() {
        let range_count = 50;
        let prefixes: Vec<String> = (0..range_count).map(|i| format!("zzz-{i:04}/")).collect();
        let (_tmp, store) = store_with(&["aaa.bin"]);

        let mut bounds = vec![CandidateBound::exact("aaa.bin")];
        bounds.extend(prefixes.iter().map(|p| CandidateBound::prefix(p)));
        let query = DesiredQuery::from_candidate_bounds(bounds);
        let DesiredSpan::Union(plan) = &query.span else {
            panic!("expected a union plan");
        };
        assert_eq!(plan.ranges.len(), range_count, "ranges must stay disjoint");
        assert_eq!(plan.exact, vec![gp("aaa.bin")]);

        let before = test_support::snapshot();
        assert!(store.desired_any(query).unwrap());
        let after = test_support::snapshot();
        assert_eq!(
            after.0 - before.0,
            1,
            "desired_any must open exactly one statement (the exact chunk's), not any range's"
        );
        assert_eq!(
            after.3 - before.3,
            1,
            "desired_any must decode exactly the one row it reports, no range row"
        );
    }

    /// An early range hit must not force the exact side to prepare a
    /// statement for any of many later, lexically-greater, entirely
    /// empty `IN (...)` chunks: `UnionCursor::next_row` must see that
    /// the range stream's actual row already sorts before the exact
    /// stream's lower bound (its smallest requested path), so it can
    /// return without ever calling `ExactStream::advance`, no matter how
    /// many chunks the exact side would otherwise need to exhaust.
    #[test]
    fn desired_any_does_not_touch_many_later_empty_exact_chunks_after_an_early_range_hit() {
        let exact_count = sql_chunk_size(1) + 5;
        let exact_paths: Vec<_> = (0..exact_count)
            .map(|i| format!("zzz/{i:06}.bin"))
            .collect();
        let (_tmp, store) = store_with(&["range/a.bin"]);

        let mut bounds: Vec<CandidateBound<'_>> = vec![CandidateBound::prefix("range/")];
        bounds.extend(exact_paths.iter().map(|p| CandidateBound::exact(p)));
        let query = DesiredQuery::from_candidate_bounds(bounds);
        let DesiredSpan::Union(plan) = &query.span else {
            panic!("expected a union plan");
        };
        assert!(
            plan.exact.len() > sql_chunk_size(1),
            "test must exceed one exact chunk to be a meaningful regression"
        );

        let before = test_support::snapshot();
        assert!(store.desired_any(query).unwrap());
        let after = test_support::snapshot();
        assert_eq!(
            after.0 - before.0,
            1,
            "desired_any must open exactly one statement (the range's), not any exact chunk"
        );
        assert_eq!(
            after.3 - before.3,
            1,
            "desired_any must decode exactly the one row it reports, no exact-chunk row"
        );
    }

    #[test]
    fn prefix_upper_bound_handles_unicode_boundaries_and_carries() {
        for (prefix, expected) in [
            ("", None),
            ("abc", Some("abd")),
            ("a\u{7f}", Some("a\u{80}")),
            ("a\u{7ff}", Some("a\u{800}")),
            ("a\u{d7ff}", Some("a\u{e000}")),
            ("a\u{ffff}", Some("a\u{10000}")),
            ("a\u{10ffff}\u{10ffff}", Some("b")),
            ("\u{10ffff}", None),
        ] {
            assert_eq!(lexical_prefix_upper(prefix).as_deref(), expected);
        }
    }

    #[test]
    fn lexical_prefix_range_handles_unicode_prefixes() {
        let (_tmp, store) = store_with(&["ßeta/a.bin", "ßeta-model.bin", "ßeta2.bin", "z.bin"]);
        let query = DesiredQuery::from_candidate_bounds([CandidateBound::prefix("ßeta")]);
        assert_eq!(
            paths(store.desired_rows(query).unwrap()),
            vec!["ßeta-model.bin", "ßeta/a.bin", "ßeta2.bin"]
        );
    }

    #[test]
    fn any_candidate_bound_forces_full_scan_fallback() {
        let query = DesiredQuery::from_candidate_bounds([
            CandidateBound::prefix("data/"),
            CandidateBound::Any,
            CandidateBound::exact("README.bin"),
        ]);
        assert!(matches!(query.span, DesiredSpan::All));
    }

    #[test]
    fn selection_include_bounds_narrow_candidate_rows() {
        let (_tmp, store) = store_with(&[
            "data/models/a.onnx",
            "data/models/b.bin",
            "data/other/c.onnx",
            "data/x.txt",
            "other/d.onnx",
        ]);
        let selection = selection(Some("data"), &["models/**/*.onnx"], &[]);
        let query = DesiredQuery::for_selection(&selection);
        let visited = candidate_rows_visited_delta(|| {
            let got = paths(store.desired_rows(query.clone()).unwrap());
            assert_eq!(got, vec!["data/models/a.onnx"]);
        });
        assert_eq!(visited, 2);
    }

    #[test]
    fn selection_scope_and_mid_component_include_prefix() {
        let (_tmp, store) = store_with(&[
            "data/models/model-a.onnx",
            "data/models/other.onnx",
            "data/models/model-b.bin",
            "data/other/model-c.onnx",
        ]);
        let selection = selection(Some("data"), &["models/model-*.onnx"], &[]);
        let query = DesiredQuery::for_selection(&selection);
        let visited = candidate_rows_visited_delta(|| {
            let got = paths(store.desired_rows(query.clone()).unwrap());
            assert_eq!(got, vec!["data/models/model-a.onnx"]);
        });
        assert_eq!(visited, 2);
    }

    #[test]
    fn selection_exact_include_uses_exact_candidate_lookup() {
        let (_tmp, store) = store_with(&["models/a.onnx", "models/b.onnx", "other/c.onnx"]);
        let selection = selection(None, &["models/a.onnx"], &[]);
        let query = DesiredQuery::for_selection(&selection);
        let DesiredSpan::Union(plan) = &query.span else {
            panic!("expected a union plan");
        };
        assert!(plan.ranges.is_empty());
        assert_eq!(plan.exact, vec!["models/a.onnx".to_string()]);
        let visited = candidate_rows_visited_delta(|| {
            let got = paths(store.desired_rows(query.clone()).unwrap());
            assert_eq!(got, vec!["models/a.onnx"]);
        });
        assert_eq!(visited, 1);
    }

    #[test]
    fn selection_excludes_remain_residual_filters() {
        let (_tmp, store) =
            store_with(&["models/keep/a.onnx", "models/skip/b.onnx", "other/c.onnx"]);
        let selection = selection(None, &["models/**/*.onnx"], &["models/skip/**"]);
        let query = DesiredQuery::for_selection(&selection);
        let visited = candidate_rows_visited_delta(|| {
            let got = paths(store.desired_rows(query.clone()).unwrap());
            assert_eq!(got, vec!["models/keep/a.onnx"]);
        });
        assert_eq!(visited, 2);
    }

    #[test]
    fn selection_any_include_does_not_narrow_beyond_scope() {
        let selection = selection(Some("data"), &["**/*.onnx"], &[]);
        let query = DesiredQuery::for_selection(&selection);
        assert!(matches!(query.span, DesiredSpan::Scope(path) if *path == gp("data")));
    }

    /// `for_selection` narrows storage by `scope_path()` but must never
    /// narrow away a row `matches()` would have accepted, and must never
    /// hand back a row `matches()` rejects.
    #[test]
    fn for_selection_agrees_with_matches_over_the_whole_state() {
        let all = ["a.bin", "data/a.bin", "data/n/b.txt", "data2/c.bin"];
        let (_tmp, store) = store_with(&all);
        for selection in [
            selection(None, &[], &[]),
            selection(Some("data"), &[], &[]),
            selection(None, &["**/*.bin"], &[]),
            selection(None, &[], &["**/*.txt"]),
            selection(Some("data"), &[], &["**/*.txt"]),
        ] {
            let expected: Vec<String> = all
                .iter()
                .filter(|p| selection.matches(&gp(p)))
                .map(|p| (*p).to_string())
                .collect();
            let got = paths(
                store
                    .desired_rows(DesiredQuery::for_selection(&selection))
                    .unwrap(),
            );
            assert_eq!(got, expected, "selection {selection:?}");
        }
    }

    #[test]
    fn desired_any_stops_at_the_first_row_and_reports_emptiness() {
        let (_tmp, store) = store_with(&["data/a.bin"]);
        assert!(store.desired_any(DesiredQuery::scope(&gp("data"))).unwrap());
        assert!(
            store
                .desired_any(DesiredQuery::scope(&gp("data/a.bin")))
                .unwrap()
        );
        assert!(!store.desired_any(DesiredQuery::scope(&gp("dat"))).unwrap());
        assert!(
            !store
                .desired_any(DesiredQuery::scope(&gp("other")))
                .unwrap()
        );
        let missing = vec![gp("nope.bin")];
        assert!(!store.desired_any(DesiredQuery::exact(&missing)).unwrap());
    }

    /// `mount show`'s tracked-row count must be a `COUNT(*)`,
    /// not a full row materialize-then-`.len()` -- verified here by
    /// agreement with the collecting path over every query shape
    /// `desired_count` fast-paths (`Scope`/`All`), plus the generic cursor
    /// fallback for an `Exact` span.
    #[test]
    fn desired_count_agrees_with_collecting_every_matching_row() {
        let (_tmp, store) = store_with(&["data/a.bin", "data/b.bin", "data.bin", "other.bin"]);
        assert_eq!(
            store
                .desired_count(DesiredQuery::scope(&gp("data")))
                .unwrap(),
            store
                .desired_rows(DesiredQuery::scope(&gp("data")))
                .unwrap()
                .len() as u64
        );
        assert_eq!(
            store
                .desired_count(DesiredQuery::scope(&gp("missing")))
                .unwrap(),
            0
        );
        assert_eq!(
            store.desired_count(DesiredQuery::all()).unwrap(),
            store.desired_rows(DesiredQuery::all()).unwrap().len() as u64
        );
        let paths = vec![gp("data/a.bin"), gp("other.bin")];
        assert_eq!(store.desired_count(DesiredQuery::exact(&paths)).unwrap(), 2);
    }

    #[test]
    fn all_span_streams_every_row_in_path_order() {
        let (_tmp, store) = store_with(&["z.bin", "a.bin", "m/n.bin"]);
        let streamed = store
            .with_desired_rows(DesiredQuery::all(), |mut rows| -> Result<Vec<String>> {
                let mut seen = Vec::new();
                while let Some(row) = rows.next()? {
                    seen.push(row.path.to_string());
                }
                Ok(seen)
            })
            .unwrap();
        assert_eq!(streamed, vec!["a.bin", "m/n.bin", "z.bin"]);
        assert_eq!(
            streamed,
            paths(store.desired_rows(DesiredQuery::all()).unwrap())
        );
    }

    fn store_sharded_with(
        paths: &[&str],
        shard_levels: crate::lock::LockShardLevels,
    ) -> (tempfile::TempDir, StateStore) {
        let tmp = state_directory();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let entries: Vec<Entry> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| entry(p, (i % 251).to_le_bytes()[0] + 1))
            .collect();
        store
            .desired_write(|write| write.upsert_entries(&entries, shard_levels))
            .unwrap();
        (tmp, store)
    }

    /// A full shard-grouped traversal must yield every row exactly once,
    /// grouped so that every row in one group shares the same *stored*
    /// `desired_shard_id`, groups themselves in ascending shard-id order,
    /// and rows within a group in `path` order -- the ordering the
    /// same-shape merge in `commands::compare` depends on.
    #[test]
    fn current_shard_groups_partitions_rows_by_their_stored_shard_id() {
        let all: Vec<&str> = vec![
            "data/aaa.bin",
            "data/bbb.bin",
            "data/ccc.bin",
            "other/ddd.bin",
            "other/eee.bin",
            "z.bin",
        ];
        let (_tmp, store) = store_sharded_with(&all, crate::lock::LockShardLevels::new(2).unwrap());

        let groups = store
            .with_current_shard_groups(
                None,
                |mut groups| -> Result<Vec<(crate::lock::LockShardId, Vec<DesiredRow>)>> {
                    let mut out = Vec::new();
                    while let Some((id, entries)) = groups.next_group()? {
                        out.push((id, entries));
                    }
                    Ok(out)
                },
            )
            .unwrap();

        // Groups are in ascending shard-id order.
        let ids: Vec<crate::lock::LockShardId> = groups.iter().map(|(id, _)| *id).collect();
        let mut sorted_ids = ids.clone();
        sorted_ids.sort();
        assert_eq!(ids, sorted_ids);

        // Every row appears exactly once, and lands in the group its
        // stored `desired_shard_id` says it should.
        let mut seen: Vec<String> = Vec::new();
        for (id, entries) in &groups {
            let mut prev: Option<&str> = None;
            for e in entries {
                assert_eq!(
                    shard_id_for_path(&e.path, LockShardLevels::new(2).unwrap()),
                    *id
                );
                if let Some(prev) = prev {
                    assert!(
                        prev < e.path.as_str(),
                        "rows within a group must be path-ordered"
                    );
                }
                prev = Some(e.path.as_str());
                seen.push(e.path.to_string());
            }
        }
        seen.sort();
        let mut expected: Vec<_> = all.iter().map(std::string::ToString::to_string).collect();
        expected.sort();
        assert_eq!(seen, expected);
    }

    /// A residual [`Selection`] must be applied to every row a
    /// shard-grouped traversal yields, exactly like [`DesiredRows`].
    #[test]
    fn current_shard_groups_applies_the_residual_selection() {
        let all: Vec<&str> = vec!["data/a.bin", "data/b.txt", "other/c.bin"];
        let (_tmp, store) = store_sharded_with(&all, crate::lock::LockShardLevels::new(1).unwrap());
        let selection = selection(None, &["**/*.bin"], &[]);

        let paths: Vec<String> = store
            .with_current_shard_groups(Some(&selection), |mut groups| -> Result<Vec<String>> {
                let mut out = Vec::new();
                while let Some((_, entries)) = groups.next_group()? {
                    out.extend(entries.into_iter().map(|e| e.path.to_string()));
                }
                Ok(out)
            })
            .unwrap();
        let mut paths = paths;
        paths.sort();
        assert_eq!(paths, vec!["data/a.bin", "other/c.bin"]);
    }

    /// The parse-once-per-group cache introduced for this cursor must
    /// never let a malformed `desired_shard_id` slip through: since
    /// `decode_or_reuse_shard_id` only reuses the previous row's typed
    /// id when the raw spelling still matches it, a spelling change --
    /// including a change *into* garbage -- always re-parses and fails
    /// immediately, rather than being silently reused from the last
    /// good group or skipped.
    #[test]
    fn current_shard_groups_fails_on_the_first_row_of_a_malformed_shard_group() {
        let all: Vec<&str> = vec!["data/aaa.bin", "data/bbb.bin", "other/ccc.bin"];
        let (_tmp, store) = store_sharded_with(&all, crate::lock::LockShardLevels::new(2).unwrap());
        store
            .conn
            .execute(
                "UPDATE state SET desired_shard_id = 'not-a-shard-id' WHERE path = 'other/ccc.bin'",
                [],
            )
            .unwrap();

        let err = store
            .with_current_shard_groups(None, |mut groups| -> Result<()> {
                while groups.next_group()?.is_some() {}
                Ok(())
            })
            .unwrap_err();
        assert!(
            matches!(err, StateStoreError::InvalidRow { .. }),
            "expected an InvalidRow failure, got {err:?}"
        );
    }
}
