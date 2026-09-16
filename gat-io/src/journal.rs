//! Durable transaction journals owned by the physical I/O layer.

pub mod mount;

/// Physical journal reads distinguish an absent file from an unsupported
/// entry. In particular, a dangling symlink is not abandoned scratch.
#[derive(Debug)]
pub(crate) enum JournalReadError {
    Io(std::io::Error),
    NotRegular,
}

/// Reject non-regular final components before reading their contents.
/// Concurrent replacement of the file or its ancestors is outside this
/// check's contract.
pub(crate) fn read_text_if_present(
    path: &std::path::Path,
) -> Result<Option<String>, JournalReadError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(JournalReadError::NotRegular),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(JournalReadError::Io(error)),
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(JournalReadError::Io)
}
