//! The pure semantic identity of a `gat.lock` shard: which fan-out depth
//! it lives at and the hash-derived bytes that pin it to a specific leaf,
//! entirely independent of how that identity is later encoded into a
//! filesystem path, a `SQLite` `TEXT` column, or a Git tree/index path.
//! Those encodings (and their own error types) live at their respective
//! I/O boundaries in `gat-io`: lock persistence for the physical
//! shard-file layout, state persistence for `SQLite`, and Git snapshots for
//! tree/index paths. Those owners call into this module only through
//! [`LockShardId::parse_canonical`]/
//! [`LockShardId::to_canonical_string`]/[`LockShardId::write_canonical`].

/// Fixed-capacity buffer for building the canonical shard-ID spelling
/// (`"gat.lock"` or `"gat.lock/xx/.../yy.tsv"`) without a heap
/// allocation. The longest possible canonical spelling
/// (`"gat.lock/xx/xx/xx/xx.tsv"` at [`LockShardLevels::MAX`]) is well
/// under this capacity.
///
struct CanonicalIdBuf {
    buf: [u8; 32],
    len: usize,
}

impl CanonicalIdBuf {
    const fn new() -> Self {
        Self {
            buf: [0; 32],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn as_str(&self) -> &str {
        // Only ASCII bytes (`gat.lock`, `/`, `.`, hex digits, `tsv`) are
        // ever written by `LockShardId::write_canonical`.
        std::str::from_utf8(self.as_bytes()).expect("canonical shard id is always ASCII")
    }
}

impl std::fmt::Write for CanonicalIdBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let bytes = s.as_bytes();
        let end = self.len + bytes.len();
        if end > self.buf.len() {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// A validated `gat.lock` shard fan-out depth, guaranteed to fit
/// [`LockShardId`]'s four-byte `prefix`: `0` (the flat, single-file
/// sentinel) or one through [`Self::MAX`]. Every shard-placement, publication,
/// and comparison API uses this validated type, so an out-of-range depth
/// cannot be constructed and no truncating fallback exists.
///
/// This is *not* the same thing as the configured `lock.shard_levels`
/// policy limit ([`crate::config::MAX_SHARD_LEVELS`]): that's a
/// separate, tighter business policy (avoid an unreasonably large
/// `gat.lock/` tree) enforced once at the config-parsing boundary, while
/// [`Self::MAX`] is the structural limit this type itself is capable of
/// representing at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LockShardLevels(u8);

/// A depth passed to [`LockShardLevels::new`] outside zero through [`LockShardLevels::MAX`].
#[derive(Debug, thiserror::Error)]
#[error("shard fan-out depth {depth} exceeds the maximum of {max}")]
pub struct LockShardLevelsError {
    pub depth: u8,
    pub max: u8,
}

impl LockShardLevels {
    /// The highest fan-out depth [`LockShardId`] can structurally
    /// represent (its `prefix` is exactly four bytes).
    pub const MAX: u8 = 4;

    /// The flat, single-file `gat.lock` depth.
    pub const FLAT: Self = Self(0);

    /// Validates `depth`, rejecting anything outside zero through [`Self::MAX`].
    pub const fn new(depth: u8) -> Result<Self, LockShardLevelsError> {
        if depth > Self::MAX {
            return Err(LockShardLevelsError {
                depth,
                max: Self::MAX,
            });
        }
        Ok(Self(depth))
    }

    /// Whether this is the flat sentinel depth (see [`Self::FLAT`]).
    #[must_use]
    pub const fn is_flat(&self) -> bool {
        self.0 == 0
    }

    /// The validated depth as a plain `u8`, for callers that genuinely
    /// need one (config/`OnDiskShape` serialization, diagnostics).
    #[must_use]
    pub const fn get(&self) -> u8 {
        self.0
    }
}

impl serde::Serialize for LockShardLevels {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(self.0)
    }
}

/// Deserializes and validates against [`Self::MAX`] only -- the
/// *structural* limit this type itself can represent. Machine-produced
/// data this type appears in directly (today, only
/// `super::persistence::OnDiskShape::Sharded`'s reshape-journal encoding)
/// never needs the tighter `lock.shard_levels` *product* policy limit
/// ([`crate::config::MAX_SHARD_LEVELS`]); that's enforced once,
/// separately, at the `gat.yaml`/`gat config` boundary that produces a
/// depth in the first place.
impl<'de> serde::Deserialize<'de> for LockShardLevels {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let depth = u8::deserialize(deserializer)?;
        Self::new(depth).map_err(serde::de::Error::custom)
    }
}

/// Compact, `Copy` identity for one `gat.lock` shard: which fan-out depth
/// it lives at (`0` for the flat, single-file `gat.lock` sentinel; one through
/// [`LockShardLevels::MAX`] for a sharded `gat.lock/` leaf), and the
/// leading bytes of `blake3(path)` that depth's hash placement keys on.
///
/// This is the in-memory replacement for the textual `"gat.lock"` /
/// `"gat.lock/ab/cd.tsv"` shard-ID strings. `LockShardId` is deliberately kept
/// separate from any physical lock-file path: converting one to a
/// filesystem path (by `gat-io`'s private lock persistence adapter) or a
/// `SQLite` `TEXT` value is always an explicit, on-demand step at the I/O
/// boundary, never implied by equality/ordering/hashing here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LockShardId {
    pub(super) depth: u8,
    pub(super) prefix: [u8; 4],
}

/// Ordering matches the canonical persisted spelling
/// (`"gat.lock"` / `"gat.lock/ab/cd.tsv"`) without allocating: compare
/// the prefix bytes both IDs actually use (`0..min(depth)`) first, since
/// two-digit lowercase hex preserves each byte's numeric order, and only
/// fall back to comparing `depth` once every shared byte matches -- at
/// that point the shorter ID's canonical text ends right there (`.tsv`,
/// `0x2e`) while the longer one continues with another path separator
/// (`/`, `0x2f`), so the shorter ID always sorts first, exactly like the
/// derived-from-strings comparison would.
impl Ord for LockShardId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let shared = self.depth.min(other.depth) as usize;
        self.prefix[..shared]
            .cmp(&other.prefix[..shared])
            .then_with(|| self.depth.cmp(&other.depth))
    }
}

impl PartialOrd for LockShardId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A malformed shard-ID spelling passed to
/// [`LockShardId::parse_canonical`] -- deliberately carries only the
/// rejected text, not a filesystem path, `SQLite` row context, or Git tree
/// location: the caller holding that context (lock persistence, the
/// `SQLite` state store, or Git snapshot/index parsing) is the one that
/// translates this into its own boundary-appropriate error.
#[derive(Debug, thiserror::Error)]
#[error("{raw:?} is not a valid gat.lock shard identifier")]
pub struct LockShardIdError {
    pub raw: String,
}

impl LockShardId {
    /// The flat, single-file `gat.lock` shard identity every tracked path
    /// maps to when `lock.shard_levels == 0`.
    #[must_use]
    pub const fn flat() -> Self {
        Self {
            depth: 0,
            prefix: [0; 4],
        }
    }

    /// Whether this is the flat sentinel (see [`Self::flat`]).
    #[must_use]
    pub const fn is_flat(&self) -> bool {
        self.depth == 0
    }

    /// The fan-out depth this shard lives at: `0` for the flat sentinel,
    /// otherwise the number of hash-derived path components its
    /// canonical spelling has under `gat.lock/`. Always a valid
    /// [`LockShardLevels`] because `depth` is only ever set by
    /// [`Self::for_path`] (which takes an already-validated depth) or
    /// [`Self::parse_canonical`] (which rejects anything outside range).
    #[must_use]
    pub const fn levels(&self) -> LockShardLevels {
        LockShardLevels(self.depth)
    }

    /// This shard's hash-derived prefix bytes, one per fan-out level
    /// (`&[]` for the flat sentinel) -- the same bytes
    /// [`Self::write_canonical`] renders as lowercase hex. Exposed so
    /// filesystem/I/O boundaries can build a physical path without reaching
    /// into this type's private representation.
    #[must_use]
    pub fn prefix_bytes(&self) -> &[u8] {
        &self.prefix[..self.depth as usize]
    }

    /// The shard identity that owns `path`'s row at `shard_levels` fan-out
    /// levels (`shard_levels` flat means every path maps to
    /// [`Self::flat`]). Hashes the canonical path exactly once and copies
    /// only the leading `shard_levels` bytes of that hash -- never builds
    /// hexadecimal shard text. Infallible: `shard_levels` is already a
    /// validated [`LockShardLevels`], so there is no depth to reject and
    /// no truncation to perform.
    #[must_use]
    pub fn for_path(path: &crate::lexical_path::GatPath, shard_levels: LockShardLevels) -> Self {
        Self::for_validated_path(path.as_str(), shard_levels)
    }

    pub(crate) fn for_validated_path(path: &str, shard_levels: LockShardLevels) -> Self {
        if shard_levels.is_flat() {
            return Self::flat();
        }
        let hash = blake3::hash(path.as_bytes());
        let mut prefix = [0u8; 4];
        let levels = shard_levels.get() as usize;
        prefix[..levels].copy_from_slice(&hash.as_bytes()[..levels]);
        Self {
            depth: shard_levels.get(),
            prefix,
        }
    }

    /// Strictly parse one of the canonical spellings
    /// (`"gat.lock"`, `"gat.lock/ab.tsv"`, ..., `"gat.lock/ab/cd/ef/01.tsv"`)
    /// directly over bytes/ASCII, without splitting into allocated
    /// strings. Any other spelling (wrong prefix, non-hex/non-lowercase
    /// digits, missing/extra `.tsv`, depth outside zero through [`LockShardLevels::MAX`])
    /// is rejected as [`LockShardIdError`] -- translate that into the
    /// caller's own boundary error (`LockError::CorruptShard`,
    /// `StateStoreError::InvalidRow`, `GitError::InvalidLockSnapshot`).
    pub fn parse_canonical(raw: &str) -> Result<Self, LockShardIdError> {
        fn corrupt(raw: &str) -> LockShardIdError {
            LockShardIdError {
                raw: raw.to_string(),
            }
        }
        fn parse_hex_byte(b: &[u8]) -> Option<u8> {
            if b.len() != 2 {
                return None;
            }
            let hi = (b[0] as char).to_digit(16)?;
            let lo = (b[1] as char).to_digit(16)?;
            // Reject uppercase hex: the canonical spelling is always
            // lowercase, so round-tripping through parse/format must be
            // exact.
            if !b[0].is_ascii_digit() && !b[0].is_ascii_lowercase() {
                return None;
            }
            if !b[1].is_ascii_digit() && !b[1].is_ascii_lowercase() {
                return None;
            }
            u8::try_from((hi << 4) | lo).ok()
        }

        if raw == "gat.lock" {
            return Ok(Self::flat());
        }
        let rel = raw.strip_prefix("gat.lock/").ok_or_else(|| corrupt(raw))?;
        let rel = rel.strip_suffix(".tsv").ok_or_else(|| corrupt(raw))?;
        let mut prefix = [0u8; 4];
        let mut depth: usize = 0;
        for component in rel.split('/') {
            if depth >= LockShardLevels::MAX as usize {
                return Err(corrupt(raw));
            }
            prefix[depth] = parse_hex_byte(component.as_bytes()).ok_or_else(|| corrupt(raw))?;
            depth += 1;
        }
        if depth == 0 {
            return Err(corrupt(raw));
        }
        Ok(Self {
            depth: u8::try_from(depth).map_err(|_| corrupt(raw))?,
            prefix,
        })
    }

    /// Write this shard's canonical spelling (`"gat.lock"` or
    /// `"gat.lock/xx/.../yy.tsv"`) into `out` directly, with no
    /// intermediate `String` allocation.
    pub fn write_canonical(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        if self.is_flat() {
            return out.write_str("gat.lock");
        }
        out.write_str("gat.lock/")?;
        for byte in &self.prefix[..self.depth as usize - 1] {
            write!(out, "{byte:02x}/")?;
        }
        write!(out, "{:02x}.tsv", self.prefix[self.depth as usize - 1])
    }

    /// The canonical spelling as an owned `String`, for callers that
    /// genuinely need one (`SQLite` `TEXT` binding, error messages,
    /// display). Prefer [`Self::write_canonical`]/[`Self::hash_into`] on
    /// any path that doesn't otherwise need an owned `String`.
    #[allow(clippy::wrong_self_convention)]
    #[must_use]
    #[allow(
        clippy::missing_panics_doc,
        reason = "The fixed buffer fits every valid shard ID"
    )]
    pub fn to_canonical_string(&self) -> String {
        let mut buf = CanonicalIdBuf::new();
        self.write_canonical(&mut buf)
            .expect("canonical shard id always fits in the fixed buffer");
        buf.as_str().to_string()
    }

    /// Feed the length-prefixed canonical spelling into `hasher` without
    /// allocating an intermediate string. This encoding is part of the
    /// [`super::CanonicalDesiredIdentity`] contract.
    #[allow(
        clippy::missing_panics_doc,
        reason = "The fixed buffer fits every valid shard ID"
    )]
    pub fn hash_into(&self, hasher: &mut blake3::Hasher) {
        let mut buf = CanonicalIdBuf::new();
        self.write_canonical(&mut buf)
            .expect("canonical shard id always fits in the fixed buffer");
        hasher.update(&(buf.len as u64).to_le_bytes());
        hasher.update(buf.as_bytes());
    }
}

impl std::fmt::Display for LockShardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.write_canonical(f)
    }
}

impl PartialEq<str> for LockShardId {
    fn eq(&self, other: &str) -> bool {
        let mut buf = CanonicalIdBuf::new();
        self.write_canonical(&mut buf)
            .expect("canonical shard id always fits in the fixed buffer");
        buf.as_str() == other
    }
}

impl PartialEq<&str> for LockShardId {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl PartialEq<LockShardId> for str {
    fn eq(&self, other: &LockShardId) -> bool {
        other == self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gp(s: &str) -> crate::lexical_path::GatPath {
        crate::lexical_path::GatPath::parse_canonical(s).unwrap()
    }

    fn levels(n: u8) -> LockShardLevels {
        LockShardLevels::new(n).unwrap()
    }

    #[test]
    fn flat_is_depth_zero_and_has_an_all_zero_prefix() {
        let flat = LockShardId::flat();
        assert!(flat.is_flat());
        assert_eq!(flat.levels(), LockShardLevels::FLAT);
        assert_eq!(flat.to_canonical_string(), "gat.lock");
    }

    #[test]
    fn for_path_at_flat_levels_always_returns_the_flat_sentinel() {
        assert_eq!(
            LockShardId::for_path(&gp("a.bin"), levels(0)),
            LockShardId::flat()
        );
        assert_eq!(
            LockShardId::for_path(&gp("nested/b.bin"), levels(0)),
            LockShardId::flat()
        );
    }

    #[test]
    fn parse_canonical_round_trips_for_path() {
        let id = LockShardId::for_path(&gp("some/tracked/file.bin"), levels(2));
        let text = id.to_canonical_string();
        assert_eq!(LockShardId::parse_canonical(&text).unwrap(), id);
    }

    #[test]
    fn canonical_writer_matches_owned_format_without_intermediate_text() {
        for raw in [
            "gat.lock",
            "gat.lock/00.tsv",
            "gat.lock/ab/cd.tsv",
            "gat.lock/ab/cd/ef/01.tsv",
        ] {
            let id = LockShardId::parse_canonical(raw).unwrap();
            let mut out = String::new();
            id.write_canonical(&mut out).unwrap();
            assert_eq!(out, raw);
            assert_eq!(out, id.to_canonical_string());
        }
    }

    #[test]
    fn parse_canonical_rejects_malformed_spellings() {
        for bad in [
            "",
            "gat.lock/",
            "gat.lock/zz.tsv",
            "gat.lock/AB.tsv",
            "gat.lock/ab",
            "gat.lock/ab/cd/ef/01/23.tsv",
            "not-gat-lock",
        ] {
            assert!(LockShardId::parse_canonical(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ord_matches_canonical_string_ordering_even_across_mixed_depths() {
        // These specific IDs are the case a naive `#[derive(Ord)]` on
        // `(depth, prefix)` gets backwards: "ac.tsv" (depth 1) and
        // "ab/ff.tsv" (depth 2) share no prefix byte, so their canonical
        // spellings order by the *first* diverging byte ('a'=='a' then
        // 'c' > 'b'), not by depth -- a derived tuple `Ord` would instead
        // put every depth-1 ID before every depth-2 one regardless of
        // prefix, sorting "ac.tsv" before "ab/ff.tsv".
        let cases = [
            "gat.lock",
            "gat.lock/ab/00.tsv",
            "gat.lock/ab/ff.tsv",
            "gat.lock/ab.tsv",
            "gat.lock/ac.tsv",
            "gat.lock/ac/00.tsv",
        ];
        let mut ids: Vec<LockShardId> = cases
            .iter()
            .map(|s| LockShardId::parse_canonical(s).unwrap())
            .collect();
        ids.sort();
        let sorted_strings: Vec<String> = ids
            .iter()
            .map(super::LockShardId::to_canonical_string)
            .collect();
        let mut expected: Vec<&str> = cases.to_vec();
        expected.sort_unstable();
        assert_eq!(sorted_strings, expected);
    }

    #[test]
    fn shard_levels_new_rejects_depths_above_max() {
        assert!(LockShardLevels::new(LockShardLevels::MAX).is_ok());
        let err = LockShardLevels::new(LockShardLevels::MAX + 1).unwrap_err();
        assert_eq!(err.depth, LockShardLevels::MAX + 1);
        assert_eq!(err.max, LockShardLevels::MAX);
    }
}
