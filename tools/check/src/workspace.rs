use crate::{Finding, Result};
use serde_json::Value;
use std::{collections::BTreeSet, path::Path, process::Command};

pub fn check(root: &Path, findings: &mut Vec<Finding>) -> Result<()> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    inspect(&metadata, findings)?;
    Ok(())
}

fn inspect(metadata: &Value, findings: &mut Vec<Finding>) -> Result<()> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata has no packages")?;
    let defaults = metadata["workspace_default_members"]
        .as_array()
        .ok_or("cargo metadata has no default members")?;
    let root_msrv = packages
        .iter()
        .find(|package| package["name"] == "gat")
        .and_then(|package| package["rust_version"].as_str())
        .ok_or(
            "application package must declare rust-version before workspace MSRV can be checked",
        )?;
    let package_names = packages
        .iter()
        .map(|package| package["name"].as_str().ok_or("package has no name"))
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    for package in packages {
        let name = package["name"].as_str().ok_or("package has no name")?;
        let allowed: Option<&[&str]> = match name {
            "gat-core" => Some(&[]),
            "gat-io" => Some(&["gat-core"]),
            "gat-engine" => Some(&["gat-core", "gat-io"]),
            "gat-command" => Some(&["gat-core", "gat-engine"]),
            "gat" => Some(&["gat-core", "gat-engine", "gat-command"]),
            "gat-check" | "gat-bench" => Some(&[]),
            _ => None,
        };
        let path = match name {
            "gat" => "Cargo.toml".to_owned(),
            "gat-docs" => "tools/docs/Cargo.toml".to_owned(),
            "gat-bench" => "tools/benchmark/Cargo.toml".to_owned(),
            "gat-check" => "tools/check/Cargo.toml".to_owned(),
            _ => format!("{name}/Cargo.toml"),
        };
        let mut report = |rule, message| {
            findings.push(Finding {
                path: path.clone(),
                line: 1,
                rule,
                message,
            });
        };
        if matches!(name, "gat-check" | "gat-docs" | "gat-bench")
            && defaults.contains(&package["id"])
        {
            report(
                "workspace/defaults",
                "developer tools must not be default workspace members".into(),
            );
        }
        if package["rust_version"].as_str() != Some(root_msrv) {
            report(
                "workspace/msrv",
                format!("declare rust-version = {root_msrv:?} to match the application MSRV"),
            );
        }
        for dependency in package["dependencies"]
            .as_array()
            .ok_or("package has no dependencies")?
        {
            if dependency["kind"] == "dev" {
                continue;
            }
            let dependency_name = dependency["name"]
                .as_str()
                .ok_or("dependency has no name")?;
            let internal = package_names.contains(dependency_name);
            if internal && allowed.is_some_and(|allowed| !allowed.contains(&dependency_name)) {
                report(
                    "workspace/direction",
                    format!("{name} must not depend on {dependency_name} outside dev-dependencies"),
                );
            }
            if name == "gat" && dependency["kind"].is_null() && dependency["optional"] == true {
                report(
                    "workspace/optional",
                    "root normal dependencies must not be optional".into(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_application_msrv_cannot_silently_disable_policy() {
        for packages in [
            json!([]),
            json!([{"name":"gat", "id":"app", "dependencies":[]}]),
        ] {
            let metadata = json!({"workspace_default_members": [], "packages":packages});
            assert!(inspect(&metadata, &mut Vec::new()).is_err());
        }
    }

    #[test]
    fn workspace_packages_share_the_declared_application_msrv() {
        for rust_version in [json!("1.91"), json!("1.90"), Value::Null] {
            let metadata = json!({
                "workspace_default_members": [],
                "packages": [
                    {"name":"gat", "id":"app", "rust_version":"1.91", "dependencies":[]},
                    {"name":"gat-check", "id":"check", "rust_version":rust_version, "dependencies":[]}
                ]
            });
            let mut findings = Vec::new();
            inspect(&metadata, &mut findings).unwrap();
            if rust_version == "1.91" {
                assert!(findings.is_empty());
            } else {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].rule, "workspace/msrv");
                assert_eq!(findings[0].path, "tools/check/Cargo.toml");
            }
        }
    }

    #[test]
    fn default_members_and_optional_root_dependencies_are_independent_rules() {
        let metadata = json!({
            "workspace_default_members": ["checker"],
            "packages": [
                {"name":"gat-check", "id":"checker", "rust_version":"1.91", "dependencies":[]},
                {"name":"gat", "id":"app", "rust_version":"1.91", "dependencies":[
                    {"name":"external", "kind":null, "optional":true}
                ]}
            ]
        });
        let mut findings = Vec::new();
        inspect(&metadata, &mut findings).unwrap();
        assert_eq!(
            findings
                .iter()
                .map(|finding| finding.rule)
                .collect::<Vec<_>>(),
            ["workspace/defaults", "workspace/optional"]
        );
        assert_eq!(findings[0].path, "tools/check/Cargo.toml");
    }

    #[test]
    fn metadata_checks_resolved_packages_and_all_production_dependency_kinds() {
        for kind in [Value::Null, json!("build"), json!("dev")] {
            let metadata = json!({
                "workspace_default_members": [],
                "packages": [
                    {"name":"gat", "id":"app", "rust_version":"1.91", "dependencies":[]},
                    {"name":"gat-core", "id":"core", "rust_version":"1.91", "dependencies":[
                        {"name":"gat-io", "rename":"storage", "kind":kind, "target":"cfg(windows)", "optional":false}
                    ]},
                    {"name":"gat-io", "id":"io", "rust_version":"1.91", "dependencies":[]}
                ]
            });
            let mut findings = Vec::new();
            inspect(&metadata, &mut findings).unwrap();
            assert_eq!(findings.is_empty(), kind == "dev");
        }
    }
}
