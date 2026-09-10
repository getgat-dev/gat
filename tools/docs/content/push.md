Upload objects needed by the current lock. Explicit history selectors replace
that set with objects from selected committed snapshots. Path filters apply
before content is <Tooltip tip="Selected paths with identical content IDs need only one object within the applicable storage destination. Different filenames or revisions can reference the same bytes." cta="Content identity" href="/concepts/how-gat-works#content-identity-is-independent-of-path">deduplicated</Tooltip>.

A broad push skips [mount-owned paths](/guides/consuming-gat-assets) and summarizes
each skipped mount. Cache problems are reported per path.

<Accordion title="Upload imported content explicitly">
  A selection rooted at or inside a mount can upload its objects.
  `--path .` still skips mounts; selecting a mount's path opts it in.
  See [mount ownership](/guides/consuming-gat-assets#local-modifications)
  before publishing imported content.
</Accordion>

Choose a [default remote](/set-up-a-remote#select-the-default), configure
[routes](/guides/using-multiple-remotes#remote-selection-at-a-glance), or pass
`--remote NAME`. Adding a named remote alone does not select it for transfers.

<Tip>
Run `gat push` before `git push` so collaborators can fetch the bytes referenced
by your commits. See [History selection](/concepts/history-selection) to publish
objects from earlier snapshots too.
</Tip>
