Compare tracked paths and content IDs across <Tooltip tip="Git commit identifiers such as a branch, tag, commit hash, or HEAD~2. Gat reads the lock at each selected commit to compare file versions." cta="Resolve ambiguous names" href="/concepts/history-selection#when-a-branch-and-tag-share-a-name">revisions</Tooltip>, independent of the lock's
text layout. Mount-owned entries show their mount name.

| Revisions supplied | Comparison |
| --- | --- |
| None | `HEAD` against the current lock. |
| One | That revision against the current lock. |
| Two | The two committed snapshots. |

To compare the working lock with Git’s staged lock, use {{command:status}}. For the commit and upload sequence,
see [Branching and merging](/guides/branching-and-merging#typical-workflows).
