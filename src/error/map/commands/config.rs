//! `Failure` mapping for `gat_command::ConfigError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::ConfigError;

impl From<ConfigError> for Failure {
    fn from(err: ConfigError) -> Self {
        match err {
            ConfigError::UnknownKey { key } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidArgumentValue, "Unknown config key")
                    .with_subject(UserLine::config_key(&key))
                    .with_detail(UserLine::compose([
                        UserLine::authored("Expected one of: "),
                        UserLine::join(
                            gat_core::config_keys::ConfigKey::CANONICAL
                                .into_iter()
                                .map(|key| UserLine::config_key(key.as_str())),
                            ", ",
                        ),
                    ])),
            ),
            ConfigError::WrongValueCount { key, got } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidArgumentValue, "Wrong number of values")
                    .with_subject(UserLine::config_key(key.as_str()))
                    .with_detail(UserLine::compose([
                        UserLine::authored("`"),
                        UserLine::identifier(key.as_str()),
                        UserLine::authored("` takes exactly one value (got "),
                        UserLine::number(got as i64),
                        UserLine::authored(")"),
                    ]))
                    .with_hint(UserLine::compose([
                        UserLine::authored("Use `"),
                        UserLine::compose([
                            UserLine::authored("gat config "),
                            UserLine::identifier(key.as_str()),
                            UserLine::authored(" <value>"),
                        ])
                        .unbroken(),
                        UserLine::authored("`."),
                    ])),
            ),
            ConfigError::ClearNotSupportedForScalar { key } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "`--clear` isn't supported here",
                )
                .with_subject(UserLine::config_key(key.as_str()))
                .with_hint("Use `--unset` instead for a scalar key."),
            ),
            ConfigError::EmptyListNotAllowed { key } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "At least one value is required",
                )
                .with_subject(UserLine::config_key(key.as_str()))
                .with_hint("Use `--clear` to persist an explicit empty list."),
            ),
            ConfigError::ClearAlwaysInvalid { key, source } => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "This key cannot be cleared to an empty list",
                )
                .with_subject(UserLine::config_key(key.as_str())),
                ConfigError::ClearAlwaysInvalid { key, source },
            ),
            ConfigError::Config(source) => source.into(),
            ConfigError::Repository(source) => (*source).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_retain_canonical_choices_as_separate_words() {
        let failure: Failure = ConfigError::UnknownKey {
            key: "selection\nSENTINEL".to_string(),
        }
        .into();
        let diagnostic = failure.diagnostic();
        assert!(!diagnostic.subject_line().unwrap().as_str().contains('\n'));
        let detail = &diagnostic.detail_lines()[0];
        let words: Vec<_> = detail.wrapping_words().collect();
        for key in gat_core::config_keys::ConfigKey::CANONICAL {
            assert!(
                words
                    .iter()
                    .any(|word| word.trim_end_matches(',') == key.as_str())
            );
        }
        assert!(!detail.as_str().contains("git.exclude_patterns"));
    }
}
