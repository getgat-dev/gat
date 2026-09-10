//! Inspect the artifact with the platform's readelf; no ELF parser dependency.
use crate::Result;
use std::{path::Path, process::Command};

fn version(value: &str) -> Result<[u64; 3]> {
    let parts = value.split('.').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(format!("invalid glibc version: {value}").into());
    }
    let mut result = [0; 3];
    for (slot, part) in result.iter_mut().zip(parts) {
        *slot = part.parse()?;
    }
    Ok(result)
}

fn validate_header(header: &str, target: &str) -> Result<bool> {
    let (machine, musl) = match target {
        "x86_64-unknown-linux-gnu" => ("Advanced Micro Devices X86-64", false),
        "x86_64-unknown-linux-musl" => ("Advanced Micro Devices X86-64", true),
        "aarch64-unknown-linux-gnu" => ("AArch64", false),
        "aarch64-unknown-linux-musl" => ("AArch64", true),
        _ => return Err(format!("unsupported Linux target: {target}").into()),
    };
    let field = |name| {
        header
            .lines()
            .find_map(|line| line.trim().strip_prefix(name))
            .map_or("", str::trim)
    };
    if field("Class:") != "ELF64"
        || !field("Data:").ends_with("little endian")
        || !matches!(
            field("Type:").split_whitespace().next(),
            Some("EXEC" | "DYN")
        )
    {
        return Err("release must be a 64-bit little-endian executable".into());
    }
    if field("Machine:") != machine {
        return Err("release has the wrong architecture".into());
    }
    Ok(musl)
}

fn validate_symbols(text: &str, maximum: [u64; 3]) -> Result<()> {
    let mut count = 0;
    for line in text.lines() {
        let words = line.split_whitespace().collect::<Vec<_>>();
        for pair in words.windows(2) {
            if pair[0] == "Name:"
                && let Some(value) = pair[1].strip_prefix("GLIBC_")
            {
                count += 1;
                if version(value).map_or(true, |version| version > maximum) {
                    return Err(format!("unsupported glibc symbol: {}", pair[1]).into());
                }
            }
        }
    }
    if count == 0 {
        return Err("no glibc symbol requirements found".into());
    }
    Ok(())
}

fn validate_static(headers: &str, dynamic: &str) -> Result<()> {
    if headers.split_whitespace().any(|word| word == "INTERP") || dynamic.contains("(NEEDED)") {
        return Err(
            "musl release must have no ELF interpreter or shared-library dependencies".into(),
        );
    }
    Ok(())
}

pub fn check(binary: &Path, target: &str, maximum: &str) -> Result<Option<String>> {
    let maximum = version(maximum)?;
    let read = |argument| -> Result<String> {
        let output = Command::new("readelf")
            .args([argument, "--wide"])
            .arg(binary)
            .env("LC_ALL", "C")
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "readelf {argument} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(output.stdout)?)
    };
    let musl = match validate_header(&read("--file-header")?, target) {
        Ok(musl) => musl,
        Err(error) => return Ok(Some(error.to_string())),
    };
    let validation = if musl {
        validate_static(&read("--program-headers")?, &read("--dynamic")?)
    } else {
        validate_symbols(&read("--version-info")?, maximum)
    };
    Ok(validation.err().map(|error| error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn musl_rejects_both_interpreters_and_dynamic_dependencies() {
        assert!(validate_static("LOAD", "").is_ok());
        assert!(validate_static("LOAD INTERP", "").is_err());
        assert!(validate_static("LOAD", "(NEEDED) libc.so").is_err());
    }

    #[test]
    fn glibc_versions_are_numeric_and_unknown_requirements_fail_closed() {
        let maximum = version("2.28").unwrap();
        assert!(validate_symbols("Name: GLIBC_2.9\nName: GLIBC_2.28", maximum).is_ok());
        for value in ["2.28.1", "2.29", "2.100", "3.0", "PRIVATE", "ABI_DT_RELR"] {
            assert!(validate_symbols(&format!("Name: GLIBC_{value}"), maximum).is_err());
        }
        assert!(validate_symbols("", maximum).is_err());
        assert!(version("2.28junk").is_err());
    }

    #[test]
    fn executable_header_must_match_target() {
        let header =
            "Class: ELF64\nData: 2's complement, little endian\nType: DYN (PIE)\nMachine: AArch64";
        assert!(validate_header(header, "aarch64-unknown-linux-musl").unwrap());
        assert!(validate_header(header, "x86_64-unknown-linux-musl").is_err());
        assert!(
            validate_header(
                &header.replace("ELF64", "ELF32"),
                "aarch64-unknown-linux-musl"
            )
            .is_err()
        );
        assert!(
            validate_header(&header.replace("DYN", "REL"), "aarch64-unknown-linux-musl").is_err()
        );
    }
}
