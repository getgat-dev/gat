//! Fan-out layout: the content-addressed key space shared by the local
//! cache and every remote backend, built on top of the pure [`Oid`] value
//! type ([`gat_core::oid`]). Independently reusable by `gc`-scale
//! code that needs a compact, ordered oid type without pulling in cache
//! hashing/ingestion or remote concerns.
//!
//! Two related but distinct notions live in this module:
//!
//! - a complete **storage object key** ([`object_key_oid`]): an OID nested
//!   under the two-level `xx/yy` directory split and the current hash
//!   algorithm's namespace
//!   (`blake3/xx/yy/oid`), which is what actually addresses an object on
//!   disk (under `objects_dir`) or on a remote. [`parse_object_key`] is
//!   the inverse: it accepts only a complete, well-formed object key and
//!   recovers the [`Oid`] it names.
//!
//! The pure oid type -- parsing, formatting, serde, ordering, and hashing
//! semantics -- lives in [`gat_core::oid`] and has no dependency on
//! this module or on filesystem state, cache layout, remote storage,
//! rusqlite, or `OpenDAL`; this module is where that value type is given
//! a storage/I/O-specific meaning (a fan-out directory layout and a
//! complete object key).

pub use gat_core::oid::Oid;

/// The current object-hash algorithm's storage namespace: every finalized
/// object -- local or remote -- lives under this directory name, one
/// level above its `xx/yy/oid` fan-out path (see [`object_key_oid`]). `gat`
/// supports exactly one hash algorithm at a time, so this is a plain
/// constant rather than something derived per-object; the namespace
/// keeps algorithm identity separate from `Oid`.
pub const OBJECT_HASH_NAMESPACE: &str = "blake3";

/// The canonical, complete storage key for `oid`: `blake3/xx/yy/oid`.
/// This is what addresses a finalized object,
/// whether under a local `objects_dir` (joined directly, see
/// `cache_path_oid`) or as a remote object key.
/// Nesting fan-out under [`OBJECT_HASH_NAMESPACE`] keeps the hash
/// algorithm's object namespace a sibling of other `objects_dir`/remote
/// root state (`cache.sqlite3`, `tmp-*`) rather than mixed
/// directly into it.
///
/// Builds the canonical
/// `blake3/xx/yy/oid` storage key directly from an already-validated
/// [`Oid`], in exactly one destination allocation (a single
/// pre-sized `String`, copied from a stack encoding) -- no intermediate
/// `to_string()`/`format!` chain. An `Oid`'s 64-hex-character width is a
/// type-level guarantee, so storage-key construction never reparses text.
#[must_use]
pub fn object_key_oid(oid: &Oid) -> String {
    ObjectKey::new(oid).as_str().to_owned()
}

/// Stack-owned canonical encoding shared by remote strings and local paths.
/// The bytes are private and the constructor only emits ASCII.
pub(super) struct ObjectKey([u8; OBJECT_HASH_NAMESPACE.len() + 7 + 64]);

impl ObjectKey {
    pub(super) fn new(oid: &Oid) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let namespace = OBJECT_HASH_NAMESPACE.len();
        let start = namespace + 7;
        let mut bytes = [b'/'; OBJECT_HASH_NAMESPACE.len() + 7 + 64];
        bytes[..namespace].copy_from_slice(OBJECT_HASH_NAMESPACE.as_bytes());
        for (i, byte) in oid.as_bytes().iter().enumerate() {
            bytes[start + i * 2] = HEX[(byte >> 4) as usize];
            bytes[start + i * 2 + 1] = HEX[(byte & 0xf) as usize];
        }
        bytes.copy_within(start..start + 2, namespace + 1);
        bytes.copy_within(start + 2..start + 4, namespace + 4);
        Self(bytes)
    }

    pub(super) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("object keys contain only ASCII")
    }
}

/// Parses `key` (a path/key found while listing a remote or local
/// `objects_dir`) as a complete storage object key
/// (`blake3/xx/yy/<64-lowercase-hex-oid>`, see [`object_key_oid`]) and
/// returns the [`Oid`] it names, or `None` if `key` isn't a
/// well-formed object key at all -- including bookkeeping that lives
/// alongside the object namespace, like
/// the proof database (`cache.sqlite3`),
/// or ingest scratch files (`tmp-*`), all of which are siblings of
/// [`OBJECT_HASH_NAMESPACE`], not entries under it. Requires the fan-out
/// directory segments to match the oid's own first four hex characters.
#[must_use]
pub fn parse_object_key(key: &str) -> Option<Oid> {
    let rest = key.strip_prefix(OBJECT_HASH_NAMESPACE)?;
    let rest = rest.strip_prefix('/')?;
    let mut parts = rest.split('/');
    let (Some(l1), Some(l2), Some(oid_hex), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    ObjectFanout::from_segments(l1, l2)?.parse_leaf(oid_hex)
}

/// Parsed directory identity, reusable for every leaf in a fan-out directory.
#[derive(Clone, Copy)]
pub(crate) struct ObjectFanout([u8; 4]);

pub(crate) fn parse_fanout_segment(segment: &str) -> Option<u8> {
    const fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    let [high, low] = *segment.as_bytes() else {
        return None;
    };
    Some(digit(high)? * 16 + digit(low)?)
}

impl ObjectFanout {
    pub(crate) const fn new(first: u8, second: u8) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        Self([
            HEX[(first >> 4) as usize],
            HEX[(first & 0xf) as usize],
            HEX[(second >> 4) as usize],
            HEX[(second & 0xf) as usize],
        ])
    }

    pub(crate) fn from_segments(first: &str, second: &str) -> Option<Self> {
        Some(Self::new(
            parse_fanout_segment(first)?,
            parse_fanout_segment(second)?,
        ))
    }

    pub(crate) fn parse_leaf(self, name: &str) -> Option<Oid> {
        // Reject unrelated names before parsing an OID (whose diagnostic owns
        // invalid input). Inventory only needs an optional identity.
        if name.len() != 64 || !name.as_bytes().starts_with(&self.0) {
            return None;
        }
        Oid::from_hex(name).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_encoding_and_local_paths_agree_for_every_byte() {
        for byte in 0..=u8::MAX {
            let oid = Oid::from_bytes([byte; 32]);
            let hex = oid.to_hex();
            let expected = format!("blake3/{}/{}/{hex}", &hex[..2], &hex[2..4]);
            assert_eq!(object_key_oid(&oid), expected);
            assert_eq!(parse_object_key(&expected), Some(oid));
            for root in ["", ".", "/", "relative", "relative/", "space name/é"] {
                let root = std::path::Path::new(root);
                assert_eq!(
                    super::super::object::cache_path_oid(root, &oid),
                    root.join(&expected)
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_object_paths_preserve_non_utf8_roots() {
        use std::os::unix::ffi::OsStringExt;
        let root = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"cache/\xff".to_vec()));
        let oid = Oid::from_bytes([0xab; 32]);
        assert_eq!(
            super::super::object::cache_path_oid(&root, &oid),
            root.join(object_key_oid(&oid))
        );
    }

    #[test]
    fn fanout_requires_canonical_segments_and_matching_leaves() {
        let oid = Oid::from_bytes([0xab; 32]);
        for segment in ["AB", "aB", "Ab", "a", "abc", "é", "gg", ""] {
            assert!(ObjectFanout::from_segments(segment, "ab").is_none());
            assert!(ObjectFanout::from_segments("ab", segment).is_none());
        }
        let fanout = ObjectFanout::from_segments("ab", "ab").unwrap();
        assert_eq!(fanout.parse_leaf(&oid.to_hex()), Some(oid));
        assert!(fanout.parse_leaf(&oid.to_hex().to_uppercase()).is_none());
        assert!(
            ObjectFanout::new(0xab, 0xac)
                .parse_leaf(&oid.to_hex())
                .is_none()
        );
    }

    #[test]
    fn object_key_nests_fan_out_under_the_hash_namespace() {
        let oid = "0123456789abcdef".repeat(4);
        let oid = Oid::from_hex(&oid).unwrap();
        assert_eq!(
            object_key_oid(&oid),
            format!("blake3/01/23/{}", oid.to_hex())
        );
        assert_eq!(OBJECT_HASH_NAMESPACE, "blake3");
    }

    #[test]
    fn parse_object_key_accepts_a_well_formed_blake3_key() {
        let oid_hex = "0123456789abcdef".repeat(4);
        let oid = Oid::from_hex(&oid_hex).unwrap();
        let key = object_key_oid(&oid);
        let parsed = parse_object_key(&key).expect("well-formed key must parse");
        assert_eq!(parsed.to_hex(), oid_hex);
    }

    #[test]
    fn parse_object_key_rejects_a_trailing_slash() {
        let oid_hex = "0123456789abcdef".repeat(4);
        let oid = Oid::from_hex(&oid_hex).unwrap();
        let key = object_key_oid(&oid);
        assert!(
            parse_object_key(&format!("{key}/")).is_none(),
            "a trailing slash must not be normalized away -- only the \
             exact blake3/<aa>/<bb>/<oid> shape is a valid object key"
        );
    }

    #[test]
    fn parse_object_key_rejects_non_object_keys() {
        assert!(parse_object_key("not/a/fan/out/key").is_none());
        assert!(parse_object_key("blake3/01/23").is_none());
        // Directory segments don't match the oid's own prefix.
        let bad_prefix_oid = "ff".to_string() + &"0".repeat(62);
        assert!(parse_object_key(&format!("blake3/01/23/{bad_prefix_oid}")).is_none());
        // Missing the blake3/ namespace prefix entirely.
        let oid_hex = "0123456789abcdef".repeat(4);
        assert!(parse_object_key(&format!("01/23/{oid_hex}")).is_none());
        // A root-level/unrelated key.
        assert!(parse_object_key("cache.sqlite3").is_none());
        assert!(parse_object_key("tmp-abc123").is_none());
    }

    // Regression tests for GAT-ARCH-06: OID/fan-out key parsing must never
    // panic on malformed UTF-8, even when a multibyte character straddles
    // (or sits right at) the fixed-width boundary.

    #[test]
    fn parse_object_key_rejects_multibyte_unicode_without_panicking() {
        // Multibyte char right at the start of the oid segment, straddling
        // the byte-offset-2 boundary the old code sliced on.
        assert!(parse_object_key("blake3/01/23/é123456789abcdef").is_none());
        // Multibyte char straddling the byte-offset-4 boundary.
        let mut oid = "01".to_string();
        oid.push('é');
        oid.push_str("23456789abcdef");
        assert!(parse_object_key(&format!("blake3/01/23/{oid}")).is_none());
        // Multibyte chars as the directory segments themselves.
        assert!(parse_object_key("blake3/é1/23/0123456789abcdef").is_none());
    }

    #[test]
    fn parse_object_key_rejects_too_short_and_malformed_segments() {
        assert!(parse_object_key("").is_none());
        assert!(parse_object_key("blake3/01/23/").is_none());
        assert!(parse_object_key("blake3/01/23/1").is_none());
        assert!(parse_object_key("blake3/01/23/123").is_none());
        assert!(parse_object_key("blake3/0/23/0123456789abcdef").is_none());
        assert!(parse_object_key("blake3/01/2/0123456789abcdef").is_none());
        assert!(parse_object_key("blake3/gg/23/gg23456789abcdef").is_none());
        // Right shape, but not under the blake3/ namespace at all.
        let oid_hex = "0123456789abcdef".repeat(4);
        assert!(parse_object_key(&format!("01/23/{oid_hex}")).is_none());
    }
}
