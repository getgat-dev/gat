//! Git-location validation without exposing the physical Git parser.

use gat_core::git_location::GitLocationSpec;

#[derive(Debug, thiserror::Error)]
#[error("invalid Git repository location")]
pub struct GitLocationValidationError(#[source] gat_io::GitLocationError);

pub fn validate_git_location(location: &GitLocationSpec) -> Result<(), GitLocationValidationError> {
    gat_io::parse_location(location)
        .map(|_| ())
        .map_err(GitLocationValidationError)
}
