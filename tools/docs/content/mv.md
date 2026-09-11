`gat mv SOURCE DEST` moves a tracked file or directory and updates its paths in
`gat.lock`. The content IDs stay the same, so renaming does not create new objects.
Supply the complete destination path. Use `--force` only when you intend to
replace an existing destination.

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
