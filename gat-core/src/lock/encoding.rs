//! Canonical lock serialization without per-row formatting or allocation.

use super::{Entry, VERSION};
use std::io::{self, Write};

/// Write the canonical version line, including its LF terminator.
///
/// # Errors
/// Returns the destination's write error.
pub fn write_header(out: &mut impl Write) -> io::Result<()> {
    out.write_all(VERSION.as_bytes())?;
    out.write_all(b"\n")
}

/// Write one canonical row, without allocating or retaining it.
///
/// The caller establishes ordering and whole-lock invariants. Controls in the
/// decoded path (including CR and LF) always use lowercase `\xhh` escapes.
/// Ordinary spans are passed directly to the destination, so a buffered sink
/// can stream arbitrarily long paths without materializing an encoded row.
///
/// # Errors
/// Returns the destination's write error. Partial output is possible; callers
/// publishing a file must discard their staging file on failure.
pub fn write_row(out: &mut impl Write, entry: &Entry) -> io::Result<()> {
    let mut hex = [0; 64];
    entry.oid.encode_hex(&mut hex);
    out.write_all(&hex)?;
    out.write_all(b"\t")?;
    let path = entry.path.as_str().as_bytes();
    let mut start = 0;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, &byte) in path.iter().enumerate() {
        if byte < 32 {
            out.write_all(&path[start..i])?;
            out.write_all(&[
                b'\\',
                b'x',
                HEX[usize::from(byte >> 4)],
                HEX[usize::from(byte & 15)],
            ])?;
            start = i + 1;
        }
    }
    out.write_all(&path[start..])?;
    out.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexical_path::GatPath, oid::Oid};

    #[test]
    fn canonical_bytes_match_display_for_all_digest_bytes_and_controls() {
        let controls: String = (0..32).map(char::from).collect();
        let path = GatPath::parse_canonical(&format!("dir/é\" {controls}終\r\n")).unwrap();
        for offset in 0..=255u8 {
            let entry = Entry {
                path: path.clone(),
                oid: Oid::from_bytes(std::array::from_fn(|i| {
                    offset.wrapping_add(u8::try_from(i).unwrap())
                })),
            };
            let mut bytes = Vec::new();
            write_header(&mut bytes).unwrap();
            write_row(&mut bytes, &entry).unwrap();
            assert_eq!(
                bytes,
                format!(
                    "{VERSION}\n{}\t{}\n",
                    entry.oid,
                    super::super::EscapedPath(&entry.path)
                )
                .as_bytes()
            );
        }
    }
}
