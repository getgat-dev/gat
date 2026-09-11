//! Invocation-scoped immutable input ownership and repository construction.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

pub use gat_io::{EnvironmentName, InputValueReason, InvocationInputError};

#[derive(Clone, Debug)]
pub struct Invocation {
    pub(crate) inputs: Arc<gat_io::InvocationInputs>,
    cancellation: crate::TransferCancellation,
}

impl Invocation {
    /// Cancels work owned by this invocation, including repositories created later.
    #[must_use]
    pub fn cancellation(&self) -> crate::TransferCancellation {
        self.cancellation.clone()
    }

    pub fn capture_process() -> Result<Self, InvocationInputError> {
        Ok(Self {
            inputs: Arc::new(gat_io::InvocationInputs::capture_process()?),
            cancellation: crate::TransferCancellation::default(),
        })
    }
    pub fn from_pairs<K: Into<OsString>, V: Into<OsString>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Self, InvocationInputError> {
        Ok(Self {
            inputs: Arc::new(gat_io::InvocationInputs::from_pairs(pairs)?),
            cancellation: crate::TransferCancellation::default(),
        })
    }
    pub fn discover(&self) -> Result<crate::Repository, crate::RepositoryError> {
        Ok(crate::Repository::from_layout(
            gat_io::RepositoryLayout::discover()?,
            self.inputs.clone(),
            self.cancellation.clone(),
        ))
    }
    pub fn discover_from(
        &self,
        start: PathBuf,
    ) -> Result<crate::Repository, crate::RepositoryError> {
        Ok(crate::Repository::from_layout(
            gat_io::RepositoryLayout::discover_from(start)?,
            self.inputs.clone(),
            self.cancellation.clone(),
        ))
    }
    #[must_use]
    pub fn repository_at(&self, root: PathBuf) -> crate::Repository {
        crate::Repository::from_layout(
            gat_io::RepositoryLayout::at(root),
            self.inputs.clone(),
            self.cancellation.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_reaches_existing_and_future_sessions_but_not_other_invocations() {
        let invocation = Invocation::from_pairs([] as [(&str, &str); 0]).unwrap();
        let isolated = Invocation::from_pairs([] as [(&str, &str); 0]).unwrap();
        let repo = invocation.repository_at(PathBuf::from("unused"));
        let session = crate::session::Session::new(&repo, &Default::default());
        invocation.cancellation().cancel();
        assert!(session.transfer_cancellation().is_cancelled());
        assert!(
            repo.cancellation
                .git_interrupt()
                .load(std::sync::atomic::Ordering::Acquire)
        );
        let later = invocation.repository_at(PathBuf::from("later"));
        assert!(
            crate::session::Session::new(&later, &Default::default())
                .transfer_cancellation()
                .is_cancelled()
        );
        assert!(!isolated.cancellation().is_cancelled());
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(invocation.cancellation().cancelled());
    }
}
