Fetch downloads selected objects into the <Tooltip tip="File bytes stored locally by content ID, ready for sync to restore into the working tree." cta="Cache and working files" href="/concepts/how-gat-works#what-lives-where">local cache</Tooltip> without changing working files.
Without history flags it reads the current lock; explicit history selectors use
selected committed snapshots instead. Fetch includes selected mount-owned paths.
See [fetch, sync, and pull](/concepts/how-gat-works#fetch-sync-and-pull) to choose an operation,
or [history selection](/concepts/history-selection) to prefetch older versions.
