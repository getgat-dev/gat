//! `Failure` mapping for `gat_command::RemoteError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::RemoteError;

impl From<RemoteError> for Failure {
    fn from(err: RemoteError) -> Self {
        if let RemoteError::ValidateUrl(source) = &err {
            let kind = source.kind().clone();
            let template = source.template().clone();
            return super::super::remote::semantic_remote_open_failure(&kind, &template, err);
        }
        match err {
            RemoteError::DefaultWouldDangle { name } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    "Removal would leave the default remote dangling",
                )
                .with_subject(UserLine::identifier(name.as_str()))
                .with_hint(UserLine::compose([
                    UserLine::authored("Change or unset the default with "),
                    UserLine::authored("`gat remote default`").unbroken(),
                    UserLine::authored(" before removing this definition."),
                ])),
            ),
            RemoteError::Scope(source) => source.into(),
            RemoteError::ReservedName { name } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidConfig, "That remote name is reserved")
                    .with_subject(UserLine::identifier(name.as_str()))
                    .with_hint("Choose a different name."),
            ),
            RemoteError::UnknownRemote { name } => Self::expected(
                Diagnostic::new(ErrorCode::RemoteNotFound, "No remote with that name")
                    .with_subject(UserLine::identifier(name.as_str())),
            ),
            RemoteError::DuplicateRemote { name } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidConfig, "That remote already exists")
                    .with_subject(UserLine::identifier(name.as_str()))
                    .with_hint(UserLine::compose([
                        UserLine::authored("Run `"),
                        UserLine::compose([
                            UserLine::authored("gat remote update "),
                            UserLine::identifier(name.as_str()),
                            UserLine::authored(" --url <url>"),
                        ])
                        .unbroken(),
                        UserLine::authored("` instead."),
                    ])),
            ),
            RemoteError::Repository(source) => (*source).into(),
            RemoteError::ValidateUrl(_) => unreachable!("handled above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::{ErrorCode, Failure};
    use gat_core::endpoint::RemoteUrlTemplate;
    use std::error::Error as _;

    #[test]
    fn validation_failure_retains_the_command_error_without_exposing_io_types() {
        let template = RemoteUrlTemplate::from_string(
            "unsupported://host/path?token=SYNTHETIC-SECRET".to_string(),
        );
        let source = gat_engine::validate_remote_url(&template).unwrap_err();
        let failure: Failure = gat_command::RemoteError::ValidateUrl(source).into();

        assert_eq!(failure.diagnostic().code(), ErrorCode::RemoteInvalid);
        assert!(failure.diagnostic().subject().is_some_and(
            |subject| subject.contains("token=") && !subject.contains("SYNTHETIC-SECRET")
        ));
        assert!(!failure.diagnostic().summary().contains("SYNTHETIC-SECRET"));
        assert!(
            failure
                .technical_source()
                .is_some_and(|source| source.downcast_ref::<gat_command::RemoteError>().is_some())
        );
    }

    #[test]
    fn invalid_interpolation_remains_an_expected_argument_failure() {
        let template = RemoteUrlTemplate::from_string(
            "file:///tmp?token=SYNTHETIC-SECRET&value=${TOKEN=SECRET}".to_string(),
        );
        let source = gat_engine::validate_remote_url(&template).unwrap_err();
        let failure: Failure = gat_command::RemoteError::ValidateUrl(source).into();

        assert_eq!(failure.diagnostic().code(), ErrorCode::InvalidArgumentValue);
        assert_eq!(
            failure.diagnostic().summary(),
            "Invalid `${...}` interpolation"
        );
        assert!(!failure.diagnostic().summary().contains("SYNTHETIC-SECRET"));
        assert!(!failure.diagnostic().summary().contains("TOKEN=SECRET"));
    }

    #[test]
    fn validation_failure_preserves_the_complete_hidden_source_chain() {
        const SECRET: &str = "SYNTHETIC-REMOTE-SECRET";
        let validation = gat_engine::test_support::remote_url_validation_error_for_test(
            "s3://bucket/path?token=<redacted>",
            SECRET,
        );
        let error = gat_command::RemoteError::ValidateUrl(validation);
        let failure: Failure = error.into();

        assert_eq!(failure.diagnostic().code(), ErrorCode::RemoteInvalid);
        assert!(
            failure
                .diagnostic()
                .subject()
                .is_some_and(|subject| subject.contains("token=") && !subject.contains(SECRET))
        );
        assert!(!failure.diagnostic().summary().contains(SECRET));

        let command = failure
            .technical_source()
            .and_then(|source| source.downcast_ref::<gat_command::RemoteError>())
            .expect("failure must retain the command error");
        let gat_command::RemoteError::ValidateUrl(validation) = command else {
            panic!("command error must retain URL validation");
        };
        let open = validation
            .source()
            .and_then(|source| source.downcast_ref::<gat_engine::RemoteOpenError>())
            .expect("validation must retain the engine remote-open error");
        let io_open = open
            .source()
            .and_then(|source| source.downcast_ref::<gat_io::OpenRemoteError>())
            .expect("engine error must retain the I/O remote-open error");
        let gat_io::OpenRemoteError::Remote(remote) = io_open else {
            panic!("I/O remote-open error must retain the classified remote error");
        };
        let backend = remote
            .source()
            .and_then(|source| source.downcast_ref::<gat_io::RemoteBackendError>())
            .expect("classified remote error must retain the backend wrapper");
        assert!(
            backend
                .source()
                .and_then(|source| source.downcast_ref::<opendal::Error>())
                .is_some()
        );
    }
}

#[cfg(test)]
mod template_tests {
    use crate::error::Failure;

    #[test]
    fn backend_failure_renders_original_azure_template_and_hides_source() {
        // hygiene-ok: synthetic template for error mapping; no backend is opened.
        let template = "azblob://${CONTAINER}/gat?endpoint=https://${STORAGE_ACCOUNT}.blob.core.windows.net&sas_token=${SAS_TOKEN}&token=literal-secret";
        let validation = gat_engine::test_support::remote_url_validation_error_for_test(
            template,
            "RESOLVED-BACKEND-SECRET",
        );
        assert!(!format!("{validation:?}").contains("literal-secret"));
        assert!(!format!("{validation:?}").contains("RESOLVED-BACKEND-SECRET"));
        let failure: Failure = gat_command::RemoteError::ValidateUrl(validation).into();
        assert_eq!(
            failure.diagnostic().subject(),
            Some(crate::redaction::render_remote_template(&template.into()).as_str())
        );
        assert!(
            !failure
                .diagnostic()
                .summary()
                .contains("RESOLVED-BACKEND-SECRET")
        );
    }
}
