<Tooltip tip="Deleting stored content that is not protected by the selected locks and histories. Protection depends on the repositories and history you supply, not on when an object was last downloaded.">Garbage collection</Tooltip> keeps objects referenced by the selected Git histories and
the current repository's working lock. Local-cache and remote collection use
the same rules to decide what to keep. See [history selection](/concepts/history-selection)
for snapshot traversal and [shared caches](/guides/improving-performance#reuse-a-cache-across-repositories)
for storage used by multiple clones.

| History choice | Protected snapshots |
| --- | --- |
| No history flags | All available history, plus the current working lock. |
| Explicit history flags | Selected history from every repository, plus the current working lock. |
| `--no-history` | The current working lock and each additional repository's HEAD lock. |

Use repeatable `--repository <LOCATION>` to protect other Git repositories.
History flags apply to each repository independently: revision and exclusion
names must resolve in every repository. Per-repository selectors are not supported.

```sh
gat gc --dry-run --repository ../other-project --repository https://example.com/team/project.git
gat gc --remote origin --dry-run --repository ../other-project
```

<Warning>
Supply every repository that shares the storage, including shared local caches.
Gat does not discover them automatically. Only committed locks from additional
repositories are protected; their uncommitted changes are not included.
</Warning>

<Accordion title="How additional repositories are inspected">
  Local paths, file URLs, and remote Git URLs all use temporary bare clones.
  Gat removes these clones after inspection.
</Accordion>

Start with `--dry-run`. Remote deletion requires `--unsafe`:

```sh
gat gc --remote origin --unsafe --repository ../other-project
```

<Danger>
An inspection failure or shallow history blocks deletion unless `--unsafe` is
supplied; dry runs report uncertain objects. `--unsafe` can delete objects still
in use. It does not protect against concurrent uploads or unlisted repositories
sharing storage.
</Danger>
