//! Environment-variable resolution for locating a user's home directory
//! and runtime overrides. Centralizing these `std::env`
//! reads here keeps them out of the engine, returning already-resolved
//! values so callers needing deterministic behavior (mainly tests) can
//! inject an explicit value instead of reading the process environment
//! at all.

use std::ffi::OsString;
use std::path::PathBuf;

/// The user's home directory: `$HOME` on Unix, `%USERPROFILE%` on
/// Windows. `None` if it isn't set. Read with `var_os` (not `var`) so a
/// non-UTF-8 home directory still resolves instead of being silently
/// discarded.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from)
}

/// An explicit object-cache directory override from `GAT_CACHE_DIR`, if
/// set. Read with `var_os` (not `var`) so a non-UTF-8 override still
/// resolves instead of being silently discarded.
#[must_use]
pub fn cache_dir_override() -> Option<OsString> {
    std::env::var_os("GAT_CACHE_DIR")
}

/// Total network readiness budget from `GAT_CONNECT_TIMEOUT`, in whole seconds.
/// Unset means five seconds; invalid values are rejected without retaining them.
pub fn remote_connect_timeout() -> Result<std::time::Duration, crate::RemoteError> {
    parse_connect_timeout(std::env::var_os("GAT_CONNECT_TIMEOUT").as_deref())
}

fn parse_connect_timeout(
    value: Option<&std::ffi::OsStr>,
) -> Result<std::time::Duration, crate::RemoteError> {
    let Some(value) = value else {
        return Ok(std::time::Duration::from_secs(5));
    };
    let invalid = || crate::RemoteError::InvalidConnectTimeout;
    let value = value.to_str().ok_or_else(invalid)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let seconds = value.parse::<u64>().map_err(|_| invalid())?;
    let budget = std::time::Duration::from_secs(seconds);
    if budget.is_zero() || std::time::Instant::now().checked_add(budget).is_none() {
        return Err(invalid());
    }
    Ok(budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_timeout_defaults_to_five_seconds_and_accepts_overrides() {
        assert_eq!(
            parse_connect_timeout(None).unwrap(),
            std::time::Duration::from_secs(5)
        );
        for seconds in [1, 5, 15, 60] {
            assert_eq!(
                parse_connect_timeout(Some(std::ffi::OsStr::new(&seconds.to_string()))).unwrap(),
                std::time::Duration::from_secs(seconds)
            );
        }
    }

    #[test]
    fn connect_timeout_rejects_invalid_values_without_retaining_them() {
        for value in [
            "",
            "0",
            "-1",
            "+5",
            "1.5",
            "5s",
            " 5",
            "18446744073709551615",
            "18446744073709551616",
            "SYNTHETIC-SECRET",
        ] {
            let error = parse_connect_timeout(Some(std::ffi::OsStr::new(value))).unwrap_err();
            assert!(matches!(error, crate::RemoteError::InvalidConnectTimeout));
            assert!(!format!("{error:?}").contains("SYNTHETIC-SECRET"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn connect_timeout_rejects_non_unicode_values() {
        use std::os::unix::ffi::OsStrExt;
        assert!(matches!(
            parse_connect_timeout(Some(std::ffi::OsStr::from_bytes(&[0xff]))),
            Err(crate::RemoteError::InvalidConnectTimeout)
        ));
    }

    /// `home_dir`/`cache_dir_override` just wrap `std::env::var_os` --
    /// this only asserts they compile and return an `Option` without
    /// panicking; behavior against explicit values is covered by the
    /// pure resolvers in `repository_layout` that take an already
    /// -resolved `Option<&Path>`/`Option<&OsStr>` instead of reading the
    /// environment.
    #[test]
    fn home_dir_and_cache_dir_override_do_not_panic() {
        let _ = home_dir();
        let _ = cache_dir_override();
    }
}
