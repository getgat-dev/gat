use super::{DecodedRow, finish, tail};
use crate::lock::{Result, reader::ValidatedLockFile};
use std::arch::x86_64::{
    __m128i, __m256i, _mm_and_si128, _mm_cmpeq_epi8, _mm_cmpgt_epi8, _mm_loadu_si128, _mm_min_epu8,
    _mm_movemask_epi8, _mm_or_si128, _mm_packus_epi16, _mm_set1_epi8, _mm_set1_epi16,
    _mm_slli_epi16, _mm_srli_epi16, _mm_storeu_si128, _mm_sub_epi8, _mm256_add_epi8,
    _mm256_and_si256, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_maddubs_epi16, _mm256_min_epu8,
    _mm256_movemask_epi8, _mm256_or_si256, _mm256_packus_epi16, _mm256_permute4x64_epi64,
    _mm256_set1_epi8, _mm256_set1_epi16, _mm256_storeu_si256, _mm256_sub_epi8,
};
use std::borrow::Cow;

#[target_feature(enable = "avx2")]
pub(super) unsafe fn decode_avx2(source: Cow<'_, str>) -> Result<ValidatedLockFile<'_>> {
    // SAFETY: the feature-checked caller enters the entire parser with AVX2 enabled.
    ValidatedLockFile::decode_with(source, |hash, path| unsafe { row_avx2(hash, path) })
}

#[target_feature(enable = "sse2")]
pub(super) unsafe fn decode_sse2(source: Cow<'_, str>) -> Result<ValidatedLockFile<'_>> {
    // SAFETY: SSE2 is enabled throughout this parser invocation.
    ValidatedLockFile::decode_with(source, |hash, path| unsafe { row_sse2(hash, path) })
}

#[inline]
#[target_feature(enable = "avx2")]
fn hex32(x: __m256i) -> (__m256i, __m256i) {
    let d = _mm256_sub_epi8(x, _mm256_set1_epi8(b'0' as i8));
    let a = _mm256_sub_epi8(x, _mm256_set1_epi8(b'a' as i8));
    let digits = _mm256_cmpeq_epi8(_mm256_min_epu8(d, _mm256_set1_epi8(9)), d);
    let letters = _mm256_cmpeq_epi8(_mm256_min_epu8(a, _mm256_set1_epi8(5)), a);
    let nibbles = _mm256_add_epi8(
        _mm256_and_si256(x, _mm256_set1_epi8(15)),
        _mm256_and_si256(letters, _mm256_set1_epi8(9)),
    );
    (nibbles, _mm256_or_si256(digits, letters))
}

#[inline]
#[target_feature(enable = "avx2")]
pub(super) unsafe fn row_avx2(hash: &[u8; 64], path: &[u8]) -> DecodedRow {
    // SAFETY: digest loads span exactly the 64-byte array. Path loads require
    // 32 remaining initialized bytes. The output store spans exactly the OID.
    unsafe {
        let (n0, v0) = hex32(_mm256_loadu_si256(hash.as_ptr().cast()));
        // A fixed prologue overlaps the first path block with digest decoding.
        // Even an immediate LF must wait for validation of the second hash block.
        let first_path = if path.len() >= 32 {
            let x = _mm256_loadu_si256(path.as_ptr().cast());
            let ends =
                _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, _mm256_set1_epi8(10))).cast_unsigned();
            let controls = _mm256_cmpeq_epi8(_mm256_min_epu8(x, _mm256_set1_epi8(31)), x);
            let special = _mm256_movemask_epi8(_mm256_or_si256(
                controls,
                _mm256_cmpeq_epi8(x, _mm256_set1_epi8(92)),
            ))
            .cast_unsigned();
            Some((ends, special))
        } else {
            None
        };
        let (n1, v1) = hex32(_mm256_loadu_si256(hash.as_ptr().add(32).cast()));
        if _mm256_movemask_epi8(_mm256_and_si256(v0, v1)) != -1 {
            return None;
        }
        // Adjacent nibbles become 16*high + low; values cannot saturate.
        let weights = _mm256_set1_epi16(0x0110);
        let packed = _mm256_packus_epi16(
            _mm256_maddubs_epi16(n0, weights),
            _mm256_maddubs_epi16(n1, weights),
        );
        // packus is lane-local: restore groups [0, 2, 1, 3].
        let packed = _mm256_permute4x64_epi64::<0xD8>(packed);
        let mut oid = [0; 32];
        _mm256_storeu_si256(oid.as_mut_ptr().cast(), packed);
        let mut pos = 0;
        let mut first_special = None;
        if let Some((ends, special)) = first_path {
            if ends != 0 {
                let n = ends.trailing_zeros() as usize;
                let special = special & ((1u32 << n) - 1);
                if special != 0 {
                    first_special = Some(special.trailing_zeros() as usize);
                }
                return Some(finish(oid, path, n, first_special));
            }
            if special != 0 {
                first_special = Some(special.trailing_zeros() as usize);
            }
            pos = 32;
        }
        while path.len() - pos >= 32 {
            let x = _mm256_loadu_si256(path.as_ptr().add(pos).cast());
            let ends =
                _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, _mm256_set1_epi8(10))).cast_unsigned();
            let controls = _mm256_cmpeq_epi8(_mm256_min_epu8(x, _mm256_set1_epi8(31)), x);
            let special = _mm256_movemask_epi8(_mm256_or_si256(
                controls,
                _mm256_cmpeq_epi8(x, _mm256_set1_epi8(92)),
            ))
            .cast_unsigned();
            if ends != 0 {
                let n = ends.trailing_zeros() as usize;
                let special = special & ((1u32 << n) - 1);
                if first_special.is_none() && special != 0 {
                    first_special = Some(pos + special.trailing_zeros() as usize);
                }
                return Some(finish(oid, path, pos + n, first_special));
            }
            if first_special.is_none() && special != 0 {
                first_special = Some(pos + special.trailing_zeros() as usize);
            }
            pos += 32;
        }
        // A 16-byte cleanup avoids scalar work for medium-sized final fragments.
        scan_sse2(oid, path, pos, first_special)
    }
}

#[inline]
#[target_feature(enable = "sse2")]
fn hex16(x: __m128i) -> (__m128i, __m128i) {
    let digits = _mm_and_si128(
        _mm_cmpgt_epi8(x, _mm_set1_epi8(47)),
        _mm_cmpgt_epi8(_mm_set1_epi8(58), x),
    );
    let letters = _mm_and_si128(
        _mm_cmpgt_epi8(x, _mm_set1_epi8(96)),
        _mm_cmpgt_epi8(_mm_set1_epi8(103), x),
    );
    let nibbles = _mm_or_si128(
        _mm_and_si128(digits, _mm_sub_epi8(x, _mm_set1_epi8(48))),
        _mm_and_si128(letters, _mm_sub_epi8(x, _mm_set1_epi8(87))),
    );
    // Even bytes hold high nibbles, odd bytes low nibbles. Convert to words
    // before packing, using SSE2 only (no SSSE3 multiply-add dependency).
    let pairs = _mm_or_si128(
        _mm_slli_epi16::<4>(_mm_and_si128(nibbles, _mm_set1_epi16(15))),
        _mm_srli_epi16::<8>(nibbles),
    );
    (pairs, _mm_or_si128(digits, letters))
}

#[inline]
#[target_feature(enable = "sse2")]
pub(super) unsafe fn row_sse2(hash: &[u8; 64], path: &[u8]) -> DecodedRow {
    // SAFETY: four 16-byte loads span hash; two 16-byte stores span oid.
    unsafe {
        let (p0, v0) = hex16(_mm_loadu_si128(hash.as_ptr().cast()));
        let (p1, v1) = hex16(_mm_loadu_si128(hash.as_ptr().add(16).cast()));
        let (p2, v2) = hex16(_mm_loadu_si128(hash.as_ptr().add(32).cast()));
        let (p3, v3) = hex16(_mm_loadu_si128(hash.as_ptr().add(48).cast()));
        let valid = _mm_and_si128(_mm_and_si128(v0, v1), _mm_and_si128(v2, v3));
        if _mm_movemask_epi8(valid) != 65535 {
            return None;
        }
        let mut oid = [0; 32];
        _mm_storeu_si128(oid.as_mut_ptr().cast(), _mm_packus_epi16(p0, p1));
        _mm_storeu_si128(oid.as_mut_ptr().add(16).cast(), _mm_packus_epi16(p2, p3));
        scan_sse2(oid, path, 0, None)
    }
}

#[inline]
#[target_feature(enable = "sse2")]
unsafe fn scan_sse2(
    oid: [u8; 32],
    path: &[u8],
    mut pos: usize,
    mut first_special: Option<usize>,
) -> DecodedRow {
    while path.len() - pos >= 16 {
        // SAFETY: this iteration has 16 initialized bytes within path.
        let x = unsafe { _mm_loadu_si128(path.as_ptr().add(pos).cast()) };
        let ends = _mm_movemask_epi8(_mm_cmpeq_epi8(x, _mm_set1_epi8(10))).cast_unsigned();
        let controls = _mm_cmpeq_epi8(_mm_min_epu8(x, _mm_set1_epi8(31)), x);
        let special =
            _mm_movemask_epi8(_mm_or_si128(controls, _mm_cmpeq_epi8(x, _mm_set1_epi8(92))))
                .cast_unsigned();
        if ends != 0 {
            let n = ends.trailing_zeros() as usize;
            let special = special & ((1u32 << n) - 1);
            if first_special.is_none() && special != 0 {
                first_special = Some(pos + special.trailing_zeros() as usize);
            }
            return Some(finish(oid, path, pos + n, first_special));
        }
        if first_special.is_none() && special != 0 {
            first_special = Some(pos + special.trailing_zeros() as usize);
        }
        pos += 16;
    }
    tail(oid, path, pos, first_special)
}
