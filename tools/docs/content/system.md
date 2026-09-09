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

If lock repair cannot resolve invalid lock or transaction state, the report retains
the inspection findings and any validated recovery choices. `repair all` stops at
that lock report before repairing other domains.

Recovering one transaction can leave other transaction problems unresolved. The
report shows both the completed recovery and remaining findings; `repair all`
continues only once blocking lock state is resolved.

If several transactions support a recovery choice, specify `--transaction` using
one of the recovery commands shown in the report. This is distinct from a request
that matches no eligible transaction.
