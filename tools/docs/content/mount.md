A mount imports tracked files from another Git repository and owns every
Gat-managed path below its target. Its stable name identifies the source
independently of where the files are placed.

| Storage choice | Behavior on add or update |
| --- | --- |
| Automatic setup (default) | Preserve an existing route at the exact target; otherwise reuse or import the source default remote and create a route. |
| `--remote NAME` | Resolve the name in the destination first, then the source, and set up the target route. |
| `--no-setup` | Skip remote and route setup for this invocation. A later update can set them up. |

`--remote` and `--no-setup` cannot be combined. Setup never changes the
destination's default remote.

<Accordion title="How automatic setup chooses a remote">
  Gat reuses a destination remote with the same URL template as the source
  default remote. Otherwise, it imports that remote under an available name.

  If the source has no default remote, the mount succeeds without automatic
  storage setup. Configure storage with `gat remote` and `gat route`.
</Accordion>

<Note>
Routes and remotes are independent configuration. Moving or removing a mount
does not move or remove them.
</Note>
