# gat

Simple, fast, versioned large-file storage for git.

`gat` is what `git-lfs` would be if it didn't need a special server, and what
`dvc` would be if it did one thing. Point it at S3, Azure Blob, GCS, and
other object stores (or a plain directory), `gat add` your large files or
directories, and keep using `git` exactly as before — `commit`, `push`,
`pull`, `clone` all just work. No clean/smudge filter magic: `gat sync`
reconciles the working tree with the current `gat.lock` file or shard directory.
[Git hooks](docs/concepts/automatic-sync.mdx) installed by `gat init` run sync
after checkout, merge/pull, and rebase/amend. Each clone needs `gat init`;
use `gat pull` to download content missing from its cache.

## Install

**Linux / macOS**

```bash
curl -fsSL https://getgat.dev/install.sh | sh
```

**Windows (PowerShell)**

```powershell
& ([scriptblock]::Create((irm https://getgat.dev/install.ps1)))
```

This downloads the latest matching release archive, verifies its checksum,
and installs `gat` to your user-local bin directory. Add that directory to
your `PATH` if prompted. See [Installation](docs/installation.mdx)
for version-pinned installs, manual downloads, and Cargo-based
source installs.

Alternatively, install from source with Cargo:

```bash
cargo install --locked --bin gat --git https://github.com/getgat-dev/gat gat
```

Building `gat` from source requires Rust 1.91 or newer (the minimum supported
Rust version, or MSRV, tracked by `rust-version` in `Cargo.toml`). Bumping the
MSRV is considered a breaking change.

## Quick example

Run inside an existing Git repository, using a file Git does not already track.
Replace the bucket and region and configure
[storage credentials](docs/references/remote-providers.mdx).

```sh
gat init
gat remote add origin 's3://my-bucket/assets?region=eu-west-1'
gat add models/encoder.safetensors
git add gat.lock gat.yaml
git commit -m "Track encoder weights with gat"
gat push --remote origin
```

Adding a remote does not select a default. Use `--remote origin` for each
transfer, or run `gat remote default origin`. See
[remote setup](docs/set-up-a-remote.mdx) for sharing storage configuration and
[How Gat works](docs/concepts/how-gat-works.mdx) for the lock, cache, and working files.

## Docs

Open the documentation at **[getgat.dev](https://getgat.dev/)**.

- [Quickstart](docs/quickstart.mdx) — track, push, and pull your first file
- [Installation](docs/installation.mdx) — prerequisites and setup
- [Set up a remote](docs/set-up-a-remote.mdx) — connect storage and choose a default
- [Gat MCP server](docs/guides/use-gat-mcp.mdx) — connect your AI tool to the docs
- [Commands](docs/commands) — reference, one page per subcommand

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for dev setup and PR conventions.
