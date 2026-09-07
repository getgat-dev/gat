//! `Failure` mapping for `gat_command::ConfigError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::ConfigError;

impl From<ConfigError> for Failure {
    fn from(err: ConfigError) -> Self {
        match err {
            ConfigError::UnknownKey { key, supported } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidArgumentValue, "Unknown config key")
                    .with_subject(UserLine::config_key(&key))
                    .with_detail(UserLine::compose([
                        UserLine::authored("Expected one of: "),
                        UserLine::identifier(&supported),
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
                        UserLine::authored("Use `gat config "),
                        UserLine::identifier(key.as_str()),
                        UserLine::authored(" <value>`."),
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
                .with_hint(
                    "Use `--clear` to persist an explicit empty list instead of `Set` with \
                         zero values.",
                ),
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

    /// [`ConfigError::InvalidGlobPattern`] retention: the mapper
    /// must keep the whole typed error (key + source) as
    /// `Failure`'s hidden technical source, not merely the inner
    /// `GlobError` -- otherwise the key this failure was raised for is
    /// lost from anything inspecting the technical source.
    /// An unknown key containing an embedded newline must never fabricate
    /// an extra rendered diagnostic line.
    #[test]
    fn unknown_key_detail_never_splits_on_an_embedded_newline_in_the_key() {
        let err = ConfigError::UnknownKey {
            key: "selection.include\nSENTINEL_INJECTED_LINE".to_string(),
            supported: "a, b".to_string(),
        };
        let failure: Failure = err.into();
        let detail = failure
            .diagnostic()
            .detail()
            .expect("UnknownKey attaches a detail");
        assert_eq!(detail.lines().count(), 1);
        assert!(!detail.contains('\n'));
    }

    /// Same guarantee for [`ConfigError::UnknownKey`]'s supported-key list.
    #[test]
    fn unknown_key_detail_never_splits_on_an_embedded_newline_in_supported() {
        let err = ConfigError::UnknownKey {
            key: "sync.bogus".to_string(),
            supported: "a, b\nSENTINEL_INJECTED_LINE".to_string(),
        };
        let failure: Failure = err.into();
        let detail = failure
            .diagnostic()
            .detail()
            .expect("UnknownKey attaches a detail");
        assert_eq!(detail.lines().count(), 1);
        assert!(!detail.contains('\n'));
    }
}
