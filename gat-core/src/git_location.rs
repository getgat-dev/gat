//! [`GitLocationSpec`]: the domain type for a Git repository location
//! (`gat mount add <NAME> <URL> <TARGET>`, `gat gc --repository`,
//! `MountConfig.url` in `gat.yaml`) exactly as the user/config wrote it --
//! a local filesystem path, a standard Git URL (`https://`, `http://`,
//! `ssh://`, `git://`, `file://`), or scp-like `[user@]host:path`
//! shorthand.
//!
//! Kept Gix-independent in `gat-core`, mirroring
//! [`crate::git::GitRevisionSpec`]. `gat-io::parse_location` is the sole
//! authoritative parser/classifier of the text this type wraps; this type
//! performs no Git-location grammar of its own and stores the raw text
//! completely unvalidated.
//!
//! Unlike [`crate::git::GitRevisionSpec`], this value may embed
//! credentials (`******host/...`, an scp-like userinfo, ...), so -- like
//! [`crate::endpoint::RemoteUrlTemplate`] -- it exposes its raw text only
//! through an explicitly named accessor
//! ([`GitLocationSpec::as_location_str`]) and never through a raw
//! `Display`/derived `Debug`: this type's own `Debug` renders a fixed
//! opaque `<redacted>` marker with no URL/Git parsing dependency; a rich,
//! sanitized rendering is available only through
//! `redaction::RedactedUrl::render`/`redaction::display_url` at a
//! presentation boundary.

use std::fmt;

/// A Git repository location, exactly as written in `gat.yaml`/passed to
/// `gat mount add`/`gat gc --repository` -- unvalidated, and
/// potentially credential-bearing. See the module documentation.
#[derive(Clone, PartialEq, Eq)]
pub struct GitLocationSpec(String);

impl GitLocationSpec {
    /// Wraps an owned location `String`, moving its existing allocation
    /// rather than copying it -- e.g. a `gat mount add`/`update` CLI
    /// argument, a `gat gc --repository` value, or a
    /// `MountConfigInput`/`RemotesConfigInput` value straight off
    /// deserialization.
    #[must_use]
    pub const fn from_string(value: String) -> Self {
        Self(value)
    }

    /// Borrows the raw, unvalidated location text. Named explicitly
    /// (rather than a generic `AsRef<str>`/`Deref<Target = str>`) so
    /// every call site visibly acknowledges it is handling a value that
    /// may embed credentials -- I/O parsing and presentation-layer
    /// redaction are the only legitimate consumers.
    #[must_use]
    pub fn as_location_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for GitLocationSpec {
    fn from(value: String) -> Self {
        Self::from_string(value)
    }
}

impl From<&str> for GitLocationSpec {
    fn from(value: &str) -> Self {
        Self::from_string(value.to_string())
    }
}

/// Fail-closed, opaque `Debug`: never touches the raw location text (and
/// so never needs `redaction`'s `url`/Gix-aware rendering -- this type
/// stays usable from a pure `gat-core`-style layer with no I/O
/// dependency), so `MountConfig`/`RemotesConfig` (both of which derive
/// `Debug`) can never leak a credential through a stray debug log/panic
/// message/`assert!` failure. A caller that needs a rich, sanitized-but-
/// informative rendering (scheme/host/path visible, only credentials
/// redacted) must go through
/// `redaction::RedactedUrl::render`/`redaction::display_url` explicitly,
/// at a presentation boundary permitted to depend on URL/Git parsing.
impl fmt::Debug for GitLocationSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("GitLocationSpec")
            .field(&"<redacted>")
            .finish()
    }
}

/// Serializes as the unchanged location text -- `gat.yaml`'s `mounts:`/
/// `remotes:` format is unaffected by this type's introduction.
impl serde::Serialize for GitLocationSpec {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

/// Deserializes via [`GitLocationSpec::from_string`], retaining the
/// deserialized `String`'s own allocation -- no Git-location grammar
/// validation is performed here (see the type's own doc comment).
impl<'de> serde::Deserialize<'de> for GitLocationSpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_location_str_returns_the_exact_stored_text() {
        let spec = GitLocationSpec::from_string("git@github.com:acme/models.git".to_string());
        assert_eq!(spec.as_location_str(), "git@github.com:acme/models.git");
    }

    #[test]
    fn debug_redacts_embedded_userinfo() {
        // hygiene-ok: synthetic URL string used only to exercise redaction logic; never dialed.
        let spec = GitLocationSpec::from_string("******host/repo.git".to_string());
        let rendered = format!("{spec:?}");
        assert!(!rendered.contains("hunter2"), "rendered = {rendered}");
    }

    #[test]
    fn serde_round_trips_the_exact_location_text() {
        let spec = GitLocationSpec::from_string("../my_models_repo".to_string());
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(json, "\"../my_models_repo\"");
        let round_tripped: GitLocationSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, spec);
    }
}
