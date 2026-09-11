`gat rm PATH` stops tracking matching files and deletes their working copies.
Use `gat rm --cached PATH` to stop tracking while keeping the files on disk.
Paths and glob patterns match entries in `gat.lock`, so you can also remove
entries whose working files are missing.

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
