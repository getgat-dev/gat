# Contributing to gat

Thanks for contributing!

## Get started

Install [`task`](https://taskfile.dev) and
[`cargo-nextest`](https://nexte.st), then:

```sh
git clone https://github.com/getgat-dev/gat
cd gat
task build
task test
```

If `task` is unavailable, use `Taskfile.yml` as the command reference. If
`cargo-nextest` is unavailable, use `cargo test --all-features --locked`.

Common commands:

```sh
task build          # debug build
task run -- status  # run gat with arguments
task fmt            # format
task lint           # clippy with warnings denied
task test           # test suite
task check          # formatting, linting, and tests
task docs:generate  # regenerate CLI and configuration docs
task docs:validate  # verify generated docs and the docs site
```

## Make a change

1. Fork the repository and create a branch from `main`.
2. Make a focused change and add tests for new or changed behavior.
3. Run the smallest relevant tests while developing.
4. Run `task check` before opening a pull request.
5. Explain what changed and why in the pull request.

Prefix commit messages and pull request titles with a
[gitmoji](https://gitmoji.dev/), for example:
`:sparkles: add remote pruning`. Pull requests are squash-merged, so the pull
request title becomes the commit message on `main`.

## Generated documentation

Do not edit `docs/commands/*.mdx` or
`docs/references/configuration.mdx` by hand.

When changing a public command:

1. Update its clap definition and document every user-facing argument.
2. Update its `CommandDoc`, parse-valid example, and source Markdown under
   `tools/docs/`.
3. Run `task docs:generate`, `task docs:validate`, and `task check`.

When changing persisted configuration, update the typed `Config` model,
semantic validation, `gat-core/src/config_keys.rs`, and the `gat config`
handler when the key is directly settable. Then run the same three tasks.

Checked command and argument references intentionally fail generation when a
referenced CLI surface changes. Update those references and lifecycle
metadata with the implementation.

## Architecture boundaries

The production dependency direction is:

```text
gat-core <- gat-io <- gat-engine <- gat-command <- gat
```

The root crate may also depend directly on `gat-core` and `gat-engine`.
Keep implementation modules private unless they form a deliberate semantic
namespace or opaque capability facade. Do not expose storage, repository, or
locking representations as convenience APIs.

When changing workspace manifests or cross-layer behavior, run:

```sh
task lint:workspace-boundary:selftest lint:workspace-boundary
```

The boundary policy also requires that engine code avoid buffered file I/O
and ambient environment lookup, remote clients be opened by the
operation-scoped session, and command code avoid physical filesystem work.

## Errors and user-visible output

Subsystems return typed errors with structured fields. Only
`src/error/map/**` converts them into fatal `Failure`/`Diagnostic` values or
non-fatal `UserProblem` values. Lower layers must not import or construct
those presentation types.

`gat-command` owns no user-facing wording. Error wording belongs in
`error::map`, while success, progress, partial-outcome, and lifecycle wording
belongs in `output`. Human-readable output must remain typed as
`presentation::UserLine` until rendering. Machine-readable output must come
from a specific validated or redacted domain representation.

Never create user-visible text by formatting an error, calling
`.to_string()` on it, or walking its source chain. Match the typed error
variant and build safe wording from its fields. Use the appropriate
`UserLine` constructor for dynamic paths, identifiers, keys, object IDs, and
URLs. Add an adversarial sentinel test whenever a new low-level error source
must be mapped.

## Tests

- Put focused tests for private logic beside the code in an inline
  `#[cfg(test)] mod tests`.
- Put public API and cross-component workflow tests in the owning crate's
  `tests/` directory.
- Keep `tests/app_dispatch.rs` limited to dispatch and policy.
- Use spawned-binary tests only for real process, Git, environment, stream,
  exit-code, or OS boundaries.

Keep tests deterministic and isolated:

- Do not use arbitrary sleeps, wall-clock performance assertions, or retries
  to hide flakes.
- Do not mutate the process environment in parallel unit tests; inject values
  or set them only on a spawned child process.
- Use `file://` remotes instead of real network services.
- Prefer fixture-owned or thread-local test state over mutable process
  globals.
- Keep shared integration fixtures in `tests/common`, `test-support-git`, or
  `test-support-gat` as appropriate.

`task lint:test-hygiene` enforces these rules across the workspace.

## Code and documentation

Follow existing Rust style and preserve cross-platform behavior and the
`Cargo.toml` MSRV. Every workspace package must inherit the shared lint
policy from the root `Cargo.toml` with `[lints] workspace = true`.
`task lint` checks all workspace packages, including developer tools.
Comments should explain non-obvious intent or invariants,
not restate code. Rustdoc should document contracts and meaningful errors,
panics, safety requirements, ownership, or side effects. Keep TODOs actionable
and record historical rationale in an ADR rather than in code comments.

## Releases

[The release workflow](.github/workflows/release.yml) builds and publishes
GitHub Release archives. `task release` only builds an optimized local binary;
it does not package, tag, or publish a release. The workflow does not publish
crates to crates.io.

### Prepare and validate

1. Choose the next version and update the root package version in `Cargo.toml`.
   Refresh `Cargo.lock` with Cargo (for example, `cargo check`) and commit both
   files. The release tag must be exactly `v` followed by that package version.
2. Run `task check` and `task docs:validate`. Run `task docs:generate` first if
   CLI or configuration documentation changed. Merge the release preparation
   through a PR and wait for the checks on the intended `main` commit.
3. Review the draft release notes. [Release Drafter](.github/release-drafter.yml)
   updates them on pushes to `main`, using PR labels to group changes and suggest
   a version. Ensure the draft's tag and title match the chosen package version;
   its suggested version does not update Cargo automatically. Leave the draft
   unpublished so the release workflow can attach validated assets.
4. Run the **Release** workflow manually on the intended commit's branch for a
   complete build and packaging rehearsal. Manual runs and matching PR runs
   validate artifacts but never publish. PR runs are triggered only by changes
   to the paths listed in the release workflow; a version-only PR therefore
   needs a manual rehearsal to exercise the release matrix.

All builds use the pinned `RELEASE_RUST_VERSION`, which preparation checks
against the root package's MSRV. Cargo builds use `--locked`. Keep the release
Rust pin aligned whenever changing the MSRV.

### Platforms and artifacts

| Platform | Targets | Build and execution |
| --- | --- | --- |
| Linux GNU | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | Zig build targeting glibc 2.28; native runners and Rocky Linux 8 acceptance tests |
| Linux musl | `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` | Static Zig build; native runners and Alpine acceptance tests |
| macOS | `x86_64-apple-darwin`, `aarch64-apple-darwin` | Cargo build on ARM64 macOS; Intel executable tested through Rosetta 2 |
| Windows | `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` | Cargo build and acceptance tests on native x86_64 and ARM64 runners |

Archives are named `gat-v<VERSION>-<TARGET>.tar.gz` on Linux/macOS and
`gat-v<VERSION>-<TARGET>.zip` on Windows. Each contains a matching top-level
folder with `gat` (or `gat.exe`), `README.md`, and `LICENSE`. The public release
includes all eight archives and a combined `SHA256SUMS` file. Per-archive
`.sha256` files are intermediate workflow artifacts.

Every target extracts its candidate archive and runs the shared
`release_artifact_` tests against that executable through `GAT_TEST_BIN`.
These check the exact package version, help output, repository initialization
and hooks, a file-remote push/pull round trip, and configuration isolation.
Linux also undergoes the ABI and baseline-runtime checks described below.

After every build succeeds, the workflow verifies the complete expected asset
inventory and checksums. It then creates and verifies GitHub provenance
attestations for every archive, checking the source commit and signing workflow.
Fork PRs skip attestation because their tokens lack permission to write it;
tag releases require the attestation job to succeed before publishing.

To rerun acceptance locally, extract the candidate archive and use an absolute
path to its executable. On Linux/macOS:

```sh
GAT_TEST_BIN=/absolute/path/to/gat cargo test --locked --test cli_integration release_artifact_
```

On Windows, in PowerShell:

```powershell
$env:GAT_TEST_BIN = 'C:\absolute\path\to\gat.exe'
try {
    cargo test --locked --test cli_integration release_artifact_
} finally {
    Remove-Item Env:GAT_TEST_BIN
}
```

### Tag and publish

After validation, tag the reviewed release commit and push that specific tag.
For example, for a package version of `0.1.0`, from the intended commit:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The tag push starts a new release run. Preparation rejects a tag/package-version
mismatch. Publication requires all builds, inventory verification, and
attestations to succeed. The final job uploads the archives and `SHA256SUMS`
and publishes the matching draft (or creates a release if no draft exists).
Do not publish the draft manually to trigger builds: the trigger is the tag
push, and the workflow refuses to modify an already public release.

After completion, confirm the GitHub Release has every expected archive and
`SHA256SUMS`, then smoke-test the pinned release with the Unix and PowerShell
installers on the supported platforms. See [installation documentation](docs/installation.mdx)
for pinned installation, manual checksum verification, and Linux libc selection.
Installer fixture tests use local assets; they do not replace this check of the
published downloads.

### Recover from a failed release

Inspect the first failed job before retrying. A transient runner, network, or
upload failure can be retried on the same tag while its release remains
unpublished. Workflow artifacts expire after seven days; rerun the builds if
needed to recreate them. A manual workflow run remains a rehearsal and cannot
complete publication for a failed tag push.

If source or workflow changes are required, fix them through a PR and prepare
a new version/tag. Keep existing release tags fixed. If a release is already
public, ship corrections as a new version; the publishing guard prevents a
rerun from replacing its assets.

### Linux release compatibility

The release workflow pins Rust to the MSRV, cargo-zigbuild, and Zig. Linux GNU
builds explicitly target glibc 2.28 on modern runners; Linux musl builds are
static. Both variants are built for x86_64 and ARM64 and tested on native
runners. macOS and Windows use the ordinary Cargo build.

`tools/check-linux-release.sh` rejects GNU binaries requiring glibc symbols
newer than 2.28 and musl binaries with an ELF interpreter or shared-library
dependencies. It also checks the ELF class, byte order, and architecture.
Its fixture tests run with `task test:install`.

After packaging, the workflow runs the `release_artifact_` acceptance tests
against the extracted executable on the runner and through
`tools/test-linux-release.sh` in a baseline runtime container: Rocky Linux 8
(glibc 2.28) for GNU, Alpine for musl. The existing Rust harness stays on the
host; every Gat process runs inside the container against shared fixture paths.
The runtime helper checks the candidate ABI before starting Docker. Only the
temporary candidate/fixture directory is mounted into the container.
To reproduce the container check locally (Docker and the Rust toolchain required):

```sh
bash tools/check-linux-release.sh /path/to/gat x86_64-unknown-linux-gnu 2.28
bash tools/test-linux-release.sh /path/to/gat x86_64-unknown-linux-gnu 2.28
```

When changing the GNU baseline, update `GLIBC_VERSION` in the workflow,
`MIN_GLIBC` in `docs/install.sh`, the GNU runtime image in
`tools/release-runtime.Dockerfile`, and the installation documentation together.
Release preparation checks that the installer and build baselines agree; runtime
acceptance checks that the container provides that same glibc version.

## Issues

Use the repository's
[issue templates](https://github.com/getgat-dev/gat/issues/new/choose) to
report bugs or request features.
