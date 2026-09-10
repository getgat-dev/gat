Track files, directories, or quoted glob patterns. Gat caches the bytes, records
paths and <Tooltip tip="A BLAKE3 hash of the file bytes. Identical content shares an ID even under different filenames." cta="Content identity" href="/concepts/how-gat-works#content-identity-is-independent-of-path">content IDs</Tooltip> in `gat.lock`, and updates `.git/info/exclude`.
See [the file lifecycle](/concepts/how-gat-works#follow-one-file-through-a-change)
for how this becomes a shared Git version.

Ignore rules from `.gatignore` and Git apply during expansion. `--force` bypasses
ignore filtering only for the requested paths. It still rejects Git-tracked files,
mount-owned paths, unsupported file types, and Gat or Git infrastructure paths.
See [ignore rules](/guides/improving-performance#combine-file-exclusions) for the
difference between filtering ingest and keeping managed files out of Git.

<Accordion title="Tracking succeeded, but metadata cleanup failed">
  If ownership recording or Git exclusion updates fail after publication, the
  paths are already tracked in `gat.lock`. The error names the incomplete stage;
  it does not mean the addition was undone. Inspect the lock and Git exclusions
  before staging files with Git.
</Accordion>
