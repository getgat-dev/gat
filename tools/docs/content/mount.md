A mount imports tracked files from another Git repository and owns every
Gat-managed path below its <Tooltip tip="The destination subtree in your repository. Gat tracking commands cannot change mount-owned entries; update them through the source or detach the mount." cta="Mount ownership" href="/guides/consuming-gat-assets#local-modifications">target</Tooltip>.
Its stable name identifies the source independently of where the files are placed.

Adding or updating a mount records a <Tooltip tip="The imported paths and content IDs selected from the source repository. Recording the snapshot does not download its file bytes." cta="Pin an upstream version" href="/guides/consuming-gat-assets#pin-the-upstream-version">snapshot</Tooltip>;
use {{command:pull}} to restore its working files. Follow
[Consuming Gat assets](/guides/consuming-gat-assets#mount-and-use-upstream-data)
for the import, pull, and commit sequence.

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
  storage setup. Configure storage with {{command:remote}} and {{command:route}}.
</Accordion>

<Note>
[Routes and remotes](/guides/using-multiple-remotes#remote-selection-at-a-glance)
are independent configuration. Moving or removing a mount does not move or remove them.
</Note>
