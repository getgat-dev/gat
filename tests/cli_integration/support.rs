//! Small helpers shared by several of `cli_integration`'s
//! concern-specific submodules.

use std::path::Path;

/// Delegates to the shared `test-support` crate's `file_remote_url` so
/// this crate doesn't keep its own duplicate of the Windows-drive-letter
/// handling logic (see `newline_integration.rs`'s copy of this same
/// wrapper, which does the same, since these are separate compiled test
/// crates and cannot share a function directly).
pub fn remote_url(path: &Path) -> String {
    test_support::file_remote_url(path)
}
