//! `Failure` mapping for `gat_command::ConfigError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::ConfigError;
use gat_core::config_keys::ConfigResource;

impl From<ConfigError> for Failure {
    fn from(err: ConfigError) -> Self {
        match err {
            ConfigError::ManagedResource {
                key,
                resource,
                read_only,
            } => Self::expected(resource_diagnostic(&key, resource, read_only)),
            ConfigError::UnknownKey { key } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidArgumentValue, "Unknown config key")
                    .with_subject(UserLine::config_key(&key))
                    .with_detail(UserLine::compose([
                        UserLine::authored("Expected one of: "),
                        UserLine::join(
                            gat_core::config_keys::SettingKey::CANONICAL
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

fn resource_diagnostic(key: &str, resource: ConfigResource, read_only: bool) -> Diagnostic {
    let default_pointer = matches!(
        (resource, key),
        (ConfigResource::Remote, "remotes.default")
            | (ConfigResource::Selection, "selections.default")
    );
    let (inspect, update, help) = match resource {
        ConfigResource::Remote if default_pointer => (
            "gat remote default",
            "gat remote default NAME",
            "gat remote default --help",
        ),
        ConfigResource::Selection if default_pointer => (
            "gat selection default",
            "gat selection default NAME",
            "gat selection default --help",
        ),
        ConfigResource::Remote => (
            "gat remote show NAME",
            "gat remote update NAME --url URL",
            "gat remote --help",
        ),
        ConfigResource::Route => (
            "gat route show NAME",
            "gat route update NAME --path PATH --remote REMOTE",
            "gat route --help",
        ),
        ConfigResource::Mount => (
            "gat mount show NAME",
            "gat mount update NAME",
            "gat mount --help",
        ),
        ConfigResource::Selection => (
            "gat selection show NAME",
            "gat selection update NAME",
            "gat selection --help",
        ),
    };
    let mut diagnostic = Diagnostic::new(
        ErrorCode::InvalidArgumentValue,
        "Managed resources use their own commands",
    )
    .with_subject(UserLine::config_key(key))
    .with_hint(command_hint(
        if read_only { inspect } else { update },
        if default_pointer && read_only {
            " to inspect the default."
        } else if default_pointer {
            " to choose the default; use `--unset` to restore inheritance."
        } else if read_only {
            " to inspect a named definition."
        } else {
            " with the fields to change; omit fields you want to keep."
        },
    ))
    .with_hint(command_hint(help, " for available operations and options."));
    if resource == ConfigResource::Mount
        && !read_only
        && key.strip_prefix("mounts.").is_some_and(|field| {
            field
                .split_once('.')
                .is_some_and(|(_, field)| field == "rev_lock")
        })
    {
        diagnostic = diagnostic.with_detail(
            "`rev_lock` is generated by mount add/update. Use `--rev REV` to select a revision.",
        );
    }
    diagnostic
}

fn command_hint(command: &'static str, suffix: &'static str) -> UserLine {
    UserLine::compose([
        UserLine::authored("Use "),
        UserLine::compose([
            UserLine::authored("`"),
            UserLine::authored(command),
            UserLine::authored("`"),
        ])
        .unbroken(),
        UserLine::authored(suffix),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_rejections_offer_the_owning_update_command_and_escape_keys() {
        for (resource, command) in [
            (ConfigResource::Remote, "gat remote update NAME --url URL"),
            (
                ConfigResource::Route,
                "gat route update NAME --path PATH --remote REMOTE",
            ),
            (ConfigResource::Mount, "gat mount update NAME"),
            (ConfigResource::Selection, "gat selection update NAME"),
        ] {
            let failure: Failure = ConfigError::ManagedResource {
                key: "name\nSENTINEL\x1b[31m.field".to_string(),
                resource,
                read_only: false,
            }
            .into();
            let diagnostic = failure.diagnostic();
            let subject = diagnostic.subject_line().unwrap().as_str();
            assert!(!subject.contains('\n'));
            assert!(!subject.contains('\x1b'));
            assert!(diagnostic.hints().iter().any(|hint| hint.contains(command)));
        }
    }

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
        for key in gat_core::config_keys::SettingKey::CANONICAL {
            assert!(
                words
                    .iter()
                    .any(|word| word.trim_end_matches(',') == key.as_str())
            );
        }
        assert!(!detail.as_str().contains("git.exclude_patterns"));
    }
}
