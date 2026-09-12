use super::*;

fn supported() -> Vec<Kernel> {
    [
        Some(Kernel::scalar()),
        #[cfg(target_arch = "x86_64")]
        std::is_x86_feature_detected!("sse2").then_some(Kernel(Backend::Sse2)),
        #[cfg(target_arch = "x86_64")]
        std::is_x86_feature_detected!("avx2").then_some(Kernel(Backend::Avx2)),
        #[cfg(target_arch = "aarch64")]
        std::arch::is_aarch64_feature_detected!("neon").then_some(Kernel(Backend::Neon)),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn check(kernels: &[Kernel], hash: &[u8; 64], path: &[u8]) {
    let expected = scalar(hash, path);
    for kernel in kernels {
        assert_eq!(
            kernel.row(hash, path),
            expected,
            "hash={hash:?}, path={path:?}"
        );
    }
}

#[test]
fn all_digest_bytes_validate_even_with_an_immediate_lf() {
    let kernels = supported();
    let mut hash = [b'0'; 64];
    for i in 0..64 {
        for byte in 0..=255 {
            hash[i] = byte;
            check(&kernels, &hash, b"\n\\after\r\n");
            assert_eq!(scalar(&hash, b"\n").is_some(), nibble(byte).is_some());
        }
        hash[i] = b'0';
    }
}

#[test]
fn packing_preserves_every_byte_and_lane_order() {
    let kernels = supported();
    for start in 0..=255u8 {
        let mut oid = [0; 32];
        let mut hash = [0; 64];
        for (i, byte) in oid.iter_mut().enumerate() {
            *byte = start.wrapping_add(u8::try_from(i).unwrap());
            hash[2 * i] = b"0123456789abcdef"[usize::from(*byte >> 4)];
            hash[2 * i + 1] = b"0123456789abcdef"[usize::from(*byte & 15)];
        }
        for kernel in &kernels {
            assert_eq!(kernel.row(&hash, b"path\n"), Some((oid, 4, false)));
        }
    }
}

#[test]
fn every_alignment_tail_and_special_position_respects_first_lf() {
    let kernels = supported();
    for alignment in 0..32 {
        let hash_storage = [b'0'; 96];
        let hash = hash_storage[alignment..alignment + 64].try_into().unwrap();
        for len in 0..100 {
            let mut storage = vec![b'a'; alignment + len + 34];
            storage[alignment + len] = b'\n';
            // All bytes after LF are special, including vector lookahead.
            storage[alignment + len + 1..].fill(b'\\');
            // Classification is independent of address alignment. Exercise every
            // class at every position once; cross all alignments with an ordinary
            // special and CR, whose final-position framing behavior is distinct.
            let bytes: &[u8] = if alignment == 0 {
                &[0, 9, 13, 31, 32, 92, 127, 128, 255]
            } else {
                b"\\\r"
            };
            for &byte in bytes {
                for offset in 0..len {
                    // Every event position is covered at alignment zero. Other
                    // alignments retain first/last bytes and both sides of SIMD
                    // block boundaries, rather than repeating interior lanes.
                    if alignment != 0
                        && offset != 0
                        && offset + 1 != len
                        && offset % 16 != 0
                        && offset % 16 != 15
                    {
                        continue;
                    }
                    storage[alignment + offset] = byte;
                    let special =
                        (byte < 32 || byte == b'\\') && !(byte == b'\r' && offset + 1 == len);
                    let expected = Some(([0; 32], len, special));
                    // The expected digest and first LF are fixed by construction;
                    // avoid decoding/scanning them again in a scalar oracle per case.
                    for kernel in &kernels {
                        assert_eq!(kernel.row(hash, &storage[alignment..]), expected);
                        // The exact slice boundary exercises bounded tails.
                        assert_eq!(
                            kernel.row(hash, &storage[alignment..=alignment + len]),
                            expected
                        );
                    }
                    storage[alignment + offset] = b'a';
                }
            }
            check(&kernels, hash, &storage[alignment..alignment + len]);
            check(&kernels, hash, &storage[alignment..]);
        }
    }
}

#[test]
fn randomized_suffixes_match_the_independent_scalar_oracle() {
    let kernels = supported();
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed.to_le_bytes()[0]
    };
    for i in 0..20_000 {
        let mut hash = [0; 64];
        for byte in &mut hash {
            *byte = b"0123456789abcdef"[usize::from(next() & 15)];
        }
        if i % 5 == 0 {
            hash[usize::from(next() & 63)] = next();
        }
        let mut path = vec![0; i % 257];
        for byte in &mut path {
            *byte = next();
        }
        check(&kernels, &hash, &path);
    }
}

#[test]
fn crlf_framing_does_not_hide_earlier_controls() {
    let kernels = supported();
    for len in 0..100 {
        let mut path = vec![b'a'; len];
        path.extend_from_slice(b"\r\n\\\0");
        for kernel in &kernels {
            assert_eq!(
                kernel.row(&[b'0'; 64], &path),
                Some(([0; 32], len + 1, false))
            );
        }
        if len != 0 {
            path[0] = b'\r';
            for kernel in &kernels {
                assert_eq!(
                    kernel.row(&[b'0'; 64], &path),
                    Some(([0; 32], len + 1, true))
                );
            }
        }
    }
}

// Linux guard pages catch overreads beyond the allocation, including SIMD tails.
// Other platforms run the same alignment/tail matrix above without OS-specific FFI.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn protected_pages_bound_digest_and_path_loads() {
    use std::ffi::{c_int, c_long, c_void};
    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        fn sysconf(name: c_int) -> c_long;
    }
    struct Pages {
        base: *mut c_void,
        page: usize,
    }
    impl Pages {
        fn new() -> Self {
            // SAFETY: Linux _SC_PAGESIZE has no pointer arguments. mmap creates
            // three private anonymous pages, initially inaccessible. Only the
            // middle page becomes readable/writable; no other thread uses it.
            unsafe {
                let page = usize::try_from(sysconf(30)).unwrap();
                let base = mmap(std::ptr::null_mut(), page * 3, 0, 2 | 0x20, -1, 0);
                assert_ne!(base as isize, -1);
                let pages = Self { base, page };
                assert_eq!(mprotect(base.cast::<u8>().add(page).cast(), page, 1 | 2), 0);
                pages
            }
        }
        fn ending(&mut self, len: usize) -> &mut [u8] {
            assert!(len <= self.page);
            // SAFETY: the exclusive borrow covers only initialized writable
            // memory in the middle page, ending exactly at the protected page.
            unsafe {
                std::slice::from_raw_parts_mut(self.base.cast::<u8>().add(self.page * 2 - len), len)
            }
        }
    }
    impl Drop for Pages {
        fn drop(&mut self) {
            // SAFETY: this mapping is uniquely owned and no slices survive Drop.
            unsafe {
                munmap(self.base, self.page * 3);
            }
        }
    }
    let kernels = supported();
    let mut hashes = Pages::new();
    let hash = hashes.ending(64);
    hash.fill(b'0');
    let hash: &[u8; 64] = (&*hash).try_into().unwrap();
    let mut paths = Pages::new();
    for len in 0..130 {
        let path = paths.ending(len);
        path.fill(b'a');
        check(&kernels, hash, path);
        if let Some(last) = path.last_mut() {
            *last = b'\n';
            check(&kernels, hash, path);
            if len > 1 {
                path[len - 2] = b'\r';
                check(&kernels, hash, path);
            }
        }
    }
}
