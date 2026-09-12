//! Single-pass v1 decoding and complete single-file certification.
mod kernels;

use super::{InvalidOidReason, LockDomainError, MalformedRowReason, Result, VERSION};
use crate::oid::Oid;
use std::borrow::Cow;

#[derive(Debug)]
struct Row {
    start: usize,
    len: usize,
    arena: bool,
    oid: Oid,
}

/// A completely validated, strictly path-ordered lock file.
///
/// Ordinary paths borrow the source. Escaped paths occupy one shared arena.
/// Construction validates every row and all single-file invariants before any
/// consumer can iterate. The view certifies no cross-file placement invariants.
#[derive(Debug)]
pub struct ValidatedLockFile<'a> {
    source: Cow<'a, str>,
    arena: String,
    rows: Vec<Row>,
}

impl<'a> ValidatedLockFile<'a> {
    /// Decode and certify the entire file, retaining borrowed ordinary paths.
    pub fn parse(source: &'a str) -> Result<Self> {
        Self::decode(Cow::Borrowed(source), kernels::Kernel::selected())
    }

    /// Retain owned, coherently acquired text without copying its contents.
    pub fn from_owned(source: String) -> Result<ValidatedLockFile<'static>> {
        ValidatedLockFile::decode(Cow::Owned(source), kernels::Kernel::selected())
    }

    fn decode(source: Cow<'a, str>, kernel: kernels::Kernel) -> Result<Self> {
        kernel.decode(source)
    }

    // Keep the row loop in the feature-enabled caller: ordinary inline hints
    // leave this large body out of line and force a per-row SIMD call boundary.
    #[allow(clippy::inline_always)]
    #[inline(always)]
    fn decode_with(
        source: Cow<'a, str>,
        row: impl Fn(&[u8; 64], &[u8]) -> Option<([u8; 32], usize, bool)>,
    ) -> Result<Self> {
        if source.is_empty() {
            return Err(LockDomainError::Empty.into());
        }
        let Some(header) = source
            .strip_prefix(VERSION)
            .and_then(|s| s.strip_prefix("\r\n").or_else(|| s.strip_prefix('\n')))
        else {
            return Err(LockDomainError::UnsupportedVersion {
                expected: VERSION.to_owned(),
                got: source.lines().next().unwrap_or_default().to_owned(),
            }
            .into());
        };
        let mut pos = source.len() - header.len();
        let mut result = Self {
            source,
            arena: String::new(),
            rows: Vec::new(),
        };
        let mut stack: Vec<usize> = Vec::new();
        while pos < result.source.len() {
            let line = result.rows.len() + 2;
            let bytes = result.source.as_bytes();
            let malformed = |reason| LockDomainError::MalformedRow { line, reason };
            if bytes.get(pos + 64) != Some(&b'\t') {
                return Err(malformed(MalformedRowReason::InvalidSeparator).into());
            }
            let hash: &[u8; 64] = bytes[pos..pos + 64].try_into().expect("fixed hash width");
            let start = pos + 65;
            let Some((oid, len, has_special)) = row(hash, &bytes[start..]) else {
                // Only the cold error path distinguishes digest errors from truncation.
                if hash.iter().any(|&b| kernels::nibble(b).is_none()) {
                    return Err(LockDomainError::InvalidOid {
                        line,
                        path: result.source[start..]
                            .split('\n')
                            .next()
                            .unwrap_or_default()
                            .to_owned(),
                        reason: InvalidOidReason::NotHexBlake3,
                    }
                    .into());
                }
                return Err(malformed(MalformedRowReason::MissingLineFeed).into());
            };
            let field = &result.source[start..start + len];
            let field = field.strip_suffix('\r').unwrap_or(field);
            let arena_start = result.arena.len();
            let arena = has_special && field.contains('\\');
            let path = if arena {
                decode_escaped(field, &mut result.arena).ok_or_else(|| {
                    LockDomainError::NonCanonicalPath {
                        line,
                        path: field.to_owned(),
                    }
                })?;
                &result.arena[arena_start..]
            } else {
                field
            };
            if !arena && has_special {
                return Err(LockDomainError::NonCanonicalPath {
                    line,
                    path: path.to_owned(),
                }
                .into());
            }
            super::codec::validate_path(path, line)?;
            if let Some(previous) = result.rows.last() {
                let previous = result.path(previous);
                match previous.cmp(path) {
                    std::cmp::Ordering::Equal => {
                        return Err(LockDomainError::DuplicatePath {
                            path: path.to_owned(),
                            line: Some(line),
                        }
                        .into());
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(malformed(MalformedRowReason::UnorderedPath {
                            path: path.to_owned(),
                        })
                        .into());
                    }
                    std::cmp::Ordering::Less => {}
                }
                let lcp = previous
                    .bytes()
                    .zip(path.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                while stack.last().is_some_and(|&i| result.rows[i].len > lcp) {
                    stack.pop();
                }
                if let Some(&i) = stack.last()
                    && path.as_bytes().get(result.rows[i].len) == Some(&b'/')
                {
                    return Err(LockDomainError::DirectoryPrefixConflict {
                        ancestor: result.path(&result.rows[i]).to_owned(),
                        descendant: path.to_owned(),
                    }
                    .into());
                }
            }
            let row = Row {
                start: if arena { arena_start } else { start },
                len: path.len(),
                arena,
                oid: Oid::from_bytes(oid),
            };
            stack.push(result.rows.len());
            result.rows.push(row);
            pos = start + len + 1;
        }
        #[cfg(any(test, feature = "test-support"))]
        test_probes::record_file_validation_parse();
        Ok(result)
    }

    fn path(&self, row: &Row) -> &str {
        let text: &str = if row.arena { &self.arena } else { &self.source };
        &text[row.start..row.start + row.len]
    }

    /// Iterate borrowed semantic rows after complete certification.
    #[must_use]
    pub fn rows(&self) -> impl ExactSizeIterator<Item = (&str, Oid)> + '_ {
        self.rows.iter().map(|row| (self.path(row), row.oid))
    }

    /// Borrow a certified row by its zero-based index.
    #[must_use]
    pub fn row(&self, index: usize) -> Option<(&str, Oid)> {
        self.rows.get(index).map(|row| (self.path(row), row.oid))
    }
}

pub(super) fn decode_escaped(field: &str, arena: &mut String) -> Option<()> {
    let bytes = field.as_bytes();
    let mut start = 0;
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos] < 32 {
            return None;
        }
        if bytes[pos] != b'\\' {
            pos += 1;
            continue;
        }
        arena.push_str(&field[start..pos]);
        if bytes.get(pos + 1) != Some(&b'x') {
            return None;
        }
        let value =
            kernels::nibble(*bytes.get(pos + 2)?)? * 16 + kernels::nibble(*bytes.get(pos + 3)?)?;
        if value >= 32 {
            return None;
        }
        arena.push(char::from(value));
        pos += 4;
        start = pos;
    }
    arena.push_str(&field[start..]);
    Some(())
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_probes {
    use std::cell::Cell;
    thread_local! { static FILE_VALIDATIONS: Cell<usize> = const { Cell::new(0) }; }
    pub(super) fn record_file_validation_parse() {
        FILE_VALIDATIONS.with(|n| n.set(n.get() + 1));
    }
    pub fn file_validation_parses() -> usize {
        FILE_VALIDATIONS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexical_path::GatPath;
    use crate::lock::{Entry, Lock};

    fn document(paths: &[&str]) -> String {
        let mut text = format!("{VERSION}\n");
        for path in paths {
            use std::fmt::Write;
            writeln!(text, "{}\t{path}", "a".repeat(64)).unwrap();
        }
        text
    }

    #[test]
    fn canonical_grammar_rejects_alternative_spellings() {
        for path in [
            "", "/a", "a/", "a//b", "a/./b", "a/../b", r"a\t", r"a\x2f", r"a\x1F", r"a\x0",
            r"a\u0000", r"a\\b", "a\tb", "a\rb",
        ] {
            assert!(
                ValidatedLockFile::parse(&document(&[path])).is_err(),
                "{path:?}"
            );
        }
        let valid = document(&["a"]);
        for bytes in [
            valid.trim_end().to_owned(),
            valid.clone() + "\n",
            format!("{VERSION}\n\"a\"\tblake3:{}\n", "a".repeat(64)),
        ] {
            assert!(ValidatedLockFile::parse(&bytes).is_err(), "{bytes:?}");
        }
        for pos in 0..valid.len() {
            if pos != VERSION.len() + 1 {
                assert!(
                    ValidatedLockFile::parse(&valid[..pos]).is_err(),
                    "truncation {pos}"
                );
            }
        }
        assert!(
            ValidatedLockFile::parse(&format!("{VERSION}\n"))
                .unwrap()
                .rows()
                .next()
                .is_none()
        );
    }

    #[test]
    fn lf_crlf_and_mixed_endings_share_semantics_at_every_scan_boundary() {
        for len in 1..100 {
            for path in [
                "x".repeat(len),
                "x".repeat(len) + r"\x0d",
                "x".repeat(len) + r"\x09tail",
            ] {
                let lf = document(&[&path, "z"]);
                let crlf = lf.replace('\n', "\r\n");
                let mixed = lf.replacen('\n', "\r\n", 2);
                let expected = Lock::parse(&lf).unwrap();
                for input in [&crlf, &mixed] {
                    let scalar =
                        ValidatedLockFile::decode(Cow::Borrowed(input), kernels::Kernel::scalar())
                            .unwrap();
                    let selected = ValidatedLockFile::parse(input).unwrap();
                    assert_eq!(
                        scalar.rows().collect::<Vec<_>>(),
                        selected.rows().collect::<Vec<_>>()
                    );
                    assert_eq!(Lock::parse(input).unwrap(), expected);
                    assert_eq!(Lock::parse(input).unwrap().to_string(), lf);
                }
                let bare_cr = document(&[&("x".repeat(len) + "\rinside")]);
                let double_cr = document(&[&("x".repeat(len) + "\r\r")]);
                assert!(ValidatedLockFile::parse(&bare_cr).is_err());
                assert!(ValidatedLockFile::parse(&double_cr).is_err());
            }
        }
        assert!(
            ValidatedLockFile::parse(&format!("{VERSION}\r\n"))
                .unwrap()
                .rows()
                .next()
                .is_none()
        );
        assert!(ValidatedLockFile::parse(&format!("{VERSION}\r")).is_err());
    }

    #[test]
    fn all_controls_and_unicode_roundtrip_through_shared_arena() {
        let mut paths = vec![
            "C:foo".to_owned(),
            " leading and trailing ".to_owned(),
            "a\"quote".to_owned(),
            "日本語/é\u{7f}\u{2028}".to_owned(),
        ];
        paths.extend((0..32).map(|n| format!("control/{}file", char::from(n))));
        let lock = Lock {
            entries: paths
                .iter()
                .map(|p| Entry {
                    path: GatPath::parse_canonical(p).unwrap(),
                    oid: Oid::from_bytes([19; 32]),
                })
                .collect(),
        };
        let text = lock.to_string();
        let view = ValidatedLockFile::parse(&text).unwrap();
        paths.sort_unstable();
        assert_eq!(view.rows().map(|(p, _)| p).collect::<Vec<_>>(), paths);
        for row in &view.rows {
            let path = view.path(row);
            assert_eq!(row.arena, path.bytes().any(|b| b < 32));
            if !row.arena {
                assert_eq!(path.as_ptr(), text[row.start..].as_ptr());
            }
        }
        let owned = ValidatedLockFile::from_owned(text.clone()).unwrap();
        assert_eq!(
            owned.rows().collect::<Vec<_>>(),
            view.rows().collect::<Vec<_>>()
        );
    }

    #[test]
    fn ordered_prefix_stack_agrees_with_pairwise_reference() {
        let names = [
            "a",
            "a.b",
            "a/b",
            "a/b/c",
            "a/b.c",
            "a0",
            "b",
            "b/c",
            "é",
            "日本語",
            "C:foo",
            "a\0",
            "a\t",
            "a\"",
            " space ",
        ];
        for a in names {
            for b in names {
                for c in names {
                    let mut paths = [a, b, c];
                    paths.sort_unstable();
                    let conflict = (0..3).any(|i| {
                        ((i + 1)..3).any(|j| {
                            paths[i] == paths[j]
                                || super::super::codec::is_directory_prefix(paths[i], paths[j])
                        })
                    });
                    let lock = Lock {
                        entries: paths
                            .iter()
                            .map(|p| Entry {
                                path: GatPath::parse_canonical(p).unwrap(),
                                oid: Oid::from_bytes([0; 32]),
                            })
                            .collect(),
                    };
                    assert_eq!(
                        ValidatedLockFile::parse(&lock.to_string()).is_err(),
                        conflict,
                        "{paths:?}"
                    );
                }
            }
        }
        assert!(ValidatedLockFile::parse(&document(&["z", "a"])).is_err());
    }

    #[test]
    fn scalar_and_simd_match_for_every_digest_byte_and_path_tail() {
        for len in 1..100 {
            let text = document(&[&"x".repeat(len)]);
            let scalar =
                ValidatedLockFile::decode(Cow::Borrowed(&text), kernels::Kernel::scalar()).unwrap();
            let selected = ValidatedLockFile::parse(&text).unwrap();
            assert_eq!(
                scalar.rows().collect::<Vec<_>>(),
                selected.rows().collect::<Vec<_>>()
            );
        }
        let mut hash = [b'0'; 64];
        for pos in 0..64 {
            for byte in 0..=255u8 {
                hash[pos] = byte;
                let scalar = kernels::Kernel::scalar().row(&hash, b"path\n");
                let selected = kernels::Kernel::selected().row(&hash, b"path\n");
                assert_eq!(scalar, selected, "position {pos}, byte {byte}");
                assert_eq!(scalar.is_some(), kernels::nibble(byte).is_some());
                if let Some((oid, _, _)) = scalar {
                    let expected = Oid::from_hex(std::str::from_utf8(&hash).unwrap()).unwrap();
                    assert_eq!(&oid, expected.as_bytes());
                }
                hash[pos] = b'0';
            }
        }
    }
}
