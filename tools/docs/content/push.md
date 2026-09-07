Upload objects needed by the current lock. Explicit history selectors replace
that set with objects from selected committed snapshots. Path filters apply
before content is <Tooltip tip="Selected paths with identical content IDs need only one object within the applicable storage destination. Different filenames or revisions can reference the same bytes." cta="Content identity" href="/concepts/how-gat-works#content-identity-is-independent-of-path">deduplicated</Tooltip>.

A broad push skips <Tooltip tip="Files imported from another repository. Their source repository publishes the objects; a broad push from the consuming repository does not upload them." cta="Publish and consume mounted assets" href="/guides/consuming-gat-assets">mount-owned paths</Tooltip> and summarizes each skipped mount.
Cache problems are reported per path.

<Tip>
Run `gat push` before `git push` so collaborators can fetch the bytes referenced
by your commits. See [History selection](/concepts/history-selection) to publish
objects from earlier snapshots too.
</Tip>
