Moving a tracked path renames the working-tree path, then publishes the move to
`gat.lock`, updates materialized ownership, and regenerates managed excludes.
Renaming reuses the content ID; see [path-independent content identity](/concepts/how-gat-works#content-identity-is-independent-of-path).
Destinations must be explicit; `--force` is required to replace an existing destination.

<Accordion title="Recover from a partial failure">
If tracking-state publication fails, Gat attempts to rename the path back.
The error distinguishes a completed rollback from a failed rollback. After a
failed rollback, preserve the moved files and inspect `gat.lock` before
reconciling tracking. A rollback does not restore an overwritten destination.

If metadata cleanup fails after publication, the move is already recorded in
`gat.lock`. Repeating the original move does not resume cleanup. Inspect the
moved files and tracking state before running `gat sync`.
</Accordion>
