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

## Issues

Use the repository's
[issue templates](https://github.com/getgat-dev/gat/issues/new/choose) to
report bugs or request features.
