Sync updates working files to match the current `gat.lock`, using the local
cache. Preview updates, removals, conflicts, and missing objects before applying:

```sh
gat sync --dry-run
```

Sync preserves conflicting local edits unless you pass `--force`.
See [how sync plans changes](/concepts/how-gat-works#how-sync-decides-what-to-change)
and [resolving conflicts](/concepts/automatic-sync#local-changes-and-conflicts).

| Option | Effect |
| --- | --- |
| `--fetch` | Fetch missing objects before reconciliation. Also enabled by `sync.auto_fetch`. |
| `--repair` | Fetch clean replacements for corrupted cache objects. Also enabled by `sync.auto_repair`. |
| `--trust-state` | Skip file checks where desired content and recorded materialized state agree. Also enabled by `sync.trust_state`. |
| `--rematerialize` | Recreate clean files using the current materialization strategy, always validating them. Preview with `--dry-run`. |

Without fetching or repair enabled, sync uses only the local cache.
[Materialization strategies](/guides/improving-performance#avoid-unnecessary-copying-during-materialization)
control how cached bytes become working files;
[configuration scopes](/concepts/config-inheritance) control whether sync settings
apply to one clone or the whole project.

<Warning>
Trusting recorded state can miss files changed or deleted outside Gat.
</Warning>
