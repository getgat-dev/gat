//! Domain types for Git revision expressions and exact commit identity,
//! independent of `gix`: [`GitRevisionSpec`] is an unresolved, opaque
//! textual revision expression (a branch/tag name, `HEAD~3`, a partial or
//! full commit hash, ...) exactly as the user/config wrote it, while
//! [`GitCommitId`] is an exact, resolved commit identity capable of
//! representing either Git hash algorithm currently in use (SHA-1,
//! SHA-256) without depending on `gix`'s own object-id type.
//!
//! Keeping both types Gix-independent in `gat-core` confines `gix` to
//! `gat-io`'s private Git implementation. That boundary resolves a
//! [`GitRevisionSpec`] and converts between Gix object IDs and
//! [`GitCommitId`] values without exposing physical Git types.

use std::fmt;

/// An unresolved, opaque Git revision expression -- a branch name, tag
/// name, `HEAD~3`, a partial or full commit hash, or any other spelling
/// `git rev-parse`/`gix::Repository::rev_parse_single` accepts.
///
/// Gat never validates Git's revision grammar itself; `gix` remains the
/// sole authoritative parser/resolver through `gat-io`. This type exists
/// only to keep an *unresolved* revision expression from being confused,
/// at the type level, with an *exact, resolved* commit ([`GitCommitId`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitRevisionSpec(String);

impl GitRevisionSpec {
    /// Wraps an owned revision-expression `String`, moving its existing
    /// allocation rather than copying it -- e.g. a CLI `--rev`/
    /// `--exclude-rev` argument, or a `MountConfigInput.rev` value
    /// straight off deserialization.
    #[must_use]
    pub const fn from_string(value: String) -> Self {
        Self(value)
    }

    /// Borrows the unresolved revision text, for handing to `gix`
    /// resolution APIs or rendering in diagnostics/display.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for GitRevisionSpec {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for GitRevisionSpec {
    fn from(value: String) -> Self {
        Self::from_string(value)
    }
}

impl From<&str> for GitRevisionSpec {
    fn from(value: &str) -> Self {
        Self::from_string(value.to_string())
    }
}

impl fmt::Display for GitRevisionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Serializes as the unchanged revision-expression text -- Gat never
/// canonicalizes a revision spec, so round-tripping this value through
/// persisted state (e.g. `MountConfig.rev`) must reproduce exactly what
/// the user/config wrote.
impl serde::Serialize for GitRevisionSpec {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

/// Deserializes via [`GitRevisionSpec::from_string`], retaining the
/// deserialized `String`'s own allocation -- no Git grammar validation is
/// performed here (see the type's own doc comment).
impl<'de> serde::Deserialize<'de> for GitRevisionSpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_string)
    }
}

/// Committer-time-comparable seconds since the Unix epoch, kept
/// Gix-independent at the type level even though its value is produced by
/// parsing through `gix`'s own date parser and compared against `gix`'s
/// own commit-time seconds at the Git I/O boundary (see
/// `gat_io::parse_cli_date` and [`crate::history::TimeWindow`]). Exists
/// so a history selection's time-window filter (`--since`/`--until`)
/// never exposes `gix::date::SecondsSinceUnixEpoch` in a domain/
/// engine-facing request type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitTimestamp(i64);

impl GitTimestamp {
    /// Wraps a raw Unix-epoch second count, e.g. `gix::date::Time::seconds`.
    #[must_use]
    pub const fn from_unix_seconds(seconds: i64) -> Self {
        Self(seconds)
    }

    /// The raw Unix-epoch second count, for handing back to `gix`
    /// comparisons at the Git I/O boundary.
    #[must_use]
    pub const fn unix_seconds(&self) -> i64 {
        self.0
    }
}

impl From<i64> for GitTimestamp {
    fn from(seconds: i64) -> Self {
        Self::from_unix_seconds(seconds)
    }
}

/// Typed failures constructing a [`GitCommitId`] from persisted/serialized
/// hexadecimal text -- distinct from Gat's own content-addressing
/// [`crate::oid::Oid`] errors, and from [`GitRevisionSpec`], which
/// deliberately performs no validation at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitCommitIdError {
    /// `raw`'s length matches neither a SHA-1 (40 hex characters) nor a
    /// SHA-256 (64 hex characters) commit hash.
    #[error(
        "`{raw}` is not a valid git commit id (expected 40 or 64 hexadecimal characters, got {len})"
    )]
    InvalidLength { raw: String, len: usize },
    /// `raw` is the right length for a supported hash algorithm but
    /// contains a non-hexadecimal or non-lowercase character.
    #[error("`{raw}` is not a valid git commit id (expected lowercase hexadecimal digits)")]
    InvalidHexDigit { raw: String },
}

/// Fixed-capacity buffer for rendering a [`GitCommitId`]'s canonical
/// lowercase-hex spelling without a heap allocation -- large enough for
/// the longest supported hash (SHA-256, 64 hex characters).
struct HexBuf {
    buf: [u8; 64],
    len: usize,
}

impl HexBuf {
    const fn new() -> Self {
        Self {
            buf: [0; 64],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        // Only ASCII hex digits are ever written by `GitCommitId::write_hex`.
        std::str::from_utf8(&self.buf[..self.len]).expect("hex output is always ASCII")
    }
}

impl fmt::Write for HexBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let end = self.len + bytes.len();
        if end > self.buf.len() {
            return Err(fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// An exact, resolved Git commit identity -- a fixed-size hash, never a
/// revision *expression* (see [`GitRevisionSpec`] for that). Preserves
/// whichever Git hash algorithm the commit actually uses, without
/// depending on `gix::ObjectId` -- `gat-io` converts between the two at
/// the domain/persistence boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitCommitId {
    /// A 20-byte SHA-1 commit hash (Git's original, still-default object
    /// hash algorithm).
    Sha1([u8; 20]),
    /// A 32-byte SHA-256 commit hash (Git's newer, opt-in object hash
    /// algorithm).
    Sha256([u8; 32]),
}

impl GitCommitId {
    /// The raw hash bytes, in whichever length its algorithm uses.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Sha1(bytes) => bytes.as_slice(),
            Self::Sha256(bytes) => bytes.as_slice(),
        }
    }

    /// Strictly parse a canonical lowercase-hexadecimal commit id (40
    /// characters selects SHA-1, 64 selects SHA-256) directly into the
    /// fixed byte array its length selects -- no intermediate `Vec<u8>`
    /// allocation.
    pub fn parse_hex(raw: &str) -> Result<Self, GitCommitIdError> {
        const fn hex_digit(b: u8) -> Option<u8> {
            match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                _ => None,
            }
        }
        fn decode<const N: usize>(raw: &str) -> Result<[u8; N], GitCommitIdError> {
            let bytes = raw.as_bytes();
            let mut out = [0u8; N];
            for (i, chunk) in bytes.as_chunks::<2>().0.iter().enumerate() {
                let hi = hex_digit(chunk[0]).ok_or_else(|| GitCommitIdError::InvalidHexDigit {
                    raw: raw.to_string(),
                })?;
                let lo = hex_digit(chunk[1]).ok_or_else(|| GitCommitIdError::InvalidHexDigit {
                    raw: raw.to_string(),
                })?;
                out[i] = (hi << 4) | lo;
            }
            Ok(out)
        }

        match raw.len() {
            40 => decode::<20>(raw).map(GitCommitId::Sha1),
            64 => decode::<32>(raw).map(GitCommitId::Sha256),
            len => Err(GitCommitIdError::InvalidLength {
                raw: raw.to_string(),
                len,
            }),
        }
    }

    /// Writes this commit id's canonical lowercase-hex spelling into
    /// `out` directly, with no intermediate `String` allocation.
    fn write_hex(&self, out: &mut impl fmt::Write) -> fmt::Result {
        for byte in self.as_bytes() {
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Display for GitCommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_hex(f)
    }
}

impl std::str::FromStr for GitCommitId {
    type Err = GitCommitIdError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse_hex(raw)
    }
}

/// Serializes as canonical lowercase hexadecimal, written into a
/// fixed-capacity stack buffer (see `HexBuf`) rather than an
/// intermediate heap-allocated `String`.
impl serde::Serialize for GitCommitId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut buf = HexBuf::new();
        self.write_hex(&mut buf)
            .expect("canonical commit id hex always fits in the fixed buffer");
        serializer.serialize_str(buf.as_str())
    }
}

/// Deserializes via [`GitCommitId::parse_hex`] -- rejects anything that
/// isn't already exactly a canonical lowercase-hex SHA-1/SHA-256 commit
/// id.
impl<'de> serde::Deserialize<'de> for GitCommitId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse_hex(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_spec_round_trips_unchanged_text() {
        let spec = GitRevisionSpec::from("HEAD~3");
        assert_eq!(spec.as_str(), "HEAD~3");
        assert_eq!(spec.to_string(), "HEAD~3");
    }

    #[test]
    fn revision_spec_serde_round_trips() {
        let spec = GitRevisionSpec::from("origin/main");
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(json, "\"origin/main\"");
        let back: GitRevisionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn commit_id_parses_sha1_hex() {
        let hex = "a".repeat(40);
        let id = GitCommitId::parse_hex(&hex).unwrap();
        assert_eq!(id, GitCommitId::Sha1([0xaa; 20]));
        assert_eq!(id.to_string(), hex);
    }

    #[test]
    fn commit_id_parses_sha256_hex() {
        let hex = "b".repeat(64);
        let id = GitCommitId::parse_hex(&hex).unwrap();
        assert_eq!(id, GitCommitId::Sha256([0xbb; 32]));
        assert_eq!(id.to_string(), hex);
    }

    #[test]
    fn commit_id_rejects_wrong_length() {
        let err = GitCommitId::parse_hex("abcd").unwrap_err();
        assert!(matches!(
            err,
            GitCommitIdError::InvalidLength { len: 4, .. }
        ));
    }

    #[test]
    fn commit_id_rejects_uppercase_and_non_hex() {
        let uppercase = "A".repeat(40);
        assert!(matches!(
            GitCommitId::parse_hex(&uppercase),
            Err(GitCommitIdError::InvalidHexDigit { .. })
        ));
        let non_hex = format!("{}zz", "0".repeat(38));
        assert!(matches!(
            GitCommitId::parse_hex(&non_hex),
            Err(GitCommitIdError::InvalidHexDigit { .. })
        ));
    }

    #[test]
    fn commit_id_serde_round_trips() {
        let hex = "c".repeat(40);
        let id = GitCommitId::parse_hex(&hex).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{hex}\""));
        let back: GitCommitId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
