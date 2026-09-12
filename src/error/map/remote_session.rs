//! `Failure` mapping for `gat_engine::RemoteSessionError`.

use crate::error::Failure;
use gat_engine::RemoteSessionError;

impl From<RemoteSessionError> for Failure {
    fn from(err: RemoteSessionError) -> Self {
        match &err {
            RemoteSessionError::Identity(source) => (*source).into(),
            RemoteSessionError::Open { source } => {
                let kind = source.kind().clone();
                let template = source.template().clone();
                super::remote::semantic_remote_open_failure(&kind, &template, err)
            }
        }
    }
}
