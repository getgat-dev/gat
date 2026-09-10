//! `Failure` mapping for `gat_command::DiffError`.

use super::super::super::Failure;
use gat_command::DiffError;

impl From<DiffError> for Failure {
    fn from(err: DiffError) -> Self {
        match err {
            DiffError::Snapshot(source) => source.into(),
            DiffError::Repository(source) => source.into(),
            DiffError::Compare(source) => source.into(),
        }
    }
}
