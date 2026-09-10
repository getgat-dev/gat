//! Repository policy checks, independent of the application dependency graph.
mod release;
mod source;
mod workspace;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    path: String,
    line: usize,
    rule: &'static str,
    message: String,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("gat-check: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let root = root().canonicalize()?;
    let Some(command) = arguments.first().and_then(|argument| argument.to_str()) else {
        return Err("usage: gat-check architecture | test-hygiene | all | release BINARY TARGET MAX_GLIBC | installers [SHELL]".into());
    };
    if command == "release" && arguments.len() == 4 {
        if let Some(reason) = release::check(
            Path::new(&arguments[1]),
            arguments[2].to_str().ok_or("target must be Unicode")?,
            arguments[3]
                .to_str()
                .ok_or("glibc version must be Unicode")?,
        )? {
            eprintln!(
                "{}: [release/abi] {reason}",
                Path::new(&arguments[1]).display()
            );
            return Ok(false);
        }
        println!("release: OK");
        return Ok(true);
    }
    if command == "installers" && arguments.len() <= 2 {
        let (shell, script) = if cfg!(windows) {
            ("pwsh", "tools/check/test-installers.ps1")
        } else {
            ("bash", "tools/check/test-installers.sh")
        };
        let mut process = Command::new(shell);
        if cfg!(windows) {
            process.arg("-File");
        }
        process.arg(root.join(script)).current_dir(&root);
        if let Some(shell) = arguments.get(1) {
            process.arg(shell);
        }
        return Ok(process.status()?.success());
    }
    if arguments.len() != 1 || !matches!(command, "all" | "architecture" | "test-hygiene") {
        return Err("unknown command or arguments; use architecture, test-hygiene, all, release, or installers".into());
    }
    let mut findings = Vec::new();
    if command != "test-hygiene" {
        workspace::check(&root, &mut findings)?;
    }
    source::check(
        &root,
        command != "test-hygiene",
        command != "architecture",
        &mut findings,
    )?;
    findings.sort();
    findings.dedup();
    for finding in &findings {
        eprintln!(
            "{}:{}: [{}] {}",
            finding.path, finding.line, finding.rule, finding.message
        );
    }
    println!("{command}: {} finding(s)", findings.len());
    Ok(findings.is_empty())
}
