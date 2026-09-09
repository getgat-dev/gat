//! `Failure` mapping for configuration errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;

/// `Failure` mapping for `gat_core::config`'s pure semantic/config-value errors.
impl From<gat_core::config::ConfigError> for Failure {
    fn from(err: gat_core::config::ConfigError) -> Self {
        use gat_core::config::ConfigError as DomainError;
        match &err {
            DomainError::InvalidPath { input, .. } => Self::infrastructure(
                Diagnostic::new(ErrorCode::InvalidPath, "Not a valid path")
                    .with_subject(UserLine::path_text(input)),
                err,
            ),
            DomainError::InvalidRoutePath { name, path, .. } => Self::infrastructure(
                Diagnostic::new(ErrorCode::InvalidPath, "This route has an invalid path")
                    .with_subject(UserLine::compose([
                        UserLine::identifier(name),
                        UserLine::authored(" ("),
                        UserLine::identifier(path),
                        UserLine::authored(")"),
                    ])),
                err,
            ),
            DomainError::InvalidMountSourcePath { name, path, .. } => Self::infrastructure(
                Diagnostic::new(ErrorCode::InvalidPath, "This mount has an invalid path")
                    .with_subject(UserLine::compose([
                        UserLine::identifier(name),
                        UserLine::authored(" ("),
                        UserLine::identifier(path),
                        UserLine::authored(")"),
                    ])),
                err,
            ),
            DomainError::ReservedRouteName => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    UserLine::compose([
                        UserLine::authored("Route name `*` is reserved for "),
                        UserLine::authored("`gat route list`").unbroken(),
                        UserLine::authored("'s synthetic row"),
                    ]),
                )
                .with_hint("Rename or remove this route from gat.yaml."),
            ),
            DomainError::RouteConflict {
                first_name,
                second_name,
                path,
            } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    UserLine::compose([
                        UserLine::authored("Two routes ("),
                        UserLine::identifier(first_name),
                        UserLine::authored(" and "),
                        UserLine::identifier(second_name),
                        UserLine::authored(") configure the same path"),
                    ]),
                )
                .with_subject(UserLine::path_text(path))
                .with_hint("Route paths must be unique; rename or remove one of them."),
            ),
            DomainError::MountTargetIsRoot { name } => {
                let subject = name.as_deref().unwrap_or_default();
                Self::expected(
                    Diagnostic::new(
                        ErrorCode::InvalidConfig,
                        "A mount target cannot be `.` (the repository root)",
                    )
                    .with_subject(UserLine::identifier(subject)),
                )
            }
            DomainError::MountTargetOverlap {
                name,
                target,
                other_name,
                other_target,
            } => {
                let subject = name.as_deref().unwrap_or(target);
                Self::expected(
                    Diagnostic::new(
                        ErrorCode::InvalidConfig,
                        UserLine::compose([
                            UserLine::authored("This mount target overlaps mount `"),
                            UserLine::identifier(other_name),
                            UserLine::authored("`'s target `"),
                            UserLine::identifier(other_target),
                            UserLine::authored("`"),
                        ]),
                    )
                    .with_subject(UserLine::identifier(subject))
                    .with_hint(UserLine::compose([
                        UserLine::authored("Run "),
                        UserLine::authored("`gat mount list`").unbroken(),
                        UserLine::authored(" to inspect every configured mount across all scopes."),
                    ])),
                )
            }
            DomainError::InvalidLinkMode { value } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "Unknown materialization mode",
                )
                .with_subject(UserLine::identifier(value))
                .with_hint(UserLine::compose([
                    UserLine::authored("Valid modes are: "),
                    UserLine::join(
                        gat_core::config::MaterializationMode::ALL
                            .into_iter()
                            .map(|mode| UserLine::identifier(mode.as_str())),
                        ", ",
                    ),
                    UserLine::authored("."),
                ])),
            ),
            DomainError::DuplicateLinkMode { mode } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "Duplicate materialization mode in cache.materialization_strategy",
                )
                .with_subject(UserLine::identifier(mode)),
            ),
            DomainError::EmptyLinkModeList => Self::expected(Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                "cache.materialization_strategy must list at least one mode",
            )),
            DomainError::InvalidIngestStrategy { value } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidArgumentValue, "Unknown ingest strategy")
                    .with_subject(UserLine::identifier(value))
                    .with_hint(UserLine::compose([
                        UserLine::authored("Valid strategies are: "),
                        UserLine::join(
                            gat_core::config::IngestStrategy::ALL
                                .into_iter()
                                .map(|strategy| UserLine::identifier(strategy.as_str())),
                            ", ",
                        ),
                        UserLine::authored("."),
                    ])),
            ),
            DomainError::InvalidShardLevels { value, .. } => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "Invalid lock.shard_levels value (expected a number)",
                )
                .with_subject(UserLine::identifier(value)),
                err,
            ),
            DomainError::ShardLevelsExceedsMaximum { levels, max } => {
                Self::expected(Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    UserLine::compose([
                        UserLine::authored("lock.shard_levels "),
                        UserLine::number(i64::from(*levels)),
                        UserLine::authored(" exceeds the maximum of "),
                        UserLine::number(i64::from(*max)),
                    ]),
                ))
            }
            DomainError::MultilineGitIgnorePattern { pattern } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "git.ignore_patterns entries must not contain line breaks",
                )
                .with_subject(UserLine::identifier(pattern)),
            ),
            DomainError::InvalidGitIgnorePattern { pattern } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "git.ignore_patterns does not support negated (`!`) entries",
                )
                .with_subject(UserLine::identifier(pattern)),
            ),
            DomainError::InvalidMountGlobPattern {
                name,
                field,
                pattern,
                ..
            } => {
                let field = *field;
                Self::infrastructure(
                    Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        UserLine::compose([
                            UserLine::authored("This mount has an invalid "),
                            UserLine::authored(field),
                            UserLine::authored(" pattern"),
                        ]),
                    )
                    .with_subject(UserLine::compose([
                        UserLine::identifier(name),
                        UserLine::authored(" ("),
                        UserLine::identifier(pattern),
                        UserLine::authored(")"),
                    ])),
                    err,
                )
            }
            DomainError::UnknownSelection { name } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidConfig, "No selection with that name")
                    .with_subject(UserLine::identifier(name.as_str())),
            ),
            DomainError::UnknownRemote { name } => Self::expected(
                Diagnostic::new(
                    ErrorCode::RemoteNotFound,
                    "No remote is configured with that name",
                )
                .with_subject(UserLine::identifier(name))
                .with_hint(UserLine::compose([
                    UserLine::authored("Run `"),
                    UserLine::compose([
                        UserLine::authored("gat remote add "),
                        UserLine::identifier(name),
                        UserLine::authored(" <url>"),
                    ])
                    .unbroken(),
                    UserLine::authored("` to configure it."),
                ])),
            ),
            DomainError::InvalidBooleanValue { field, value } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    UserLine::compose([
                        UserLine::authored("Invalid "),
                        UserLine::authored(field),
                        UserLine::authored(" value (expected true/false)"),
                    ]),
                )
                .with_subject(UserLine::identifier(value)),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_ignore_pattern_is_reported_without_injecting_output_lines() {
        let failure = Failure::from(gat_core::config::ConfigError::MultilineGitIgnorePattern {
            pattern: "*.bin\n!AUDIT-SENTINEL\r".to_owned(),
        });
        let diagnostic = failure.diagnostic();
        assert_eq!(diagnostic.code(), ErrorCode::InvalidArgumentValue);
        assert!(!diagnostic.subject().unwrap().contains(['\n', '\r']));
    }

    #[test]
    fn unknown_remote_has_a_remote_not_found_diagnostic() {
        let failure = Failure::from(gat_core::config::ConfigError::UnknownRemote {
            name: "origin".to_string(),
        });
        assert_eq!(failure.diagnostic().code(), ErrorCode::RemoteNotFound);
    }
}
