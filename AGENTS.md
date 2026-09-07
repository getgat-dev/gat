# AGENTS.md

Follow `CONTRIBUTING.md`.

* Run `task check` before finishing if `task` is available; otherwise run:
  * `cargo fmt --all -- --check`
  * `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
  * `bash tools/check-test-hygiene.sh --self-test` and `bash tools/check-test-hygiene.sh`
  * `bash tools/check-error-architecture.sh --self-test` and `bash tools/check-error-architecture.sh`
  * `bash tools/check-workspace-boundary.sh --self-test` and `bash tools/check-workspace-boundary.sh`
  * `bash tools/test-installers.sh`
  * `cargo nextest run --all-features --locked` if `cargo-nextest` is available; otherwise `cargo test --all-features --locked`
  * `cargo test --doc --all-features --locked` when using nextest (the `cargo test` fallback already includes doctests)
* Run `task docs:generate` after CLI/config changes if `task` is available; otherwise run:
  * `cargo run --quiet --locked -p gat-tools --bin generate-docs -- --write`
* Validate CLI/config documentation with `task docs:validate`; without `task`, run:
  * `cargo run --quiet --locked -p gat-tools --bin generate-docs -- --check`
  * `npm --prefix docs ci`
  * `npm --prefix docs run check`
* Preserve cross-platform support and the `Cargo.toml` MSRV

Prefer the cleanest design over preserving existing formats, APIs, or behavior.
