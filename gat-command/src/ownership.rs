use gat_core::lexical_path::GatPath;
use gat_core::lock::Entry;
use gat_core::name::MountName;
use gat_engine::MountOwnership;

#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "`{path}` is owned by mount `{mount}` (target `{target}`); only `gat mount` commands can change it"
)]
pub struct OwnershipError {
    pub path: GatPath,
    pub mount: MountName,
    pub target: GatPath,
}

pub(crate) fn assert_root_owned(
    policy: &MountOwnership,
    path: &GatPath,
) -> Result<(), OwnershipError> {
    if let Some(owner) = policy.owner_for_path(path) {
        return Err(OwnershipError {
            path: path.clone(),
            mount: owner.name.clone(),
            target: owner.target.clone(),
        });
    }
    Ok(())
}

pub(crate) fn assert_no_owned_entry(
    policy: &MountOwnership,
    entries: &[Entry],
) -> Result<(), OwnershipError> {
    for entry in entries {
        assert_root_owned(policy, &entry.path)?;
    }
    Ok(())
}

pub(crate) fn assert_no_first_owned_match(
    policy: &MountOwnership,
    first_owned_match: Vec<Option<GatPath>>,
) -> Result<(), OwnershipError> {
    if let Some(path) = first_owned_match.into_iter().flatten().next() {
        return assert_root_owned(policy, &path);
    }
    Ok(())
}
