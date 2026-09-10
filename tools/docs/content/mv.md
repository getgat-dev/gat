Moving a tracked path renames the working-tree path, then publishes the move to
`gat.lock`, updates <Tooltip tip="Local bookkeeping recording which content Gat last placed at each managed path. Sync uses it to distinguish clean files from local changes." cta="How sync plans changes" href="/concepts/how-gat-works#how-sync-decides-what-to-change">materialized ownership</Tooltip>, and regenerates managed excludes.
Renaming reuses the content ID; see [path-independent content identity](/concepts/how-gat-works#content-identity-is-independent-of-path).
Destinations must be explicit; `--force` is required to replace an existing destination.

A move can change the [route](/guides/using-multiple-remotes#remote-selection-at-a-glance)
that selects storage for the new path. Upload the required objects before
[publishing the changed lock](/guides/branching-and-merging#typical-workflows).

<Accordion title="Recover from a partial failure">
If tracking-state publication fails, Gat attempts to rename the path back.
The error distinguishes a completed rollback from a failed rollback. After a
failed rollback, preserve the moved files and inspect `gat.lock` before
reconciling tracking. A rollback does not restore an overwritten destination.

If metadata cleanup fails after publication, the move is already recorded in
`gat.lock`. Repeating the original move does not resume cleanup. Inspect the
moved files and tracking state before running `gat sync`.
</Accordion>
