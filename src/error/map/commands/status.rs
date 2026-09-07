//! Root-owned failure mapping for local status and tracked-file listing.

use super::super::super::Failure;
use gat_command::{LsFilesError, StatusError};

impl From<StatusError> for Failure {
    fn from(err: StatusError) -> Self {
        match err {
            StatusError::Repository(source) => source.into(),
            StatusError::Compare(source) => source.into(),
        }
    }
}

impl From<LsFilesError> for Failure {
    fn from(err: LsFilesError) -> Self {
        match err {
            LsFilesError::Repository(source) => source.into(),
            LsFilesError::DesiredState(source) => source.into(),
        }
    }
}
