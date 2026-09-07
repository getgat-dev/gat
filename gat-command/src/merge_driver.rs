//! Git merge-driver use-case orchestration.

use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct MergeDriverRequest<'a> {
    pub ancestor: &'a Path,
    pub ours: &'a Path,
    pub theirs: &'a Path,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeDriverOutcome {
    Applied,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeDriverError {
    #[error(transparent)]
    Engine(#[from] gat_engine::MergeDriverError),
}

pub fn merge_driver(
    request: MergeDriverRequest<'_>,
) -> Result<MergeDriverOutcome, MergeDriverError> {
    gat_engine::merge_driver(request.ancestor, request.ours, request.theirs)?;
    Ok(MergeDriverOutcome::Applied)
}
