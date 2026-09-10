//! Generates a tunable benchmark repository (many small files + a few large
//! ones) for exercising/timing gat's own commands (`gat add`/`push`/`fetch`/
//! `checkout`). Run via `task benchmark:generate -- [flags]`.
//!
//! File generation runs in parallel across a rayon thread pool (tune with
//! the standard `RAYON_NUM_THREADS` env var), with `indicatif::MultiProgress`
//! showing every concurrently-generated large file's own progress bar at
//! once. File content is filled from a tiny hand-rolled splitmix64 PRNG
//! seeded per-file so files are never byte-identical — otherwise gat's own
//! content-addressed dedup would collapse every generated file into a
//! single cache object, defeating the point of a benchmark repo that needs
//! many *distinct* objects to push/fetch/checkout.
//!
//! This tool only scaffolds the fixture (and `git init`s it); it does not
//! run `gat init`/`gat add` itself.

use anyhow::{Context, Result, bail};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    about = "Generate a tunable benchmark repository (many small files + a few large ones) for timing gat's commands"
)]
struct Cli {
    /// Target directory to create. Defaults to a freshly created OS temp
    /// directory (printed once generation finishes) so re-running this
    /// tool never clutters the current working directory; pass an
    /// explicit path to generate (and reuse) a benchmark repo in a fixed
    /// location instead.
    #[arg(long)]
    path: Option<PathBuf>,

    /// Number of small files to generate.
    #[arg(long, default_value_t = 5000)]
    small_files: u64,

    /// Approximate size of each small file (e.g. `4KiB`, `512B`); actual
    /// size jitters +/-20% per file so small files aren't all identical
    /// length either.
    #[arg(long, default_value = "4KiB", value_parser = parse_size)]
    small_size: u64,

    /// Number of large files to generate.
    #[arg(long, default_value_t = 5)]
    large_files: u64,

    /// Size of each large file (e.g. `256MiB`, `1GiB`).
    #[arg(long, default_value = "256MiB", value_parser = parse_size)]
    large_size: u64,

    /// Seed for deterministic file content (defaults to the current time,
    /// so re-running without `--seed` produces a fresh benchmark repo each
    /// time; pass an explicit value for reproducible content).
    #[arg(long)]
    seed: Option<u64>,

    /// Overwrite `path` if it already exists. Only meaningful with an
    /// explicit `--path`; a freshly created temp directory (the default)
    /// never needs overwriting.
    #[arg(long)]
    force: bool,
}

/// Parse a human size like `4KiB`/`256 MB`/`1GiB`/`512` (bytes, no suffix).
/// Binary units throughout (`KB`/`KiB` both mean 1024 bytes) — stdlib only,
/// no `bytesize`/`humansize` dependency for a handful of lines of parsing.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "The rounded byte count is checked as finite, nonnegative, and below 2^64 before conversion"
)]
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split_at = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split_at);
    let num: f64 = num.parse().map_err(|_| {
        format!(
            "invalid size `{s}`: expected a number, optionally followed by B/KB/KiB/MB/MiB/GB/GiB"
        )
    })?;
    let multiplier: f64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        other => {
            return Err(format!(
                "unknown size suffix `{other}` in `{s}` (expected B/KB/KiB/MB/MiB/GB/GiB)"
            ));
        }
    };
    let bytes = (num * multiplier).round();
    if !bytes.is_finite() || !(0.0..18_446_744_073_709_551_616.0).contains(&bytes) {
        return Err(format!("size `{s}` is outside the supported byte range"));
    }
    Ok(bytes as u64)
}

/// Friendly human-readable size for progress/status lines (the inverse of
/// `parse_size`, binary units).
#[allow(
    clippy::cast_precision_loss,
    reason = "Human-readable sizes are deliberately rounded to one decimal place"
)]
fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Tiny stdlib-only PRNG (splitmix64) so generated file content is
/// deterministic given a seed but never byte-identical across files or
/// across a re-run with the same seed.
struct Splitmix64(u64);

impl Splitmix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

/// Write `size` bytes of PRNG-filled content to `path`, ticking `pb` (if
/// given) once per 64KiB chunk written — the same chunk size gat's own
/// `storage::ingest` streams in, so a large-file progress bar here behaves
/// like the one users see during a real `gat add`.
fn write_random_file(
    path: &std::path::Path,
    size: u64,
    seed: u64,
    pb: Option<&ProgressBar>,
) -> Result<()> {
    let mut rng = Splitmix64::new(seed);
    let mut file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut remaining = size;
    while remaining > 0 {
        let n = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        rng.fill(&mut buf[..n]);
        file.write_all(&buf[..n])?;
        remaining -= n as u64;
        if let Some(pb) = pb {
            pb.inc(n as u64);
        }
    }
    Ok(())
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg} {pos}/{len}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn bytes_style() -> ProgressStyle {
    ProgressStyle::with_template("{bar:30.green} {bytes}/{total_bytes} ({bytes_per_sec}) {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    #[allow(
        clippy::cast_possible_truncation,
        reason = "The random seed only needs the low 64 bits of the timestamp"
    )]
    let seed = cli.seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    });

    // No explicit `--path`: generate into a fresh OS temp directory rather
    // than defaulting to a relative path that would clutter (or collide
    // with an existing) current working directory. `into_path()` hands
    // over ownership so the directory outlives this `TempDir` guard and
    // is left for the user to inspect/clean up.
    let path = match cli.path {
        Some(path) => {
            if path.exists() {
                if cli.force {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("removing existing {}", path.display()))?;
                } else {
                    bail!(
                        "🚫 {} already exists (pass --force to overwrite)",
                        path.display()
                    );
                }
            }
            path
        }
        None => tempfile::tempdir()
            .context("creating a temp directory")?
            .keep(),
    };
    std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;

    println!("🚀 generating benchmark repo at {}", path.display());
    println!("🌱 seed = {seed}");

    let small_dir = path.join("small-files");
    let large_dir = path.join("large-files");
    std::fs::create_dir_all(&small_dir)?;
    std::fs::create_dir_all(&large_dir)?;

    println!(
        "📄 generating {} small file(s) (~{} each)",
        cli.small_files,
        format_size(cli.small_size)
    );
    let mp = MultiProgress::new();
    let small_pb = mp.add(ProgressBar::new(cli.small_files));
    small_pb.set_style(spinner_style());
    small_pb.set_message("small files");
    small_pb.enable_steady_tick(std::time::Duration::from_millis(100));
    (0..cli.small_files)
        .into_par_iter()
        .try_for_each(|i| -> Result<()> {
            // Jitter +/-20% so small files aren't all identical length either.
            let mut jitter_rng = Splitmix64::new(seed ^ i ^ 0xA5A5_A5A5_A5A5_A5A5);
            let percent = 80 + jitter_rng.next_u64() % 41;
            let bytes = (u128::from(cli.small_size) * u128::from(percent) + 50) / 100;
            let size = u64::try_from(bytes).unwrap_or(u64::MAX);
            let path = small_dir.join(format!("file-{i:06}.bin"));
            write_random_file(&path, size, seed ^ i, None)?;
            small_pb.inc(1);
            Ok(())
        })?;
    small_pb.finish_and_clear();

    println!(
        "🐘 generating {} large file(s) (~{} each)",
        cli.large_files,
        format_size(cli.large_size)
    );
    (0..cli.large_files)
        .into_par_iter()
        .try_for_each(|i| -> Result<()> {
            let path = large_dir.join(format!("large-{i:03}.bin"));
            let pb = mp.add(ProgressBar::new(cli.large_size));
            pb.set_style(bytes_style());
            pb.set_message(format!("large-{i:03}.bin"));
            write_random_file(&path, cli.large_size, seed ^ i ^ 0xDEAD_BEEF_u64, Some(&pb))?;
            pb.finish_and_clear();
            Ok(())
        })?;

    println!("🔧 git init …");
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&path)
        .status()
        .context("running `git init`")?;
    anyhow::ensure!(status.success(), "`git init` failed");

    println!(
        "✅ done! {} small file(s) + {} large file(s) at {}",
        cli.small_files,
        cli.large_files,
        path.display()
    );
    println!(
        "👉 Next: cd {} && gat init && time gat add small-files/ large-files/",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_handles_bare_bytes() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("512B").unwrap(), 512);
    }

    #[test]
    fn parse_size_handles_binary_units_case_insensitively() {
        assert_eq!(parse_size("4KiB").unwrap(), 4 * 1024);
        assert_eq!(parse_size("4kb").unwrap(), 4 * 1024);
        assert_eq!(parse_size("256MiB").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_size("1GiB").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_size_handles_decimal_values_and_whitespace() {
        assert_eq!(parse_size("1.5 MiB").unwrap(), 1536 * 1024);
    }

    #[test]
    fn parse_size_rejects_out_of_range_values() {
        assert!(parse_size("18446744073709551616").is_err());
        assert!(parse_size("18446744073709551616 GiB").is_err());
        assert!(parse_size(&"9".repeat(400)).is_err());
        assert!(parse_size("-1").is_err());
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("17179869184").unwrap(), 17_179_869_184);
    }

    #[test]
    fn parse_size_rejects_unknown_suffix() {
        assert!(parse_size("4XB").is_err());
    }

    #[test]
    fn parse_size_rejects_garbage_number() {
        assert!(parse_size("not-a-size").is_err());
    }

    #[test]
    fn format_size_roundtrips_common_values() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(4 * 1024), "4.0 KiB");
        assert_eq!(format_size(256 * 1024 * 1024), "256.0 MiB");
    }

    #[test]
    fn splitmix64_is_deterministic_given_a_seed() {
        let mut a = Splitmix64::new(42);
        let mut b = Splitmix64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn splitmix64_differs_across_seeds() {
        let mut a = Splitmix64::new(1);
        let mut b = Splitmix64::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn write_random_file_produces_deterministic_distinct_content() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.bin");
        let b = tmp.path().join("b.bin");
        write_random_file(&a, 10_000, 1, None).unwrap();
        write_random_file(&b, 10_000, 2, None).unwrap();
        let content_a = std::fs::read(&a).unwrap();
        let content_b = std::fs::read(&b).unwrap();
        assert_eq!(content_a.len(), 10_000);
        assert_ne!(content_a, content_b, "distinct seeds must not collide");

        // same seed => same bytes (reproducible benchmark content)
        let a2 = tmp.path().join("a2.bin");
        write_random_file(&a2, 10_000, 1, None).unwrap();
        assert_eq!(content_a, std::fs::read(&a2).unwrap());
    }
}
