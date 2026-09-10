pub struct CommandDoc {
    pub path: &'static str,
    pub overview: &'static str,
    pub examples: &'static [CommandExample],
}

pub struct CommandExample {
    pub title: &'static str,
    pub argv: &'static [&'static str],
    pub explanation: &'static str,
}

macro_rules! example {
    ($title:literal, [$($arg:literal),+ $(,)?], $explanation:literal) => {
        CommandExample {
            title: $title,
            argv: &[$($arg),+],
            explanation: $explanation,
        }
    };
}

macro_rules! command {
    ($path:literal, $overview:expr, [$($example:expr),+ $(,)?]) => {
        CommandDoc {
            path: $path,
            overview: $overview,
            examples: &[$($example),+],
        }
    };
}

pub const COMMAND_DOCS: &[CommandDoc] = &[
    CommandDoc {
        path: "selection",
        overview: include_str!("content/selection.md"),
        examples: &[],
    },
    command!(
        "selection list",
        "List effective saved selections and mark the default.",
        [example!(
            "List selections",
            ["selection", "list"],
            "Lists available definitions without changing configuration."
        )]
    ),
    command!(
        "selection show",
        "Show the saved path, patterns, and defining scope.",
        [example!(
            "Inspect a selection",
            ["selection", "show", "runtime"],
            "Reads the effective runtime definition."
        )]
    ),
    command!(
        "selection add",
        "Save a complete definition. Adding a selection never chooses a default.",
        [example!(
            "Save model selection",
            [
                "selection",
                "add",
                "runtime",
                "--path",
                "models",
                "--include",
                "**/*.onnx",
                "--exclude",
                "experimental/**"
            ],
            "Patterns are relative to models."
        )]
    ),
    command!(
        "selection update",
        "Update a definition in its existing scope. Omitted fields are preserved; supplied lists replace the saved lists.",
        [example!(
            "Clear exclusions",
            ["selection", "update", "runtime", "--clear-exclude"],
            "Keeps the saved path and includes."
        )]
    ),
    command!(
        "selection remove",
        "Remove a definition in the selected scope. A lower-priority definition may be revealed. Removal cannot leave the effective default dangling.",
        [example!(
            "Remove a local override",
            ["selection", "remove", "runtime", "--local"],
            "Reveals a project or global runtime definition when present."
        )]
    ),
    command!(
        "selection default",
        "Show or choose the optional default. --unset removes this scope's pointer and restores inheritance. It does not select everything when a lower layer has a default.",
        [
            example!(
                "Choose default",
                ["selection", "default", "runtime", "--local"],
                "Uses the shared definition without copying its filters."
            ),
            example!(
                "Unset",
                ["selection", "default", "--local", "--unset"],
                "Removes only the local pointer."
            )
        ]
    ),
    command!(
        "remote default",
        "Show or choose the optional default remote. Adding a remote never chooses a default. --unset restores inheritance from lower layers.",
        [example!(
            "Choose a default remote",
            ["remote", "default", "origin"],
            "Selects a previously configured remote."
        )]
    ),
    command!(
        "init",
        include_str!("content/init.md"),
        [
            example!(
                "Default setup",
                ["init"],
                "Install Gat's normal Git integration and initialize the local cache."
            ),
            example!(
                "Example config",
                ["init", "--example-config"],
                "Scaffold a commented `gat.yaml` when one does not already exist."
            ),
            example!(
                "No hooks",
                ["init", "--no-hooks"],
                "Keep the merge driver while explicitly removing Gat-managed hooks."
            ),
        ]
    ),
    command!(
        "add",
        include_str!("content/add.md"),
        [
            example!(
                "One file",
                ["add", "data/model.bin"],
                "Add one large file to `gat.lock` and the local cache."
            ),
            example!(
                "Directory",
                ["add", "data/"],
                "Recursively add eligible files and summarize exclusions."
            ),
            example!(
                "Glob pattern",
                ["add", "data/**/*.bin"],
                "Add matching files at any depth."
            ),
            example!(
                "Ignored file",
                ["add", "--force", "data/model.bin"],
                "Include an ignored file without bypassing repository safety checks."
            ),
        ]
    ),
    command!(
        "rm",
        include_str!("content/rm.md"),
        [
            example!(
                "Delete file",
                ["rm", "data/model.bin"],
                "Stop tracking the file and remove its working-tree copy."
            ),
            example!(
                "Keep file",
                ["rm", "--cached", "data/model.bin"],
                "Remove only the `gat.lock` entry."
            ),
            example!(
                "Directory",
                ["rm", "data/checkpoints/"],
                "Remove all matching tracked rows recursively."
            ),
        ]
    ),
    CommandDoc {
        path: "remote",
        overview: include_str!("content/remote.md"),
        examples: &[],
    },
    command!(
        "remote list",
        "List remotes, their defining scopes, and the effective default.",
        [example!(
            "List configured remotes",
            ["remote", "list"],
            "Show configured remotes after applying scope overrides."
        ),]
    ),
    command!(
        "remote add",
        "Add a storage URL in the selected scope. Configure provider credentials before transferring objects.",
        [
            example!(
                "S3",
                [
                    "remote",
                    "add",
                    "origin",
                    "s3://my-bucket/gat?region=eu-west-1"
                ],
                "Configure S3-compatible object storage."
            ),
            example!(
                "Azure Blob",
                [
                    "remote",
                    "add",
                    "azure",
                    "azblob://my-container/gat?endpoint=https://myaccount.blob.core.windows.net"
                ],
                "Configure Azure Blob Storage."
            ),
            example!(
                "Local directory",
                ["remote", "add", "local", "file:///mnt/gat-storage"],
                "Configure storage on a local or mounted filesystem."
            ),
        ]
    ),
    command!(
        "remote show",
        "Show the redacted URL, default status, and defining scope without contacting storage.",
        [example!(
            "Inspect a remote",
            ["remote", "show", "origin"],
            "Show the redacted URL, default status, and defining scope."
        ),]
    ),
    command!(
        "remote update",
        "Change a saved URL in its defining scope. Omit `--url` to preserve it.",
        [example!(
            "Change a remote URL",
            [
                "remote",
                "update",
                "origin",
                "--url",
                "s3://new-bucket/gat?region=eu-west-1"
            ],
            "Update one named endpoint."
        ),]
    ),
    command!(
        "remote remove",
        "Remove a remote definition. Change or unset references to it first.",
        [example!(
            "Remove a remote",
            ["remote", "remove", "archive"],
            "Delete the named remote. `gat remote rm archive` is also accepted."
        ),]
    ),
    command!(
        "config",
        include_str!("content/config.md"),
        [
            example!(
                "Read",
                ["config", "cache.location"],
                "Print the value and its source after applying overrides."
            ),
            example!(
                "Set value",
                ["config", "cache.location", "/mnt/gat-cache"],
                "Write a scalar setting to the selected scope."
            ),
            example!(
                "Set list",
                ["config", "git.ignore_patterns", "*.bin", "*.onnx"],
                "Replace a list with one argument per element."
            ),
            example!(
                "Clear list",
                ["config", "git.ignore_patterns", "--clear"],
                "Persist an explicit empty list."
            ),
            example!(
                "Unset",
                ["config", "cache.location", "--unset"],
                "Restore inheritance or the built-in default."
            ),
        ]
    ),
    command!(
        "status",
        concat!(
            include_str!("content/status.md"),
            "\n",
            include_str!("content/selection-defaults.md")
        ),
        [
            example!(
                "Complete output",
                ["status", "-o"],
                "Show every row with complete paths and metadata."
            ),
            example!(
                "Local",
                ["status"],
                "Compare tracked files with staged desired state."
            ),
            example!(
                "Directory",
                ["status", "--path", "data/"],
                "Inspect one tracked subtree."
            ),
            example!(
                "Glob pattern",
                ["status", "--include", "**/*.bin"],
                "Inspect matching tracked paths."
            ),
            example!(
                "Remote",
                ["status", "--remote"],
                "Verify each path on its resolved remote."
            ),
            example!(
                "History",
                ["status", "--remote", "backup", "--rev", "HEAD~5"],
                "Verify one historical revision on a named remote."
            ),
        ]
    ),
    command!(
        "diff",
        concat!(
            include_str!("content/diff.md"),
            "\n",
            include_str!("content/selection-defaults.md")
        ),
        [
            example!(
                "Against HEAD",
                ["diff"],
                "Compare the working desired state with `HEAD`."
            ),
            example!(
                "One revision",
                ["diff", "HEAD~1"],
                "Compare a revision with the working desired state."
            ),
            example!(
                "Two revisions",
                ["diff", "v1.0", "v2.0", "--path", "data/"],
                "Restrict a revision-to-revision comparison to a subtree."
            ),
        ]
    ),
    command!(
        "ls-files",
        concat!(
            include_str!("content/ls-files.md"),
            "\n",
            include_str!("content/selection-defaults.md")
        ),
        [
            example!(
                "Default selection",
                ["ls-files"],
                "List paths in the current selection."
            ),
            example!(
                "Directory",
                ["ls-files", "--path", "data/"],
                "List one tracked subtree."
            ),
            example!(
                "Glob pattern",
                ["ls-files", "--include", "**/*.onnx"],
                "List matching tracked paths."
            ),
            example!(
                "All paths",
                ["ls-files", "--path", "."],
                "Replace the complete configured selection with an explicit repository-root selection."
            ),
        ]
    ),
    command!(
        "push",
        concat!(
            include_str!("content/push.md"),
            "\n",
            include_str!("content/selection-defaults.md")
        ),
        [
            example!(
                "Current files",
                ["push"],
                "Upload objects referenced by the current desired state."
            ),
            example!(
                "Directory",
                ["push", "--path", "data/", "--remote", "backup"],
                "Upload one subtree to a named remote."
            ),
            example!(
                "History",
                ["push", "--rev", "v1.0"],
                "Upload objects referenced by one committed revision."
            ),
        ]
    ),
    command!(
        "fetch",
        concat!(
            include_str!("content/fetch.md"),
            "\n",
            include_str!("content/selection-defaults.md")
        ),
        [
            example!(
                "Current files",
                ["fetch"],
                "Download objects required by the current desired state."
            ),
            example!(
                "Directory",
                ["fetch", "--path", "data/", "--remote", "backup"],
                "Download one subtree from a named remote."
            ),
            example!(
                "Tags",
                ["fetch", "--tags"],
                "Download objects referenced by tag tips."
            ),
        ]
    ),
    command!(
        "pull",
        concat!(
            include_str!("content/pull.md"),
            "\n",
            include_str!("content/selection-defaults.md")
        ),
        [
            example!(
                "Current files",
                ["pull"],
                "Fetch missing objects and reconcile the working tree."
            ),
            example!(
                "Directory",
                ["pull", "--path", "data/", "--remote", "backup"],
                "Restrict both fetch and materialization."
            ),
            example!(
                "History",
                ["pull", "--rev", "v1.0"],
                "Also fetch a historical selection while reconciling current state."
            ),
        ]
    ),
    command!(
        "gc",
        include_str!("content/gc.md"),
        [
            example!(
                "No history",
                ["gc", "--no-history", "--dry-run"],
                "Retain the current working lock and peer protections without consulting this repository’s history."
            ),
            example!(
                "Remote",
                [
                    "gc",
                    "--remote",
                    "backup",
                    "--dry-run",
                    "--repository",
                    "../other-project"
                ],
                "Inspect a named remote without deleting objects."
            ),
            example!(
                "Branches",
                ["gc", "--branches", "--ancestors", "--dry-run"],
                "Build the keep set from all branch histories."
            ),
        ]
    ),
    command!(
        "sync",
        concat!(
            include_str!("content/sync.md"),
            "\n",
            include_str!("content/selection-defaults.md")
        ),
        [
            example!(
                "Default selection",
                ["sync"],
                "Materialize the current desired state."
            ),
            example!(
                "Directory",
                ["sync", "--path", "data/"],
                "Restrict reconciliation to one subtree."
            ),
            example!(
                "Fetch",
                ["sync", "--fetch", "--remote", "backup"],
                "Fetch missing objects before reconciliation."
            ),
            example!(
                "Rematerialize",
                ["sync", "--rematerialize", "--dry-run"],
                "Show which clean files would be recreated."
            ),
            example!(
                "Glob pattern",
                ["sync", "--include", "**/*.onnx", "--exclude", "tests/**"],
                "Reconcile selected model files while excluding tests."
            ),
        ]
    ),
    CommandDoc {
        path: "system",
        overview: include_str!("content/system.md"),
        examples: &[],
    },
    command!(
        "system inspect",
        "Check repository state without changing it. Omit the domain to inspect all domains.",
        [
            example!(
                "All domains",
                ["system", "inspect"],
                "Inspect lock, state, cache, and Git integration."
            ),
            example!(
                "Cache",
                ["system", "inspect", "cache"],
                "Restrict inspection to the cache domain."
            ),
        ]
    ),
    command!(
        "system repair",
        "Repair the selected repository domain. Review inspection results before choosing recovery options.",
        [
            example!(
                "State",
                ["system", "repair", "state"],
                "Rebuild derived repository state."
            ),
            example!(
                "Lock backup",
                ["system", "repair", "lock", "--restore-backup"],
                "Choose the validated backup during explicit lock recovery."
            ),
        ]
    ),
    command!(
        "system clean",
        "Remove disposable state. Purge flags also remove unverified temporary files or cached objects.",
        [
            example!(
                "Disposable state",
                ["system", "clean"],
                "Remove only state Gat can prove is disposable."
            ),
            example!(
                "Temporary files",
                ["system", "clean", "cache", "--purge-temporary"],
                "Explicitly remove unverified ingest scratch files."
            ),
            example!(
                "Cached objects",
                ["system", "clean", "cache", "--purge-objects"],
                "<Warning>This deletes cached objects, including unpublished content. Upload or back up needed objects first.</Warning>"
            ),
        ]
    ),
    command!(
        "mv",
        include_str!("content/mv.md"),
        [
            example!(
                "Rename",
                ["mv", "data/model.bin", "models/model.bin"],
                "Move the file and update its tracked path."
            ),
            example!(
                "Overwrite",
                ["mv", "--force", "old.bin", "new.bin"],
                "<Warning>This overwrites the destination file. A rollback does not restore its previous contents.</Warning>"
            ),
        ]
    ),
    CommandDoc {
        path: "mount",
        overview: include_str!("content/mount.md"),
        examples: &[],
    },
    command!(
        "mount list",
        "List effective mounts and their targets.",
        [example!(
            "List mounts",
            ["mount", "list"],
            "Show every effective mount."
        ),]
    ),
    command!(
        "mount add",
        "Import a committed source snapshot under a target directory. Storage setup is automatic unless `--no-setup` is supplied.",
        [
            example!(
                "No setup",
                ["mount", "add", "resnet", "../models", "--no-setup"],
                "Import mount metadata without changing remotes or routes."
            ),
            example!(
                "Local source",
                ["mount", "add", "resnet", "../models", "releases/resnet"],
                "Import rows from another repository."
            ),
            example!(
                "Source filters",
                [
                    "mount",
                    "add",
                    "resnet",
                    "../models",
                    "releases/resnet",
                    "--path",
                    "exports",
                    "--include",
                    "**/*.onnx",
                    "--rev",
                    "main"
                ],
                "Choose a source subtree, glob, and revision."
            ),
        ]
    ),
    command!(
        "mount show",
        "Show the source, pinned revision, filters, target, owned path count, effective storage route, and defining scope.",
        [example!(
            "Show a mount",
            ["mount", "show", "resnet"],
            "Inspect source, target, ownership, revision, and routing details."
        ),]
    ),
    command!(
        "mount update",
        "Refresh the source snapshot. Omitted settings are preserved; supplied filter lists replace the saved lists.",
        [
            example!(
                "No setup",
                ["mount", "update", "resnet", "--no-setup"],
                "Refresh the mount while leaving remotes and routes unchanged."
            ),
            example!(
                "Revision",
                [
                    "mount", "update", "resnet", "--rev", "v2", "--path", "exports"
                ],
                "Refresh a mount from a new revision and source path."
            ),
            example!(
                "Filters",
                [
                    "mount",
                    "update",
                    "resnet",
                    "--include",
                    "**/*.onnx",
                    "--exclude",
                    "tests/**"
                ],
                "Replace the mount's include and exclude lists."
            ),
        ]
    ),
    command!(
        "mount remove",
        "Remove a mount and its imported tracking entries. Use `--detach-only` to retain the entries under local ownership. Routes and remotes are kept.",
        [
            example!(
                "Remove",
                ["mount", "remove", "resnet"],
                "Remove the definition and its owned rows."
            ),
            example!(
                "Detach",
                ["mount", "remove", "resnet", "--detach-only"],
                "Keep imported rows as root-owned paths."
            ),
        ]
    ),
    CommandDoc {
        path: "route",
        overview: include_str!("content/route.md"),
        examples: &[],
    },
    command!(
        "route list",
        "List effective routes. The synthetic `*` row shows the default remote fallback.",
        [example!(
            "List routes",
            ["route", "list"],
            "Show named routes and the default fallback."
        ),]
    ),
    command!(
        "route add",
        "Route a path and its descendants to an existing remote. The most-specific matching path wins.",
        [example!(
            "Route a subtree",
            ["route", "add", "datasets", "backup", "data/datasets"],
            "Send a tracked subtree to a named remote."
        ),]
    ),
    command!(
        "route show",
        "Show the route path, remote, and defining scope.",
        [example!(
            "Show a route",
            ["route", "show", "datasets"],
            "Inspect the route's normalized path, remote, and defining scope."
        ),]
    ),
    command!(
        "route update",
        "Change the path or remote in the defining scope. Omitted settings are preserved.",
        [example!(
            "Change a route",
            [
                "route", "update", "datasets", "--remote", "archive", "--path", "datasets"
            ],
            "Update route properties without changing its stable name."
        ),]
    ),
    command!(
        "route remove",
        "Remove a storage rule. Tracked files, mounts, and remote definitions are kept.",
        [example!(
            "Remove a route",
            ["route", "remove", "datasets"],
            "Delete the route definition. `gat route rm datasets` is also accepted."
        ),]
    ),
];
