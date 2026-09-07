//! The pure content-oid value type: canonical hex parsing/formatting,
//! serde, ordering, and hashing semantics, with zero dependency on
//! filesystem state, cache layout, remote storage, rusqlite, or `OpenDAL`.
//! Fan-out/object-key layout (how an [`Oid`] maps onto a directory
//! structure or storage key) is a storage/I/O concern and lives in
//! `gat-io` instead.

/// Malformed content-ID text: not exactly 64 lowercase hexadecimal bytes.
/// This is a value-format error, independent of storage or filesystem state.
#[derive(Debug, thiserror::Error)]
pub enum OidFormatError {
    #[error("invalid oid `{hex}`: expected 64 hex characters, got {actual}")]
    WrongHexLength { hex: String, actual: usize },
    #[error("invalid oid `{hex}`: expected 64 lowercase hex characters")]
    NotLowercaseHex { hex: String },
}

/// A BLAKE3 content ID stored as 32 raw bytes. It is `Copy` and compact
/// enough for object sets used by garbage collection and transfers.
/// Ordering agrees with canonical lowercase hexadecimal ordering; hashing
/// uses the raw bytes, not their textual encoding.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Oid([u8; 32]);

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl Oid {
    /// Parses a lowercase hex oid (as stored in `gat.lock`/found in a
    /// fan-out key) into its raw bytes. Rejects
    /// anything that isn't exactly 64 lowercase hex characters -- no
    /// leading/trailing whitespace is trimmed -- since every real blake3
    /// oid is exactly that canonical form; uppercase (or mixed-case) hex
    /// is rejected rather than silently accepted, so there's exactly one
    /// valid textual representation per oid. This is the strict,
    /// canonical parse for persisted/internal text (a `gat.lock` row, a
    /// fan-out key, a serialized [`Oid`]); a caller reading a genuinely
    /// lenient external boundary must trim its own input first.
    ///
    /// Works entirely on the raw bytes (never on `str` byte-range
    /// slices), so arbitrary Unicode input - including multibyte
    /// characters landing on or around the 64-byte boundary - is rejected
    /// with an error instead of panicking.
    pub fn from_hex(hex: &str) -> Result<Self, OidFormatError> {
        let bytes = hex.as_bytes();
        if bytes.len() != 64 {
            return Err(OidFormatError::WrongHexLength {
                hex: hex.to_string(),
                actual: bytes.len(),
            });
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            let (Some(hi), Some(lo)) = (hex_value(bytes[i * 2]), hex_value(bytes[i * 2 + 1]))
            else {
                return Err(OidFormatError::NotLowercaseHex {
                    hex: hex.to_string(),
                });
            };
            *byte = (hi << 4) | lo;
        }
        Ok(Self(out))
    }

    /// Wraps an already-validated raw 32-byte digest (e.g. a `BLOB` column
    /// read back from the materialized-state `SQLite` store) without paying
    /// for a hex round-trip. Callers reading untrusted bytes (a database
    /// row, a file) must still validate the length themselves before
    /// calling this -- unlike [`Self::from_hex`], this never fails, since
    /// `[u8; 32]` already proves the length is right.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw 32-byte digest, for storage representations (e.g. a
    /// `BLOB` column) that are cheaper and more compact than the 64-byte
    /// hex form `gat.lock` uses.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compares against a hex-encoded oid without
    /// allocating a `String` on either side -- cheaper than
    /// `self.to_hex() == hex` for the common case of a comparison that
    /// only needs a yes/no answer (e.g. deciding whether a row changed),
    /// not the rendered text itself. Malformed/wrong-length hex compares
    /// unequal rather than erroring, matching a plain `==` comparison's
    /// behavior for already-untrusted input.
    #[must_use]
    pub fn eq_hex(&self, hex: &str) -> bool {
        let hex = hex.as_bytes();
        if hex.len() != 64 {
            return false;
        }
        for (i, &byte) in self.0.iter().enumerate() {
            let (Some(hi), Some(lo)) = (hex_value(hex[i * 2]), hex_value(hex[i * 2 + 1])) else {
                return false;
            };
            if byte != (hi << 4) | lo {
                return false;
            }
        }
        true
    }

    /// Shared allocation-free encoding for owned strings, formatting, and serde.
    fn encode_hex(&self, buf: &mut [u8; 64]) {
        const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
        for (i, byte) in self.0.iter().enumerate() {
            buf[i * 2] = HEX_DIGITS[(byte >> 4) as usize];
            buf[i * 2 + 1] = HEX_DIGITS[(byte & 0x0f) as usize];
        }
    }

    /// Renders back to the same lowercase hex string `gat.lock`/fan-out
    /// keys use. Encodes into one preallocated 64-byte buffer
    /// via a fixed nibble lookup table rather than one `format!` call per
    /// byte. Callers that only need formatting can use [`std::fmt::Display`]
    /// to avoid allocating an owned string.
    ///
    /// Only genuinely needed when an *owned* hex `String` is required
    /// (e.g. constructing a diagnostic/error payload); a caller that only
    /// needs to format/serialize the oid as text should go through
    /// [`Display for Oid`](std::fmt::Display) or `Serialize for Oid`
    /// instead, both of which encode straight into a stack buffer with no
    /// heap allocation at all.
    #[must_use]
    #[allow(
        clippy::missing_panics_doc,
        reason = "Hex encoding only emits ASCII bytes"
    )]
    pub fn to_hex(self) -> String {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_to_hex_call();
        let mut out = [0u8; 64];
        self.encode_hex(&mut out);
        String::from_utf8(out.to_vec()).expect("hex digits are always valid UTF-8")
    }
}

/// Test-only instrumentation for asserting how many times [`Oid::to_hex`]
/// allocates an owned hexadecimal string, so a
/// structural test can prove a stat-proven sync match or a `gat diff`
/// comparison never hex-encodes a native oid it doesn't need to.
///
/// Gated on the `test-support` feature (in addition to `cfg(test)`) so the
/// root `gat` crate's own tests -- which cannot see another crate's
/// `#[cfg(test)]`-only items directly -- can still exercise this
/// instrumentation by depending on `gat-core` with `features =
/// ["test-support"]` in `[dev-dependencies]`; Cargo's feature unification
/// then activates it for those test builds. Production builds omit it
/// unless `test-support` is explicitly enabled.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static TO_HEX_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn record_to_hex_call() {
        TO_HEX_CALLS.with(|c| c.set(c.get() + 1));
    }

    /// Resets the counter and returns the count observed since the
    /// previous reset.
    #[allow(
        clippy::must_use_candidate,
        reason = "Reading also resets the test counter"
    )]
    pub fn take_to_hex_calls() -> usize {
        TO_HEX_CALLS.with(|c| c.replace(0))
    }
}

impl std::fmt::Display for Oid {
    /// Encodes straight into a stack buffer and writes the resulting
    /// `&str` -- unlike [`Self::to_hex`], this never allocates an owned
    /// `String` merely to hand it to the `Formatter`, since every hot
    /// serialization path (`gat.lock` row writing, in particular) formats
    /// an `Oid` this way once per row.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut buf = [0u8; 64];
        self.encode_hex(&mut buf);
        f.write_str(std::str::from_utf8(&buf).expect("hex digits are always valid UTF-8"))
    }
}

/// Serializes as the same canonical, lowercase 64-character hex text
/// [`Oid::to_hex`] produces.
impl serde::Serialize for Oid {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut buf = [0u8; 64];
        self.encode_hex(&mut buf);
        serializer
            .serialize_str(std::str::from_utf8(&buf).expect("hex digits are always valid UTF-8"))
    }
}

/// Deserializes via strict [`Oid::from_hex`], so malformed persisted text
/// (wrong length, non-hex, uppercase) fails deserialization instead of
/// being silently accepted or requiring a later unwrap/expect at the call
/// site.
impl<'de> serde::Deserialize<'de> for Oid {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::from_hex(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_hex_round_trips() {
        let hex = "0123456789abcdef".repeat(4);
        let oid = Oid::from_hex(&hex).unwrap();
        assert_eq!(oid.to_hex(), hex);
        assert_eq!(oid.to_string(), hex);
    }

    #[test]
    fn oid_from_hex_rejects_the_wrong_length() {
        assert!(Oid::from_hex("00").is_err());
        assert!(Oid::from_hex(&"0".repeat(63)).is_err());
        assert!(Oid::from_hex(&"0".repeat(65)).is_err());
    }

    #[test]
    fn oid_ordering_matches_hex_string_ordering() {
        let a = Oid::from_hex(&"00".repeat(32)).unwrap();
        let b = Oid::from_hex(&format!("01{}", "00".repeat(31))).unwrap();
        let ff = Oid::from_hex(&"ff".repeat(32)).unwrap();
        assert!(a < b);
        assert!(b < ff);
        let mut hexes = vec![ff.to_hex(), a.to_hex(), b.to_hex()];
        hexes.sort();
        let mut oids = [ff, a, b];
        oids.sort();
        assert_eq!(hexes, oids.iter().map(|o| o.to_hex()).collect::<Vec<_>>());
    }

    #[test]
    fn oid_from_hex_rejects_multibyte_unicode_without_panicking() {
        // 61 ASCII bytes + a 2-byte 'é' + 1 ASCII byte = 64 bytes total,
        // but only 63 chars, with non-hexadecimal content near the expected
        // length boundary.
        let mut hex = "0".repeat(61);
        hex.push('é');
        hex.push('0');
        assert_eq!(hex.len(), 64);
        assert!(Oid::from_hex(&hex).is_err());

        // A multibyte char right at the very start of the string.
        let mut hex2 = String::from("é");
        hex2.push_str(&"0".repeat(62));
        assert_eq!(hex2.len(), 64);
        assert!(Oid::from_hex(&hex2).is_err());

        // A multibyte char right at the very end of the string.
        let mut hex3 = "0".repeat(62);
        hex3.push('é');
        assert_eq!(hex3.len(), 64);
        assert!(Oid::from_hex(&hex3).is_err());
    }

    #[test]
    fn oid_from_hex_rejects_non_hex_and_uppercase() {
        // Non-hex ASCII characters.
        assert!(Oid::from_hex(&"g".repeat(64)).is_err());
        assert!(Oid::from_hex(&"z".repeat(64)).is_err());
        // Uppercase hex is not the canonical form and is rejected.
        assert!(Oid::from_hex(&"ABCDEF01".repeat(8)).is_err());
        assert!(Oid::from_hex(&format!("A{}", "0".repeat(63))).is_err());
    }

    #[test]
    fn eq_hex_matches_the_same_oid_encoded_as_lowercase_hex() {
        let hex = "0123456789abcdef".repeat(4);
        let oid = Oid::from_hex(&hex).unwrap();
        assert!(oid.eq_hex(&hex));
        assert_eq!(oid.to_hex(), hex);
    }

    #[test]
    fn eq_hex_rejects_a_different_oid() {
        let a = Oid::from_hex(&"0123456789abcdef".repeat(4)).unwrap();
        let b_hex = "fedcba9876543210".repeat(4);
        assert!(!a.eq_hex(&b_hex));
    }

    #[test]
    fn eq_hex_rejects_wrong_length_hex_instead_of_panicking() {
        let oid = Oid::from_hex(&"00".repeat(32)).unwrap();
        assert!(!oid.eq_hex(""));
        assert!(!oid.eq_hex("00"));
        assert!(!oid.eq_hex(&"00".repeat(31)));
        assert!(!oid.eq_hex(&"00".repeat(33)));
    }

    #[test]
    fn eq_hex_rejects_malformed_hex_characters() {
        let oid = Oid::from_hex(&"00".repeat(32)).unwrap();
        let mut malformed = "00".repeat(31);
        malformed.push_str("gg");
        assert!(!oid.eq_hex(&malformed));

        let mut multibyte = "00".repeat(31);
        multibyte.push('é');
        assert!(!oid.eq_hex(&multibyte));
    }
}
