//! Repository policy checks, independent of the application dependency graph.
mod release;
mod source;
mod workspace;

use std::ffi::OsStr;
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
        let platform = if cfg!(windows) {
            InstallerPlatform::Windows
        } else {
            InstallerPlatform::Unix
        };
        let mut process = installer_command(&root, platform, arguments.get(1).map(AsRef::as_ref));
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

#[derive(Clone, Copy)]
enum InstallerPlatform {
    Unix,
    Windows,
}

fn installer_command(root: &Path, platform: InstallerPlatform, shell: Option<&OsStr>) -> Command {
    let mut process = match platform {
        InstallerPlatform::Windows => {
            let mut process = Command::new(shell.unwrap_or_else(|| OsStr::new("pwsh")));
            process.args(["-NoProfile", "-NonInteractive", "-File"]);
            process.arg(root.join("tools/check/test-installers.ps1"));
            process
        }
        InstallerPlatform::Unix => {
            let mut process = Command::new("bash");
            process.arg(root.join("tools/check/test-installers.sh"));
            if let Some(shell) = shell {
                process.arg(shell);
            }
            process
        }
    };
    process.current_dir(root);
    process
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_fixtures_run_in_the_selected_powershell_edition() {
        let root = Path::new("checkout with spaces");
        for shell in ["powershell", "pwsh"] {
            let command =
                installer_command(root, InstallerPlatform::Windows, Some(OsStr::new(shell)));
            assert_eq!(command.get_program(), shell);
            assert_eq!(command.get_current_dir(), Some(root));
            let arguments = command.get_args().collect::<Vec<_>>();
            assert_eq!(&arguments[..3], ["-NoProfile", "-NonInteractive", "-File"]);
            assert_eq!(arguments[3], root.join("tools/check/test-installers.ps1"));
            assert_eq!(arguments.len(), 4);
        }
    }

    #[test]
    fn unix_fixture_driver_preserves_the_selected_installer_shell() {
        let root = Path::new("checkout with spaces");
        let command =
            installer_command(root, InstallerPlatform::Unix, Some(OsStr::new("/bin/dash")));
        assert_eq!(command.get_program(), "bash");
        assert_eq!(command.get_current_dir(), Some(root));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                root.join("tools/check/test-installers.sh").as_os_str(),
                OsStr::new("/bin/dash")
            ]
        );
    }
}
