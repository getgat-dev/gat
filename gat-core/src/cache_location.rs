//! [`CacheLocation`]: the configured `cache.location` host-path value.
//!
//! Holds the raw, not-yet-joined path text from `gat.yaml` (or a
//! `GAT_CACHE_LOCATION` override) as a native [`PathBuf`], parsed once at the
//! config-deserialization boundary rather than staying a bare [`String`]
//! all the way down to I/O-owned repository cache resolution. Relative
//! locations are joined against the repository root only when an operation's
//! snapshot resolves the final `objects_dir`; this type defines the stored
//! representation without changing join timing.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

/// A `cache.location` value: an absolute path, or one still relative to
/// the repository root. Deliberately holds a native [`PathBuf`] (not a
/// [`String`]) so a configured location can round-trip non-UTF-8 path
/// bytes on platforms that allow them, the same way a `GAT_CACHE_LOCATION`
/// environment override already must.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheLocation(PathBuf);

impl CacheLocation {
    pub fn try_from_path(path: PathBuf) -> Result<Self, crate::settings::SettingValueError> {
        if path.as_os_str().is_empty() {
            return Err(crate::settings::SettingValueError::EmptyPath);
        }
        Ok(Self(path))
    }

    /// The raw configured path, borrowed -- absolute or still relative to
    /// the repository root; callers resolving an actual cache directory
    /// must still apply the repository cache resolver's
    /// absolute/relative join logic rather than using this directly.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl TryFrom<PathBuf> for CacheLocation {
    type Error = crate::settings::SettingValueError;
    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        Self::try_from_path(path)
    }
}

/// Serializes as a plain YAML string, matching `cache.location`'s
/// documented `gat.yaml` shape. Non-UTF-8 paths cannot round-trip through
/// YAML text; a value that reached this from a `gat.yaml` file (the only
/// production source of a persisted [`CacheLocation`]) is always valid
/// UTF-8 to begin with, so this only rejects a value manufactured
/// in-process from arbitrary bytes.
impl Serialize for CacheLocation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0.to_str() {
            Some(s) => serializer.serialize_str(s),
            None => Err(serde::ser::Error::custom(
                "cache.location is not valid UTF-8",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for CacheLocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::try_from_path(PathBuf::from(raw)).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_path_returns_the_wrapped_path() {
        let loc = CacheLocation::try_from_path(PathBuf::from("relative/cache"))
            .expect("nonempty cache location");
        assert_eq!(loc.as_path(), Path::new("relative/cache"));
    }

    #[test]
    fn serde_round_trips_through_a_plain_yaml_string() {
        let loc = CacheLocation::try_from_path(PathBuf::from("/var/cache/gat"))
            .expect("nonempty cache location");
        let yaml = yaml_serde::to_string(&loc).unwrap();
        assert_eq!(yaml.trim(), "/var/cache/gat");
        let back: CacheLocation = yaml_serde::from_str(&yaml).unwrap();
        assert_eq!(back, loc);
    }

    #[test]
    fn deserializes_a_relative_location_unchanged() {
        let loc: CacheLocation = yaml_serde::from_str("relative-cache").unwrap();
        assert_eq!(loc.as_path(), Path::new("relative-cache"));
    }
}
