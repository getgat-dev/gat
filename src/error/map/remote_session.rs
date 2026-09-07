//! `Failure` mapping for `gat_engine::RemoteSessionError`.

use crate::error::Failure;
use gat_engine::RemoteSessionError;

impl From<RemoteSessionError> for Failure {
    fn from(err: RemoteSessionError) -> Self {
        let kind = err.kind().clone();
        let template = err.template().clone();
        super::remote::semantic_remote_open_failure(&kind, &template, err)
    }
}
