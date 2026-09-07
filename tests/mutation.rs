mod common;

#[path = "mutation/add.rs"]
mod add;
#[path = "mutation/move.rs"]
mod move_cmd;
#[path = "mutation/remove.rs"]
mod remove;

fn matching_lock_shape(
    repo: &gat_engine::Repository,
    root: &std::path::Path,
) -> Result<Option<gat_core::lock::LockShardLevels>, gat_engine::RepoError> {
    let target = repo.lock_shard_levels()?;
    let layout = gat_io::RepositoryLayout::at(root.to_path_buf());
    Ok(
        match gat_io::LockStore::current_repository_shard_levels(&layout)? {
            Some(levels) if levels == target => Some(levels),
            _ => None,
        },
    )
}
