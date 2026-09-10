//! Invocation-scoped immutable input ownership and repository construction.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

pub use gat_io::{EnvironmentName, InputValueReason, InvocationInputError};

#[derive(Clone, Debug)]
pub struct Invocation {
    pub(crate) inputs: Arc<gat_io::InvocationInputs>,
}

impl Invocation {
    pub fn capture_process() -> Result<Self, InvocationInputError> {
        Ok(Self {
            inputs: Arc::new(gat_io::InvocationInputs::capture_process()?),
        })
    }
    pub fn from_pairs<K: Into<OsString>, V: Into<OsString>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Self, InvocationInputError> {
        Ok(Self {
            inputs: Arc::new(gat_io::InvocationInputs::from_pairs(pairs)?),
        })
    }
    pub fn discover(&self) -> Result<crate::Repository, crate::RepositoryError> {
        Ok(crate::Repository::from_layout(
            gat_io::RepositoryLayout::discover()?,
            self.inputs.clone(),
        ))
    }
    pub fn discover_from(
        &self,
        start: PathBuf,
    ) -> Result<crate::Repository, crate::RepositoryError> {
        Ok(crate::Repository::from_layout(
            gat_io::RepositoryLayout::discover_from(start)?,
            self.inputs.clone(),
        ))
    }
    #[must_use]
    pub fn repository_at(&self, root: PathBuf) -> crate::Repository {
        crate::Repository::from_layout(gat_io::RepositoryLayout::at(root), self.inputs.clone())
    }
}
