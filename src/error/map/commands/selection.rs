use crate::error::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::{ResourceScopeError, SavedSelectionError};

impl From<ResourceScopeError> for Failure {
    fn from(error: ResourceScopeError) -> Self {
        Self::expected(
            Diagnostic::new(
                ErrorCode::InvalidConfig,
                "Resource is defined in a different scope",
            )
            .with_subject(UserLine::identifier(&error.name))
            .with_detail(UserLine::compose([
                UserLine::authored("Defined in: "),
                UserLine::identifier(&error.actual.to_string()),
                UserLine::authored("; requested: "),
                UserLine::identifier(&error.requested.to_string()),
            ]))
            .with_hint("Update the defining scope, or add a complete definition in a higher-priority scope."),
        )
    }
}

impl From<SavedSelectionError> for Failure {
    fn from(error: SavedSelectionError) -> Self {
        match error {
            SavedSelectionError::ReservedName => Self::expected(Diagnostic::new(
                ErrorCode::InvalidConfig,
                "Selection name default is reserved",
            )),
            SavedSelectionError::AlreadyExists { name } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidConfig, "Selection already exists")
                    .with_subject(UserLine::identifier(name.as_str()))
                    .with_hint(UserLine::compose([
                        UserLine::authored("Use "),
                        UserLine::authored("`gat selection update`").unbroken(),
                        UserLine::authored(" to edit the existing definition."),
                    ])),
            ),
            SavedSelectionError::NotFound { name } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidConfig, "No selection with that name")
                    .with_subject(UserLine::identifier(name.as_str())),
            ),
            SavedSelectionError::Scope(source) => source.into(),
            SavedSelectionError::Repository(source) => source.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_mismatch_includes_both_scopes_in_a_separate_detail() {
        for kind in [
            gat_command::ResourceKind::Selection,
            gat_command::ResourceKind::Remote,
            gat_command::ResourceKind::Route,
        ] {
            let failure = Failure::from(ResourceScopeError {
                kind,
                name: "runtime".into(),
                actual: gat_core::config::ConfigScope::Local,
                requested: gat_core::config::ConfigScope::Project,
            });
            let diagnostic = failure.diagnostic();
            assert_eq!(
                diagnostic.summary(),
                "Resource is defined in a different scope"
            );
            assert_eq!(diagnostic.subject(), Some("runtime"));
            assert_eq!(
                diagnostic.detail().as_deref(),
                Some("Defined in: local; requested: project")
            );
            assert_eq!(diagnostic.hints().len(), 1);
        }
    }

    #[test]
    fn selection_names_and_scope_errors_never_inject_diagnostic_lines() {
        let sentinel = "runtime\nSENTINEL_SELECTION_NAME";
        for error in [
            SavedSelectionError::NotFound {
                name: sentinel.into(),
            },
            SavedSelectionError::AlreadyExists {
                name: sentinel.into(),
            },
            SavedSelectionError::Scope(ResourceScopeError {
                kind: gat_command::ResourceKind::Selection,
                name: sentinel.into(),
                requested: gat_core::config::ConfigScope::Local,
                actual: gat_core::config::ConfigScope::Project,
            }),
        ] {
            let failure = Failure::from(error);
            let subject = failure.diagnostic().subject().unwrap();
            assert_eq!(subject.lines().count(), 1);
            assert!(subject.contains("SENTINEL_SELECTION_NAME"));
        }
    }
}
