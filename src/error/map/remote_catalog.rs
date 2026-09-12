//! `Failure` mapping for `gat_engine::RemoteCatalogError`.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_engine::RemoteCatalogError;

impl From<RemoteCatalogError> for Failure {
    fn from(err: RemoteCatalogError) -> Self {
        match err {
            RemoteCatalogError::Identity(source) => source.into(),
            RemoteCatalogError::UnknownOverride(source) => source.into(),
            RemoteCatalogError::UnknownDefault { name } => Self::expected(
                Diagnostic::new(ErrorCode::RemoteNotFound, "Unknown default remote")
                    .with_subject(UserLine::identifier(&name))
                    .with_hint(UserLine::compose([
                        UserLine::authored("add it with `"),
                        UserLine::compose([
                            UserLine::authored("gat remote add "),
                            UserLine::identifier(&name),
                            UserLine::authored(" <url>"),
                        ])
                        .unbroken(),
                        UserLine::authored("` or fix remotes.default"),
                    ])),
            ),
            RemoteCatalogError::NoRemoteConfigured => Self::expected(
                Diagnostic::new(ErrorCode::RemoteNotFound, "No remote configured").with_hint(
                    UserLine::compose([
                        UserLine::authored("run "),
                        UserLine::authored("`gat remote add <name> <url>`").unbroken(),
                    ]),
                ),
            ),
        }
    }
}

impl From<gat_engine::RemoteIdentityError> for Failure {
    fn from(err: gat_engine::RemoteIdentityError) -> Self {
        let summary = match err {
            gat_engine::RemoteIdentityError::ForeignOwner => {
                "Remote or route belongs to another operation"
            }
            gat_engine::RemoteIdentityError::Exhausted => "Operation identity space exhausted",
        };
        Self::infrastructure(Diagnostic::new(ErrorCode::Internal, summary), err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreign_catalog_diagnostic_does_not_render_configured_secrets() {
        use gat_core::{
            config::{RemoteConfig, RemotesConfig},
            name::RemoteName,
        };
        let mut config = RemotesConfig::default();
        let name = RemoteName::from("SYNTHETIC-SECRET");
        config.by_name.insert(
            name.clone(),
            RemoteConfig::from("unsupported://SYNTHETIC-SECRET".to_owned()),
        );
        let owner = gat_engine::RemoteCatalog::from_config(&config).unwrap();
        let foreign = gat_engine::RemoteCatalog::from_config(&config).unwrap();
        let error = foreign
            .remote_name(owner.id_of(&name).unwrap())
            .unwrap_err();
        let failure = Failure::from(gat_engine::RemoteSessionError::Identity(error));
        let diagnostic = failure.diagnostic();
        assert_eq!(diagnostic.code(), ErrorCode::Internal);
        assert!(!diagnostic.summary().contains("SYNTHETIC-SECRET"));
        assert!(diagnostic.subject().is_none());
        assert!(!diagnostic.hints().join(" ").contains("SYNTHETIC-SECRET"));
    }
}
