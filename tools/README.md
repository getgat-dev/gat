# Developer tools

These independent workspace crates are excluded from default builds:

| Crate | Purpose | Command |
| --- | --- | --- |
| `gat-docs` | Generate and validate documentation from the application CLI and configuration | `task docs:generate`, `task docs:validate` |
| `gat-bench` | Generate benchmark repositories without building the application | `task benchmark:generate` |
| `gat-check` | Repository policy checks, release inspection, and native installer fixtures | `task check` |

Documentation source and authored content live in `docs/`; benchmark generation
lives in `benchmark/`. Check scripts and their runtime Dockerfile live beside the
[checker](check/README.md). Benchmark and checker crates have no application
crate dependencies. All three inherit workspace lints and the application MSRV.

`task install:dev-tools` installs the application and both generators. The checker
runs from its repository checkout through Cargo.
