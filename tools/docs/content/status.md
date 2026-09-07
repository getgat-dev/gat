Without `--remote`, compare the current lock with the <Tooltip tip="The gat.lock version in Git’s index, prepared by git add for the next commit. This can differ from both the working lock and the last committed lock." cta="Review and publish changes" href="/guides/branching-and-merging#typical-workflows">staged lock</Tooltip>. Deleted files
remain visible because selection uses tracking metadata.

<Note>
Local status does not inspect file contents. Run {{command:add}} to record edits,
or `gat sync --dry-run` to preview working-tree reconciliation. Cache presence
does not verify cached bytes.
</Note>

With `--remote`, check whether required objects exist. A bare
{{arg:status:remote}} follows path routing; a named value forces one remote.
See [remote routing](/guides/using-multiple-remotes#remote-selection-at-a-glance)
for storage precedence. [History flags](/concepts/history-selection) apply only to remote checks. Mount-owned entries show their mount name.
