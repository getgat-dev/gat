//! Fixed-64 lowercase hex decoding interleaved with bounded path scanning.

#[derive(Clone, Copy)]
pub(super) enum Kernel {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Sse2,
}

impl Kernel {
    pub(super) fn selected() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            if std::is_x86_feature_detected!("sse2") {
                Self::Sse2
            } else {
                Self::Scalar
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Self::Scalar
        }
    }

    pub(super) fn row(self, hash: &[u8; 64], path: &[u8]) -> Option<([u8; 32], usize, bool)> {
        match self {
            Self::Scalar => scan::<false>(hash, path),
            #[cfg(target_arch = "x86_64")]
            Self::Sse2 => scan::<true>(hash, path),
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

fn scan<const SIMD: bool>(hash: &[u8; 64], path: &[u8]) -> Option<([u8; 32], usize, bool)> {
    let mut values = [0; 64];
    let mut block = 0;
    let mut pos = 0;
    let mut first_special = None;
    loop {
        if block < 4 {
            let start = block * 16;
            let input = &hash[start..start + 16];
            let output = &mut values[start..start + 16];
            #[cfg(target_arch = "x86_64")]
            if SIMD {
                // SAFETY: x86-64 guarantees SSE2; both slices contain 16 bytes.
                unsafe {
                    hex16(input, output)?;
                }
            } else {
                for (dst, &byte) in output.iter_mut().zip(input) {
                    *dst = nibble(byte)?;
                }
            }
            #[cfg(not(target_arch = "x86_64"))]
            for (dst, &byte) in output.iter_mut().zip(input) {
                *dst = nibble(byte)?;
            }
            block += 1;
        }
        if pos == path.len() {
            return None;
        }
        #[cfg(target_arch = "x86_64")]
        if SIMD && path.len() - pos >= 16 {
            // SAFETY: the input slice contains 16 initialized bytes, within the allocation.
            let (ends, special) = unsafe { path16(&path[pos..pos + 16]) };
            if ends != 0 {
                let n = ends.trailing_zeros() as usize;
                let special = special & ((1u32 << n) - 1);
                if special != 0 && first_special.is_none() {
                    first_special = Some(pos + special.trailing_zeros() as usize);
                }
                pos += n;
                break;
            }
            if special != 0 && first_special.is_none() {
                first_special = Some(pos + special.trailing_zeros() as usize);
            }
            pos += 16;
            continue;
        }
        let end = (pos + 16).min(path.len());
        while pos < end && path[pos] != b'\n' {
            if (path[pos] < 32 || path[pos] == b'\\') && first_special.is_none() {
                first_special = Some(pos);
            }
            pos += 1;
        }
        if path.get(pos) == Some(&b'\n') {
            break;
        }
    }
    // Short paths end before all four independent digest blocks are consumed.
    while block < 4 {
        let start = block * 16;
        let input = &hash[start..start + 16];
        let output = &mut values[start..start + 16];
        #[cfg(target_arch = "x86_64")]
        if SIMD {
            // SAFETY: same fixed-width bounds and architectural guarantee as above.
            unsafe {
                hex16(input, output)?;
            }
        } else {
            for (dst, &byte) in output.iter_mut().zip(input) {
                *dst = nibble(byte)?;
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        for (dst, &byte) in output.iter_mut().zip(input) {
            *dst = nibble(byte)?;
        }
        block += 1;
    }
    let mut oid = [0; 32];
    for (i, byte) in oid.iter_mut().enumerate() {
        *byte = values[i * 2] << 4 | values[i * 2 + 1];
    }
    // A single CR immediately before LF is framing, not a path event.
    let special = first_special.is_some_and(|i| i + 1 != pos || path[i] != b'\r');
    Some((oid, pos, special))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn hex16(input: &[u8], output: &mut [u8]) -> Option<()> {
    use std::arch::x86_64::{
        _mm_and_si128, _mm_cmpgt_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_or_si128,
        _mm_set1_epi8, _mm_storeu_si128, _mm_sub_epi8,
    };
    // SAFETY: caller supplies readable/writable slices of at least 16 bytes.
    unsafe {
        let x = _mm_loadu_si128(input.as_ptr().cast());
        let digits = _mm_and_si128(
            _mm_cmpgt_epi8(x, _mm_set1_epi8(47)),
            _mm_cmpgt_epi8(_mm_set1_epi8(58), x),
        );
        let letters = _mm_and_si128(
            _mm_cmpgt_epi8(x, _mm_set1_epi8(96)),
            _mm_cmpgt_epi8(_mm_set1_epi8(103), x),
        );
        if _mm_movemask_epi8(_mm_or_si128(digits, letters)) != 65535 {
            return None;
        }
        let values = _mm_or_si128(
            _mm_and_si128(digits, _mm_sub_epi8(x, _mm_set1_epi8(48))),
            _mm_and_si128(letters, _mm_sub_epi8(x, _mm_set1_epi8(87))),
        );
        _mm_storeu_si128(output.as_mut_ptr().cast(), values);
    }
    Some(())
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn path16(input: &[u8]) -> (u32, u32) {
    use std::arch::x86_64::{
        _mm_cmpeq_epi8, _mm_loadu_si128, _mm_min_epu8, _mm_movemask_epi8, _mm_or_si128,
        _mm_set1_epi8,
    };
    // SAFETY: caller supplies at least 16 readable bytes, including at EOF.
    unsafe {
        let x = _mm_loadu_si128(input.as_ptr().cast());
        let ends = _mm_cmpeq_epi8(x, _mm_set1_epi8(10));
        let controls = _mm_cmpeq_epi8(_mm_min_epu8(x, _mm_set1_epi8(31)), x);
        let special = _mm_or_si128(controls, _mm_cmpeq_epi8(x, _mm_set1_epi8(92)));
        (
            _mm_movemask_epi8(ends).cast_unsigned(),
            _mm_movemask_epi8(special).cast_unsigned(),
        )
    }
}
