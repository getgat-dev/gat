Save <Tooltip tip="A named root path plus optional include and exclude glob patterns. The definition describes a set of tracked paths; it does not store a copy of their bytes." cta="Matching rules and examples" href="/concepts/path-selection">reusable path filters</Tooltip> and choose an optional default. These commands change
configuration only; they do not fetch files, sync the working tree, or change tracking.

Creation requires `--path`, `--include`, or `--exclude`. To save an unrestricted
selection, make that choice explicit:

```sh
gat selection add all --local --path .
```

`list`, `show`, and `default` label this definition **All tracked paths**.
It has no path or pattern restrictions; the label is not a count of current files.
A filter that currently matches no tracked paths is still valid. Preview it with
{{command:ls-files}} using `--selection NAME`; matches can change as the lock changes.

<Note>
  Adding a selection does not activate it. Use `gat selection default NAME`
  to choose a default, or `--selection NAME` for one operation. Without a
  default, supported commands select the whole repository.
</Note>

Paths start at the repository root; patterns start at the selected path.
Exclusions win. Named and inline selectors cannot be combined.
[Mount source filters](/concepts/path-selection#mount-selection) are independent.

<Accordion title="Defaults and configuration scopes">
  Different names coexist across scopes. A higher-priority definition replaces
  the complete entry of the same name. The default choice inherits separately,
  so a local default can use a shared project definition.

  Use `add --local` with a complete definition to override a project selection.
  Updates require at least one path, pattern, or clear option. Updates and
  removals must target the defining scope. Removing an override can
  reveal an inherited definition, but cannot leave the default dangling.

  `gat selection default --local --unset` restores inheritance. To persist an
  unrestricted choice, save a selection with `--path .` and no filters, then
  choose it as the default.
</Accordion>

See [Path selection](/concepts/path-selection) for matching rules and examples,
and [Config inheritance](/concepts/config-inheritance) for shared definitions and local overrides.
