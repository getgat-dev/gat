Selection is matched against `gat.lock`, so missing working-tree files and lock-only glob matches remain removable. By
default Gat removes both tracked rows and working files; `--cached` retains the files while dropping <Tooltip tip="Gat’s responsibility for tracking and restoring a path. Removing ownership is different from deleting cached content; old Git snapshots can still refer to those bytes.">Gat ownership</Tooltip>.

To reclaim stored bytes, review {{command:gc}} and its history-protection rules.

<Accordion title="Recover from a partial failure">
Removals are published to `gat.lock` before working files are deleted.
If deletion, empty-directory pruning, materialized ownership cleanup, or Git
exclusion regeneration fails afterward, the error identifies the failed stage
and the paths are already untracked. Cleanup may be incomplete. Repeating the
same `gat rm` does not resume cleanup for those paths; inspect remaining files
and metadata before further cleanup. If ownership cleanup fails with
`--cached`, preserve retained files before running `gat sync`: stale ownership
may cause sync to delete them.
</Accordion>
