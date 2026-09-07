# gat

Simple, fast, versioned large-file storage for git.

`gat` is what `git-lfs` would be if it didn't need a special server, and what
`dvc` would be if it did one thing. Point it at S3, Azure Blob, GCS, and
other object stores (or a plain directory), `gat add` your large files or
directories, and keep using `git` exactly as before — `commit`, `push`,
`pull`, `clone` all just work. No clean/smudge filter magic: `gat sync`
reconciles the working tree with the committed `gat.lock` file or shard directory, and
Git hooks (installed by `gat init`) run it automatically after checkout,
merge/pull, and rebase/amend.

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
and puts `gat` on your `PATH`. See [Installation](docs/installation.mdx)
for version-pinned install-script URLs, manual downloads, and Cargo-based
source installs.

Alternatively, install from source with Cargo:

```bash
cargo install --locked --bin gat --git https://github.com/getgat-dev/gat gat
```

`gat` requires Rust 1.91 or newer (the minimum supported Rust version, or
MSRV, tracked by `rust-version` in `Cargo.toml`). Bumping the MSRV is
considered a breaking change.

## Quick example

```sh
gat init
gat remote add origin s3://my-bucket/assets
gat add models/encoder.safetensors
git add gat.lock gat.yaml
git commit -m "Track encoder weights with gat"
gat push
```

## Docs

Open the documentation at **[getgat.dev](https://getgat.dev/)**.

- [Quickstart](docs/quickstart.mdx) — track, push, and pull your first file
- [Installation](docs/installation.mdx) — prerequisites and setup
- [Set up a remote](docs/set-up-a-remote.mdx) — prerequisites and setup
- [Commands](docs/commands) — reference, one page per subcommand

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for dev setup and PR conventions.
