Routes map tracked path prefixes to named remotes independently of mount ownership.
The most-specific matching path wins; route names identify rules, not their priority.

A <Tooltip tip="A literal repository-relative path matched by complete segments. A route for models covers models/a.bin, but not models-old/a.bin; it is not a glob pattern." cta="Routing examples" href="/guides/using-multiple-remotes#remote-selection-at-a-glance">path prefix</Tooltip>
selects storage for a subtree. An explicit `--remote NAME` overrides routing for one
command. See [Using multiple remotes](/guides/using-multiple-remotes) for setup,
backups, and moving content between stores.

Reads combine routes from all configuration layers. Changes target one layer and
leave mounts, tracked entries, and remote definitions intact. See
[Config inheritance](/concepts/config-inheritance#what-inherits) for local overrides,
and {{command:remote}} to manage the storage locations themselves.
