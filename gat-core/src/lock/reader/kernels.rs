//! Bounded lowercase-hex decoding and first-LF scanning.
//!
//! The flag reports controls/backslashes before LF, except a single framing CR
//! immediately before LF. It does not certify UTF-8 or canonical path segments.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(test)]
mod tests;
#[cfg(target_arch = "x86_64")]
mod x86;

use super::ValidatedLockFile;
use crate::lock::Result;
use std::borrow::Cow;

type DecodedRow = Option<([u8; 32], usize, bool)>;

/// Feature-checked capability. Only this module can construct a backend.
#[derive(Clone, Copy)]
pub(super) struct Kernel(Backend);

#[derive(Clone, Copy)]
enum Backend {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Sse2,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    #[cfg(target_arch = "aarch64")]
    Neon,
}

impl Kernel {
    pub(super) fn selected() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            if std::is_x86_feature_detected!("avx2") {
                return Self(Backend::Avx2);
            }
            if std::is_x86_feature_detected!("sse2") {
                return Self(Backend::Sse2);
            }
        }
        #[cfg(target_arch = "aarch64")]
        if std::arch::is_aarch64_feature_detected!("neon") {
            return Self(Backend::Neon);
        }
        Self(Backend::Scalar)
    }

    pub(super) fn decode(self, source: Cow<'_, str>) -> Result<ValidatedLockFile<'_>> {
        // SAFETY: only feature detection constructs SIMD capabilities. Dispatch
        // occurs once per document, outside the monomorphized row loop.
        match self.0 {
            Backend::Scalar => ValidatedLockFile::decode_with(source, scalar),
            #[cfg(target_arch = "x86_64")]
            Backend::Sse2 => unsafe { x86::decode_sse2(source) },
            #[cfg(target_arch = "x86_64")]
            Backend::Avx2 => unsafe { x86::decode_avx2(source) },
            #[cfg(target_arch = "aarch64")]
            Backend::Neon => unsafe { aarch64::decode_neon(source) },
        }
    }

    #[cfg(test)]
    pub(super) const fn scalar() -> Self {
        Self(Backend::Scalar)
    }

    #[cfg(test)]
    pub(super) fn row(self, hash: &[u8; 64], path: &[u8]) -> DecodedRow {
        // SAFETY: all test capabilities use the same feature checks as production.
        match self.0 {
            Backend::Scalar => scalar(hash, path),
            #[cfg(target_arch = "x86_64")]
            Backend::Sse2 => unsafe { x86::row_sse2(hash, path) },
            #[cfg(target_arch = "x86_64")]
            Backend::Avx2 => unsafe { x86::row_avx2(hash, path) },
            #[cfg(target_arch = "aarch64")]
            Backend::Neon => unsafe { aarch64::row_neon(hash, path) },
        }
    }
}

pub(super) const fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Independent scalar oracle: decode pairs, then inspect exactly the current row.
fn scalar(hash: &[u8; 64], path: &[u8]) -> DecodedRow {
    let mut oid = [0; 32];
    for (byte, pair) in oid.iter_mut().zip(hash.chunks_exact(2)) {
        *byte = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    let end = path.iter().position(|&b| b == b'\n')?;
    let field = path[..end].strip_suffix(b"\r").unwrap_or(&path[..end]);
    Some((oid, end, field.iter().any(|&b| b < 32 || b == b'\\')))
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[inline]
fn finish(
    oid: [u8; 32],
    path: &[u8],
    end: usize,
    first_special: Option<usize>,
) -> ([u8; 32], usize, bool) {
    let has_special = first_special.is_some_and(|i| i + 1 != end || path[i] != b'\r');
    (oid, end, has_special)
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[inline]
fn tail(
    oid: [u8; 32],
    path: &[u8],
    mut pos: usize,
    mut first_special: Option<usize>,
) -> DecodedRow {
    while pos < path.len() {
        let byte = path[pos];
        if byte == b'\n' {
            return Some(finish(oid, path, pos, first_special));
        }
        if first_special.is_none() && (byte < 32 || byte == b'\\') {
            first_special = Some(pos);
        }
        pos += 1;
    }
    None
}
