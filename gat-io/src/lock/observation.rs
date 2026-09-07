use super::{
    Entry, Lock, LockError, LockShardId, Result, ShardContentIdentity, ShardIdentityResolution,
};
use crate::file_state::StatProof;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug)]
pub struct ShardObservation {
    pub shard_id: LockShardId,
    pub change: ShardObservationChange,
}

#[derive(Debug)]
pub enum ShardObservationChange {
    Unchanged,
    StatOnly(StatProof),
    Content {
        prior_identity: Option<ShardContentIdentity>,
        identity: ShardContentIdentity,
        proof: StatProof,
        entries: Vec<Entry>,
    },
}

/// Observe every live lock shard against an optional prior identity/proof
/// catalog without exposing shard filesystem paths to the caller.
pub(super) fn observe_shards(
    root: &Path,
    priors: &HashMap<LockShardId, (ShardContentIdentity, Option<StatProof>)>,
) -> Result<Vec<ShardObservation>> {
    let shard_files = super::persistence::list_shard_files(root)?;
    shard_files
        .par_iter()
        .map(|shard| {
            let prior = priors.get(&shard.shard_id).copied();
            let resolution =
                super::identity::resolve_shard_identity(&shard.full_path, prior, || {
                    #[cfg(any(test, feature = "test-support"))]
                    super::identity::race_test_hooks::fire_before_read(&shard.full_path);
                    std::fs::read(&shard.full_path)
                        .map_err(|source| LockError::io("reading", &shard.full_path, source))
                })?;
            let change = match resolution {
                ShardIdentityResolution::StatOnly { identity, .. }
                    if prior.is_some_and(|(prior_identity, _)| prior_identity == identity) =>
                {
                    ShardObservationChange::Unchanged
                }
                ShardIdentityResolution::StatOnly { proof, .. } => {
                    ShardObservationChange::StatOnly(proof)
                }
                ShardIdentityResolution::Coherent {
                    identity,
                    proof,
                    bytes,
                } if prior.is_some_and(|(prior_identity, _)| prior_identity == identity) => {
                    let _ = bytes;
                    ShardObservationChange::StatOnly(proof)
                }
                ShardIdentityResolution::Coherent {
                    identity,
                    proof,
                    bytes,
                } => ShardObservationChange::Content {
                    prior_identity: prior.map(|(identity, _)| identity),
                    identity,
                    proof,
                    entries: parse_shard_bytes(&bytes, &shard.full_path)?,
                },
            };
            Ok(ShardObservation {
                shard_id: shard.shard_id,
                change,
            })
        })
        .collect()
}

fn parse_shard_bytes(bytes: &[u8], full_path: &Path) -> Result<Vec<Entry>> {
    let text = std::str::from_utf8(bytes).map_err(|source| LockError::CorruptShard {
        path: full_path.to_path_buf(),
        source: Box::new(source),
    })?;
    Ok(Lock::parse(text)
        .map_err(|source| LockError::CorruptShard {
            path: full_path.to_path_buf(),
            source: Box::new(source),
        })?
        .entries)
}
