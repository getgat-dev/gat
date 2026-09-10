List paths from the current `gat.lock`, including files missing from disk.
Filters match tracked paths without walking the filesystem. Use
[Path selection](/concepts/path-selection#preview-your-selection) to preview a working set,
or {{command:status}} to compare the working lock with the staged lock.

<Note>
Output includes headings, counts, and hints. [Human output limits](/references/cli-output)
apply; use `gat ls-files --full-output` for all rows and complete paths. This
remains a human report, not a plain path stream for scripts.
</Note>
