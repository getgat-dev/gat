Without path flags, use the default named selection, or all paths if none is set.
Explicit filters replace that selection; `--path .` selects the whole repository.

<Accordion title="Path selection rules">
  | Selector | Selection used |
  | --- | --- |
  | `--selection NAME` | One saved definition, without changing the default. |
  | `--path`, `--include`, or `--exclude` | <Tooltip tip="Path, include, and exclude flags supplied directly to this command. These replace the saved selection as a whole, so repeat any exclusions you still need." cta="Selection precedence" href="/concepts/path-selection">A complete inline selection</Tooltip>. |

  Named and inline selectors cannot be combined. Paths are relative to the
  repository root; patterns are relative to the selected path.
  See [Path selection](/concepts/path-selection) and [gat selection](/commands/selection).
</Accordion>
