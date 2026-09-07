//! [`GitIgnorePattern`]: one validated `git.ignore_patterns` entry.
//!
//! Wraps a single, already-validated pattern string as gat itself
//! restricts it (single-line and exclusion-only: no leading `!`), while
//! leaving Git's `.gitignore`/`.git/info/exclude` grammar -- anchoring, directory
//! semantics, character classes, and matching itself -- entirely to
//! `gix::ignore::Search` behind `gat-io`. This type does not reimplement
//! that grammar; it only carries the text through, validated once at the
//! `gat.yaml`/`gat config` boundary instead of re-checking it every time
//! excludes are rendered.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An ignore entry must be one non-negated line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitIgnorePatternError {
    /// The pattern starts with `!` (negation), which gat's derived
    /// excludes do not support: they only ever subtract paths, never
    /// re-include one after a broader pattern already excluded it.
    #[error(
        "git.ignore_patterns entry `{pattern}` is negated (`!`); negated patterns are not \
         supported"
    )]
    Negated { pattern: String },
    /// Line breaks could introduce additional rules, including negation.
    #[error("git.ignore_patterns entry `{pattern}` contains a line break")]
    Multiline { pattern: String },
}

/// A single `git.ignore_patterns` entry, validated against gat's one
/// policy (one line, no negated `!`-prefixed patterns -- gat's derived
/// excludes are exclusion-only). Everything else about the pattern text
/// (glob syntax, anchoring, directory trailing-slash semantics, ...)
/// is left for `gix::ignore::Search` to interpret at match time.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitIgnorePattern(String);

impl GitIgnorePattern {
    /// Validate and wrap one pattern. Reject line breaks and a leading
    /// `!` (negation) -- gat's generated excludes only ever subtract
    /// paths, never re-include one after a broader pattern already
    /// excluded it.
    pub fn parse(pattern: impl Into<String>) -> Result<Self, GitIgnorePatternError> {
        let pattern = pattern.into();
        if pattern.contains(['\r', '\n']) {
            return Err(GitIgnorePatternError::Multiline { pattern });
        }
        if pattern.starts_with('!') {
            return Err(GitIgnorePatternError::Negated { pattern });
        }
        Ok(Self(pattern))
    }

    /// The verbatim pattern text, as written into gat's managed
    /// `.git/info/exclude` block or matched by `gix::ignore::Search`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GitIgnorePattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for GitIgnorePattern {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for GitIgnorePattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for GitIgnorePattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_a_plain_pattern() {
        let pattern = GitIgnorePattern::parse("*.safetensors").unwrap();
        assert_eq!(pattern.as_str(), "*.safetensors");
        assert_eq!(pattern.to_string(), "*.safetensors");
    }

    #[test]
    fn parse_rejects_a_negated_pattern() {
        let err = GitIgnorePattern::parse("!keep.txt").unwrap_err();
        assert!(matches!(err, GitIgnorePatternError::Negated { .. }));
    }

    #[test]
    fn deserialize_rejects_a_negated_pattern() {
        let err = yaml_serde::from_str::<GitIgnorePattern>("\"!keep.txt\"").unwrap_err();
        assert!(err.to_string().contains("negated"));
    }

    #[test]
    fn line_breaks_cannot_inject_additional_ignore_rules() {
        for raw in ["*.bin\n!keep.bin", "*.bin\r\n!keep.bin", "data/\r"] {
            assert!(matches!(
                GitIgnorePattern::parse(raw),
                Err(GitIgnorePatternError::Multiline { .. })
            ));
            assert!(matches!(
                crate::config::validate_ignore_patterns(vec![raw.to_owned()]),
                Err(crate::config::ConfigError::MultilineGitIgnorePattern { .. })
            ));
            let yaml = yaml_serde::to_string(raw).unwrap();
            assert!(yaml_serde::from_str::<GitIgnorePattern>(&yaml).is_err());
            let yaml = format!("version: 1\ngit:\n  ignore_patterns:\n    - {yaml}");
            assert!(yaml_serde::from_str::<crate::config::Config>(&yaml).is_err());
        }
    }

    #[test]
    fn serde_round_trips_through_a_plain_yaml_string() {
        let pattern = GitIgnorePattern::parse("/data/").unwrap();
        let yaml = yaml_serde::to_string(&pattern).unwrap();
        assert_eq!(yaml.trim(), "/data/");
        let back: GitIgnorePattern = yaml_serde::from_str(&yaml).unwrap();
        assert_eq!(back, pattern);
    }
}
