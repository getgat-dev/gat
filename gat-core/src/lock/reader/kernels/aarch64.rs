use super::{DecodedRow, finish, tail};
use crate::lock::{Result, reader::ValidatedLockFile};
use std::arch::aarch64::{
    uint8x16_t, vaddq_u8, vandq_u8, vbslq_u8, vceqq_u8, vcleq_u8, vdupq_n_u8, vld1q_u8, vmaxvq_u8,
    vminvq_u8, vorrq_u8, vshlq_n_u8, vst1q_u8, vsubq_u8, vuzp1q_u8, vuzp2q_u8,
};
use std::borrow::Cow;

#[target_feature(enable = "neon")]
pub(super) unsafe fn decode_neon(source: Cow<'_, str>) -> Result<ValidatedLockFile<'_>> {
    // SAFETY: the caller checked NEON before entering the entire parser.
    ValidatedLockFile::decode_with(source, |hash, path| unsafe { row_neon(hash, path) })
}

#[inline]
#[target_feature(enable = "neon")]
fn hex16(x: uint8x16_t) -> (uint8x16_t, uint8x16_t) {
    let digits = vcleq_u8(vsubq_u8(x, vdupq_n_u8(b'0')), vdupq_n_u8(9));
    let letters = vcleq_u8(vsubq_u8(x, vdupq_n_u8(b'a')), vdupq_n_u8(5));
    let nibbles = vaddq_u8(
        vandq_u8(x, vdupq_n_u8(15)),
        vandq_u8(letters, vdupq_n_u8(9)),
    );
    (nibbles, vorrq_u8(digits, letters))
}

#[inline]
#[target_feature(enable = "neon")]
fn pack(a: uint8x16_t, b: uint8x16_t) -> uint8x16_t {
    vorrq_u8(vshlq_n_u8::<4>(vuzp1q_u8(a, b)), vuzp2q_u8(a, b))
}

#[inline]
#[target_feature(enable = "neon")]
pub(super) unsafe fn row_neon(hash: &[u8; 64], path: &[u8]) -> DecodedRow {
    // SAFETY: digest loads cover exactly hash; stores cover exactly oid.
    // Every path load is guarded by at least 16 remaining initialized bytes.
    unsafe {
        let (n0, v0) = hex16(vld1q_u8(hash.as_ptr()));
        let (n1, v1) = hex16(vld1q_u8(hash.as_ptr().add(16)));
        let (n2, v2) = hex16(vld1q_u8(hash.as_ptr().add(32)));
        let (n3, v3) = hex16(vld1q_u8(hash.as_ptr().add(48)));
        if vminvq_u8(vandq_u8(vandq_u8(v0, v1), vandq_u8(v2, v3))) != 255 {
            return None;
        }
        let mut oid = [0; 32];
        vst1q_u8(oid.as_mut_ptr(), pack(n0, n1));
        vst1q_u8(oid.as_mut_ptr().add(16), pack(n2, n3));
        let indices = vld1q_u8([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15].as_ptr());
        let mut pos = 0;
        let mut first_special = None;
        while path.len() - pos >= 16 {
            let x = vld1q_u8(path.as_ptr().add(pos));
            let ends = vceqq_u8(x, vdupq_n_u8(10));
            let special = vorrq_u8(vcleq_u8(x, vdupq_n_u8(31)), vceqq_u8(x, vdupq_n_u8(92)));
            if vmaxvq_u8(ends) != 0 {
                let n = usize::from(vminvq_u8(vbslq_u8(ends, indices, vdupq_n_u8(255))));
                if first_special.is_none() {
                    let first = usize::from(vminvq_u8(vbslq_u8(special, indices, vdupq_n_u8(255))));
                    if first < n {
                        first_special = Some(pos + first);
                    }
                }
                return Some(finish(oid, path, pos + n, first_special));
            }
            if first_special.is_none() && vmaxvq_u8(special) != 0 {
                first_special =
                    Some(pos + usize::from(vminvq_u8(vbslq_u8(special, indices, vdupq_n_u8(255)))));
            }
            pos += 16;
        }
        tail(oid, path, pos, first_special)
    }
}
