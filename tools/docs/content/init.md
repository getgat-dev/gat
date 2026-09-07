Prepare the local cache and configure Gat's Git hooks and <Tooltip tip="A Git integration that merges lock entries by path and content ID. Independent changes can merge automatically; conflicting versions of the same path need a decision." cta="Resolve lock conflicts" href="/guides/branching-and-merging">semantic lock merge driver</Tooltip>. Run this in every clone; Git does not copy repository-local integration. See
[Automatic sync](/concepts/automatic-sync) for hook triggers and fetching behavior.

<Note>
Re-running init updates the integration to match the supplied flags.
`--no-hooks` removes existing Gat-managed hooks while preserving other hook code.
`--example-config` creates a commented `gat.yaml` only if none exists.
</Note>

<Accordion title="Git configuration is locked">
  Wait for the Git process to finish. Gat leaves an existing configuration lock
  intact; remove a stale lock only after confirming that no process owns it.
</Accordion>
