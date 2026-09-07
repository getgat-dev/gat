List paths from the current `gat.lock`, including files missing from disk.
Filters match tracked paths without walking the filesystem. Use
[Path selection](/concepts/path-selection#preview-your-selection) to preview a working set,
or {{command:status}} to compare the working lock with the staged lock.

<Note>
Output includes headings, counts, and hints. It is not a plain path stream for scripts.
</Note>
