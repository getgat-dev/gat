# Repository checks

`gat-check` is a developer-only Rust binary independent of the application.
Its three direct dependencies are `syn`, `proc-macro2` (source locations), and
`serde_json` (Cargo metadata), all already in the workspace lockfile.

```sh
cargo run --quiet --locked -p gat-check -- all
cargo test --locked -p gat-check
```

`task check` runs both, followed by installer and application tests.

## Commands

| Command | Checks |
| --- | --- |
| `architecture` | Dependency direction, shared MSRV, developer-tool isolation, environment acquisition/mutation, IO ownership, remote opening, root error rendering, production debug output |
| `test-hygiene` | Environment/working-directory mutation, unexplained sync/async sleeps, direct IO calls with shared paths or fixed/public endpoints |
| `all` | Both groups, parsing each source file once |
| `release BINARY TARGET MAX_GLIBC` | Linux ELF architecture/format, GNU glibc symbol ceiling, or musl static linkage |

Findings have stable rule IDs and file/line locations. Output is sorted and
deduplicated. Exit codes are 0 for success, 1 for policy failure, and 2
when the checker cannot run (arguments, parsing, Cargo, missing utilities, or IO).
Manifest findings point to line 1 of the relevant manifest.

## Source policy

The rules inspect syntax: grouped/renamed imports, block-local imports, inline
modules, explicit test gates, and expression arguments in macros. Comments and
literals are not API references. Cargo supplies dependency metadata, including
renamed, target-specific, optional, and build dependencies; dev-dependencies are
handled separately. Every package must declare the same MSRV as the application.

Hygiene checks operations, not every URL/path-shaped string. Filesystem calls
check path arguments (including both paths for copy/rename/hard-link), not
write payloads. Parser test data
needs no waiver. Direct filesystem, remote-opening, or bind calls with literal
shared paths or fixed endpoints are checked; IPv4/IPv6 loopback port zero is
allowed. Wildcard and public bind addresses are rejected, even at port zero.
Async filesystem APIs follow the same ownership and isolation rules as sync APIs.
Engine and command code must spawn child processes through IO capabilities.
Process-wide working-directory mutation is forbidden; child-process
`current_dir` configuration is allowed. Production `dbg!` output is rejected.
Explain intentional elapsed-time tests with `// sleep-ok:` and deliberate IO
boundary cases with `// hygiene-ok:`. A nonempty reason must appear within the
preceding four lines. These are review aids, not a security boundary.

This is not a Rust compiler or network sandbox. It does not resolve arbitrary
re-exports, local variable/type shadowing, method receiver types, computed paths,
external-module cfg inheritance, or macro-generated code. Unrecognized macro DSLs
are skipped. Crate dependencies and opaque APIs remain the primary enforcement;
these checks catch common regressions. Fixture isolation still needs review.

Rust files under product, test-support, test, and tool roots are inspected.
Symlinks/build directories are skipped; unreadable or malformed source fails the
run. Platform-specific production code is inspected on every host.

## Release and installers

Release inspection uses the host's `readelf` with a fixed locale. Rust fixtures
cover policy decisions; release CI validates the exact packaged executable.
Docker baseline-runtime acceptance remains in `tools/check/test-linux-release.sh`.

Installer fixtures run directly in their native shells through Task and CI:

```sh
bash tools/check/test-installers.sh /bin/dash
```

On Windows, run `./tools/check/test-installers.ps1` in PowerShell. The CI matrix
runs it in both Windows PowerShell 5.1 and PowerShell 7. `task test:install`
selects Bash on Unix and PowerShell 7 on Windows. Rust does not launch the scripts;
installer jobs need no Rust toolchain. The Unix fixture requires Bash and its
fixture utilities; its optional argument selects the shell running the installer.

## Adding a rule

Add a rule when it catches a likely regression with a useful action. Give it a
stable ID, a positive fixture, and a false-positive fixture. Prefer operations and
ownership to unrelated literals. Avoid recreating compiler type checking, Cargo's
manifest parser, or executable parsers.
