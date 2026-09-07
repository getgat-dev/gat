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
/// pre-sized `String`, hex-encoded in place) -- no intermediate
/// `to_string()`/`format!` chain. An `Oid`'s 64-hex-character width is a
/// type-level guarantee, so storage-key construction never reparses text.
#[must_use]
pub fn object_key_oid(oid: &Oid) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    // "blake3/" + "xx/" + "yy/" + 64 hex chars.
    let mut out = String::with_capacity(OBJECT_HASH_NAMESPACE.len() + 1 + 3 + 3 + 64);
    out.push_str(OBJECT_HASH_NAMESPACE);
    out.push('/');
    let bytes = oid.as_bytes();
    fn push_hex_byte(out: &mut String, b: u8) {
        out.push(HEX_DIGITS[(b >> 4) as usize] as char);
        out.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
    }
    push_hex_byte(&mut out, bytes[0]);
    out.push('/');
    push_hex_byte(&mut out, bytes[1]);
    out.push('/');
    for &b in bytes {
        push_hex_byte(&mut out, b);
    }
    out
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
    let is_hex2 = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hex2(l1) || !is_hex2(l2) {
        return None;
    }
    let oid_bytes = oid_hex.as_bytes();
    if oid_bytes.len() < 4 || !oid_bytes[..4].is_ascii() {
        return None;
    }
    if &oid_hex[0..2] != l1 || &oid_hex[2..4] != l2 {
        return None;
    }
    Oid::from_hex(oid_hex).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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
