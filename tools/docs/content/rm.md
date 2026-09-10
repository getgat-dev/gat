Selection is matched against `gat.lock`, so missing working-tree files and lock-only glob matches remain removable. By
default Gat removes both tracked rows and working files; `--cached` retains the files while dropping <Tooltip tip="Gat’s responsibility for tracking and restoring a path. Removing ownership is different from deleting cached content; old Git snapshots can still refer to those bytes.">Gat ownership</Tooltip>.

Commit the changed `gat.lock` to share the removal; see the
[publishing workflow](/guides/branching-and-merging#typical-workflows). Earlier
Git snapshots still reference the old content. To reclaim stored bytes, review
{{command:gc}} and its history-protection rules.

<Accordion title="Recover from a partial failure">
The lock is updated before working files are deleted. A later failure can leave
file deletion, directory pruning, local ownership, or Git exclusions incomplete.
The diagnostic names the failed stage; the paths are already untracked.

Repeating `gat rm` does not resume cleanup. Inspect the remaining files and
metadata first. If ownership cleanup failed with `--cached`, preserve retained
files before running `gat sync`: stale ownership may cause sync to delete them.
See [how sync uses local state](/concepts/how-gat-works#how-sync-decides-what-to-change).
</Accordion>
