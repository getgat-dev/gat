# Benchmarking

This repository keeps benchmark infrastructure, not a catalog of committed
performance scenarios. Performance questions are specific to a change,
dataset, machine, and hypothesis; committed scenarios tracked internal
refactors closely and made stale workloads look authoritative.

The maintained helpers are:

- [benchmark_support.rs](benchmark_support.rs): hermetic repository and remote fixtures plus
  deterministic desired-state, cache, and worktree data builders. It is a
  harness-free Cargo bench target so all-target checks compile the helper API;
  it performs no measurements.
- [tools/generate-benchmark.rs](../tools/generate-benchmark.rs) (`task benchmark:generate`): creates a
  realistic on-disk repository for manually timing the real `gat` CLI.

Run commands below from the repository root. For configuration tradeoffs to
measure, see [Improving performance](../docs/guides/improving-performance.mdx);
follow the [test isolation rules](../CONTRIBUTING.md#tests) when building fixtures.

## Create a local Criterion benchmark

Create a temporary `benches/scratch.rs` and add a temporary harness-free target
to `Cargo.toml`:

```toml
[[bench]]
name = "scratch"
harness = false
```

The scenario can then import the maintained helpers:

```rust
mod benchmark_support;

use benchmark_support::{RepoFixture, synthetic_entries};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use gat_core::lock::LockShardLevels;
use std::hint::black_box;

fn benchmark(c: &mut Criterion) {
    c.bench_function("load_100_desired_entries", |b| {
        b.iter_batched(
            || {
                let fixture = RepoFixture::gat();
                fixture.seed_desired(
                    &synthetic_entries(100),
                    LockShardLevels::FLAT,
                );
                fixture
            },
            |fixture| black_box(fixture.load_lock()),
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
```

Run a short exploratory sweep with:

```sh
cargo bench --bench scratch -- --measurement-time 1 --warm-up-time 1 --sample-size 10
```

Delete `benches/scratch.rs` and its `[[bench]]` entry when the investigation is
complete. Auto-discovered benches use Cargo's default libtest harness, which
does not run Criterion's harness or accept its command-line options. Scenario
choices and generated reports belong in the issue, PR, or local investigation
that interprets them, not in the repository.

## Fixture patterns

Use `RepoFixture::git()` when an operation only needs an isolated Git
repository, and `RepoFixture::gat()` when it needs Gat initialization. The
`*_with_initial_commit()` variants support operations requiring a resolvable
`HEAD`.

Use `synthetic_entries(n)` for large background state that the measured path
will not validate against real content. Entries have deterministic nested paths
and distinct synthetic OIDs. When production code must read or hash content,
use `RepoFixture::write_ingested`, `synthetic_path`, and
`deterministic_bytes`.

Use `seed_desired`, `seed_materialized`, or `seed_clean_state` to keep fixture
construction outside the timed closure. `load_lock` and `save_lock` support
resetting mutating scenarios in `iter_batched` setup. `FileRemote` provides an
owned `file://` remote without network variability.

## Benchmarking rules

Measure a public production capability, not an implementation detail exposed
only for benchmarking. Test-only access is appropriate for fixture setup, such
as recording materialized state directly, but not for the timed operation.

Keep setup outside the timed closure. Rebuild or reset mutable state in an
unmeasured `iter_batched` setup closure. Choose synthetic versus real data
deliberately.

Never cite a result without recording the commit, hardware, OS, Rust toolchain,
profile, dataset shape and size, relevant configuration and environment, cache
state, exact command, run count, and summary method. Criterion reports and
investigation notes remain local or are attached to the issue or PR; generated
benchmark results are not committed.
