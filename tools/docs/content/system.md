Inspect and maintain <Tooltip tip="Clone-specific bookkeeping, cache data, and Git integration. These commands operate on local maintenance concerns rather than publishing new asset versions." cta="Understand the local files" href="/concepts/how-gat-works#what-lives-where">local repository state</Tooltip> with these experimental commands.

| Command | Purpose |
| --- | --- |
| `inspect` | Read-only checks of lock, state, cache, and Git integration. |
| `repair` | Restore repository consistency. |
| `clean` | Remove disposable state. Explicit purge flags can delete cached objects or temporary files. |

<Warning>
Purging cached objects can remove your only copy of unpublished content.
Upload or back up needed objects before using `--purge-objects`.
</Warning>

For missing working files or damaged cached content, start with
[missing and corrupted objects](/concepts/automatic-sync#missing-and-corrupted-objects).
To remove unneeded stored content using history-based protection, see {{command:gc}}.

<Accordion title="An interrupted lock update needs recovery">
  Gat reports unresolved lock state and any validated recovery commands.
  `repair all` repairs other domains only after blocking lock problems are resolved.

  | Report | Next step |
  | --- | --- |
  | One recovery completed, but other problems remain | Review the remaining findings before continuing. |
  | Several transactions support the requested recovery | Use `--transaction` with an ID from a suggested command. |
  | No transaction supports the requested recovery | Review the findings; a transaction ID alone cannot make the recovery valid. |

  A <Tooltip tip="A recorded attempt to change the lock's layout. After an interruption, Gat inspects its saved state before offering recovery.">lock transaction</Tooltip>
  can remain after an interrupted [layout conversion](/guides/improving-performance#shard-a-very-large-gat-lock).
</Accordion>
