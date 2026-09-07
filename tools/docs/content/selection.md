Save <Tooltip tip="A named root path plus optional include and exclude glob patterns. The definition describes a set of tracked paths; it does not store a copy of their bytes." cta="Matching rules and examples" href="/concepts/path-selection">reusable path filters</Tooltip> and choose an optional default. These commands change
configuration only; they do not fetch files, sync the working tree, or change tracking.

<Note>
  Adding a selection does not activate it. Use `gat selection default NAME`
  to choose a default, or `--selection NAME` for one operation. Without a
  default, supported commands select the whole repository.
</Note>

Paths are relative to the repository root; include and exclude patterns are
relative to that path. Exclusions win. Named and inline selectors cannot be
combined, and mount source filters remain independent.

<Accordion title="Defaults and configuration scopes">
  Different names coexist across scopes. A higher-priority definition replaces
  the complete entry of the same name. The default choice inherits separately,
  so a local default can use a shared project definition.

  Use `add --local` with a complete definition to override a project selection.
  Updates and removals must target the defining scope. Removing an override can
  reveal an inherited definition, but cannot leave the default dangling.

  `gat selection default --local --unset` restores inheritance. To persist an
  unrestricted choice, save a selection with `--path .` and no filters, then
  choose it as the default.
</Accordion>

See [Path selection](/concepts/path-selection) for matching rules and examples,
and [Config inheritance](/concepts/config-inheritance) for shared definitions and local overrides.
