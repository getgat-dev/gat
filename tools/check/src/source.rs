use crate::{Finding, Result};
use std::{collections::BTreeMap, path::Path};
use syn::{
    Item, UseTree,
    spanned::Spanned,
    visit::{self, Visit},
};

pub fn check(
    root: &Path,
    architecture: bool,
    hygiene: bool,
    findings: &mut Vec<Finding>,
) -> Result<()> {
    for directory in [
        "src",
        "tests",
        "gat-core",
        "gat-io",
        "gat-engine",
        "gat-command",
        "test-support-git",
        "test-support-gat",
        "tools",
    ] {
        walk(root, &root.join(directory), architecture, hygiene, findings)?;
    }
    Ok(())
}

fn walk(
    root: &Path,
    directory: &Path,
    architecture: bool,
    hygiene: bool,
    findings: &mut Vec<Finding>,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir()
            && !matches!(
                entry.file_name().to_str(),
                Some("target" | "node_modules" | ".git")
            )
        {
            walk(root, &path, architecture, hygiene, findings)?;
        } else if kind.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
            let name = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path)?;
            let syntax = syn::parse_file(&text)
                .map_err(|error| format!("{name}: cannot parse Rust: {error}"))?;
            let test = name.starts_with("tests/")
                || name.contains("/tests/")
                || name.starts_with("test-support-");
            let mut checker = Checker {
                name: &name,
                text: &text,
                aliases: BTreeMap::new(),
                test,
                architecture,
                hygiene,
                findings,
            };
            checker.imports(&syntax.items);
            checker.visit_file(&syntax);
        }
    }
    Ok(())
}

struct Checker<'a> {
    name: &'a str,
    text: &'a str,
    aliases: BTreeMap<String, String>,
    test: bool,
    architecture: bool,
    hygiene: bool,
    findings: &'a mut Vec<Finding>,
}

fn test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute
            .path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "test")
            || (attribute.path().is_ident("cfg")
                && attribute
                    .parse_args::<syn::Meta>()
                    .is_ok_and(|meta| requires_test(&meta)))
    })
}

// Whether a cfg predicate implies test/test-support, not whether it happens
// to mention test. In particular, any(test, unix) includes production code.
fn requires_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::NameValue(value) if value.path.is_ident("feature") => {
            matches!(&value.value, syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(value), .. }) if value.value() == "test-support")
        }
        syn::Meta::List(list) => {
            let Ok(children) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            ) else {
                return false;
            };
            if list.path.is_ident("all") {
                children.iter().any(requires_test)
            } else {
                list.path.is_ident("any")
                    && !children.is_empty()
                    && children.iter().all(requires_test)
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

fn imports(tree: &UseTree, prefix: &str, aliases: &mut BTreeMap<String, String>) {
    let joined = |name: &str| {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}::{name}")
        }
    };
    match tree {
        UseTree::Path(path) => imports(&path.tree, &joined(&path.ident.to_string()), aliases),
        UseTree::Name(name) if name.ident == "self" => {
            if let Some(last) = prefix.rsplit("::").next() {
                aliases.insert(last.into(), prefix.into());
            }
        }
        UseTree::Name(name) => {
            aliases.insert(name.ident.to_string(), joined(&name.ident.to_string()));
        }
        UseTree::Rename(rename) => {
            aliases.insert(
                rename.rename.to_string(),
                if rename.ident == "self" {
                    prefix.into()
                } else {
                    joined(&rename.ident.to_string())
                },
            );
        }
        UseTree::Group(group) => {
            for item in &group.items {
                imports(item, prefix, aliases);
            }
        }
        UseTree::Glob(_) => {
            aliases.insert(format!("*{prefix}"), prefix.into());
        }
    }
}

impl Checker<'_> {
    fn imports(&mut self, items: &[Item]) {
        for item in items {
            if let Item::Use(item) = item {
                imports(&item.tree, "", &mut self.aliases);
            }
        }
    }

    fn resolve(&self, path: &syn::Path) -> String {
        let mut path = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        for _ in 0..16 {
            let (head, rest) = path.split_once("::").unwrap_or((&path, ""));
            let Some(alias) = self.aliases.get(head) else {
                break;
            };
            let expanded = if rest.is_empty() {
                alias.clone()
            } else {
                format!("{alias}::{rest}")
            };
            if expanded == path {
                break;
            }
            path = expanded;
        }
        path
    }

    fn report(&mut self, span: proc_macro2::Span, rule: &'static str, message: &'static str) {
        self.findings.push(Finding {
            path: self.name.into(),
            line: span.start().line,
            rule,
            message: message.into(),
        });
    }

    fn justified(&self, line: usize, marker: &str) -> bool {
        self.text
            .lines()
            .take(line.saturating_sub(1))
            .skip(line.saturating_sub(5))
            .any(|line| {
                line.trim_start()
                    .strip_prefix("//")
                    .and_then(|comment| comment.split_once(marker))
                    .is_some_and(|(_, reason)| !reason.trim().is_empty())
            })
    }

    fn path(&mut self, path: &syn::Path) {
        let resolved = self.resolve(path);
        let span = path.span();
        let ambient = ["var", "var_os", "vars", "vars_os"]
            .iter()
            .any(|name| resolved == format!("std::env::{name}"));
        let mutation = matches!(
            resolved.as_str(),
            "std::env::set_var" | "std::env::remove_var"
        );
        if (self.architecture || self.hygiene) && resolved == "std::env::set_current_dir" {
            self.report(span, "environment/cwd", "pass explicit paths or set a child process working directory instead of changing the process working directory");
        }
        if (self.architecture || self.hygiene) && mutation {
            self.report(
                span,
                "environment/mutation",
                "use explicit inputs or a child process; do not mutate the process environment",
            );
        }
        if self.architecture
            && !self.test
            && ambient
            && self.name != "gat-io/src/env.rs"
            && !self.name.starts_with("tools/")
        {
            self.report(
                span,
                "environment/capture",
                "read environment settings through invocation inputs",
            );
        }
        if self.architecture
            && resolved.ends_with("capture_process")
            && !matches!(
                self.name,
                "gat-io/src/env.rs" | "gat-engine/src/invocation.rs" | "src/main.rs"
            )
            && !self.name.starts_with("tools/")
        {
            self.report(
                span,
                "environment/bootstrap",
                "capture the environment only at invocation bootstrap",
            );
        }
        if self.architecture && !self.test {
            if (self.name.starts_with("gat-engine/") || self.name.starts_with("gat-command/"))
                && (resolved == "std::process::Command"
                    || resolved.starts_with("std::process::Command::")
                    || resolved == "tokio::process::Command"
                    || resolved.starts_with("tokio::process::Command::"))
            {
                self.report(
                    span,
                    "ownership/process",
                    "spawn processes through repository-bound IO capabilities",
                );
            }

            if self.name.starts_with("gat-command/")
                && (resolved.starts_with("std::fs::") || resolved.starts_with("tokio::fs::"))
            {
                self.report(
                    span,
                    "ownership/filesystem",
                    "command code must use engine capabilities for filesystem work",
                );
            }
            if self.name.starts_with("gat-engine/") {
                if matches!(
                    resolved.as_str(),
                    "std::env::current_dir"
                        | "std::io::BufReader"
                        | "std::io::BufWriter"
                        | "std::fs::File"
                ) || resolved.starts_with("std::fs::")
                    || resolved.starts_with("tokio::fs::")
                    || resolved.starts_with("std::io::BufReader::")
                    || resolved.starts_with("std::io::BufWriter::")
                {
                    self.report(
                        span,
                        "ownership/io",
                        "engine code must use repository-bound IO capabilities",
                    );
                }
                if resolved.starts_with("gat_io::RemoteClient::open")
                    && self.name != "gat-engine/src/remote_session.rs"
                {
                    self.report(
                        span,
                        "ownership/remote",
                        "open remote clients through the operation remote session",
                    );
                }
            }
        }
        if self.hygiene
            && self.test
            && matches!(
                resolved.as_str(),
                "std::thread::sleep" | "tokio::time::sleep"
            )
            && !self.justified(span.start().line, "sleep-ok:")
        {
            self.report(
                span,
                "tests/sleep",
                "use a deterministic handshake or explain elapsed-time testing with sleep-ok:",
            );
        }
    }
}

impl<'ast> Visit<'ast> for Checker<'_> {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.path(path);
        visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let mut aliases = BTreeMap::new();
        imports(&item.tree, "", &mut aliases);
        for (alias, target) in aliases {
            let target = syn::parse_str::<syn::Path>(&target)
                .map_or_else(|_| target.clone(), |path| self.resolve(&path));
            if self.architecture
                && alias.starts_with('*')
                && (target == "std"
                    || target.starts_with("std::env")
                    || target.starts_with("std::fs"))
            {
                self.report(item.span(), "imports/explicit", "use explicit imports for environment and filesystem APIs so ownership is auditable");
            }
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let aliases = self.aliases.clone();
        let test = self.test;
        self.test |= test_only(&item.attrs);
        if let Some((_, items)) = &item.content {
            self.imports(items);
        }
        visit::visit_item_mod(self, item);
        self.aliases = aliases;
        self.test = test;
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let test = self.test;
        self.test |= test_only(&item.attrs);
        if self.architecture && self.name.starts_with("src/") && item.sig.ident == "print_error" {
            self.report(
                item.span(),
                "errors/render",
                "render typed diagnostics instead of defining a generic print_error helper",
            );
        }
        visit::visit_item_fn(self, item);
        self.test = test;
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let aliases = self.aliases.clone();
        for statement in &block.stmts {
            if let syn::Stmt::Item(Item::Use(item)) = statement {
                imports(&item.tree, "", &mut self.aliases);
            }
        }
        visit::visit_block(self, block);
        self.aliases = aliases;
    }

    fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
        if self.architecture
            && !self.test
            && (self.name.starts_with("src/") || self.name.starts_with("gat-"))
            && matches!(self.resolve(&invocation.path).as_str(), "dbg" | "std::dbg")
        {
            self.report(
                invocation.span(),
                "errors/debug",
                "remove debug output from production code; use typed output",
            );
        }

        if self.architecture
            && self.name.starts_with("src/")
            && matches!(
                self.resolve(&invocation.path).as_str(),
                "eprintln" | "std::eprintln"
            )
            && self.name != "src/output/error.rs"
        {
            self.report(
                invocation.span(),
                "errors/render",
                "send diagnostics through the output renderer",
            );
        }
        // Analyze expression arguments in macros such as assert!/format!.
        // Arbitrary macro DSLs and generated expansions are outside this tool.
        use syn::parse::Parser;
        if let Ok(expressions) =
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(invocation.tokens.clone())
        {
            for expression in &expressions {
                self.visit_expr(expression);
            }
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if self.hygiene
            && self.test
            && let syn::Expr::Path(function) = call.func.as_ref()
        {
            let path = self.resolve(&function.path);
            let boundary = path.starts_with("std::fs::")
                || path.starts_with("tokio::fs::")
                || path.ends_with("::bind")
                || path.contains("RemoteClient::open");
            if boundary && !self.justified(call.span().start().line, "hygiene-ok:") {
                for argument in &call.args {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(value),
                        ..
                    }) = argument
                    {
                        let value = value.value();
                        if shared_path(&value)
                            || value.starts_with("http://")
                            || value.starts_with("https://")
                            || (path.ends_with("::bind") && !isolated_bind(&value))
                        {
                            self.report(call.span(), "tests/isolation", "use fixture-owned paths/endpoints; justify deliberate boundary tests with hygiene-ok:");
                        }
                    }
                }
            }
        }
        visit::visit_expr_call(self, call);
    }
}

fn shared_path(value: &str) -> bool {
    value == "/tmp"
        || value.starts_with("/tmp/")
        || value == "/var/tmp"
        || value.starts_with("/var/tmp/")
        || (value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
            && value.as_bytes().get(1) == Some(&b':')
            && matches!(value.as_bytes().get(2), Some(b'/' | b'\\')))
}

fn isolated_bind(value: &str) -> bool {
    value == "localhost:0"
        || value
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|address| address.ip().is_loopback() && address.port() == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(name: &str, text: &str) -> Vec<&'static str> {
        let syntax = syn::parse_file(text).unwrap();
        let mut findings = Vec::new();
        let mut checker = Checker {
            name,
            text,
            aliases: BTreeMap::new(),
            test: name.starts_with("tests/"),
            architecture: true,
            hygiene: true,
            findings: &mut findings,
        };
        checker.imports(&syntax.items);
        checker.visit_file(&syntax);
        findings.into_iter().map(|finding| finding.rule).collect()
    }

    #[test]
    fn working_directory_mutation_is_global_but_child_configuration_is_local() {
        assert_eq!(
            rules(
                "tests/x.rs",
                r#"use std::env::set_current_dir as cd; fn f(){ cd("x"); }"#
            ),
            ["environment/cwd"]
        );
        assert!(
            rules(
                "tests/x.rs",
                r#"fn f(){ std::process::Command::new("git").current_dir("x"); }"#
            )
            .is_empty()
        );
    }

    #[test]
    fn async_io_and_processes_obey_the_same_ownership_as_sync_io() {
        for owner in ["gat-engine/src/x.rs", "gat-command/src/x.rs"] {
            for text in [
                r#"use tokio::fs as disk; async fn f(){ disk::read("x").await; }"#,
                r#"use tokio::process::Command as Child; fn f(){ Child::new("git"); }"#,
                r#"fn f(){ std::process::Command::new("git"); }"#,
            ] {
                assert!(!rules(owner, text).is_empty());
                assert!(rules(owner, &format!("#[cfg(test)] mod tests {{ {text} }}")).is_empty());
                assert!(rules("gat-io/src/x.rs", text).is_empty());
            }
        }
    }

    #[test]
    fn debug_output_is_for_tests_not_production() {
        for text in [
            "fn f(){ dbg!(1); }",
            "use std::dbg as trace; fn f(){ trace!(1); }",
        ] {
            assert_eq!(rules("src/x.rs", text), ["errors/debug"]);
            assert!(rules("tests/x.rs", text).is_empty());
            assert!(rules("tools/docs/src/main.rs", text).is_empty());
        }
        assert!(rules("src/x.rs", "fn f(){ custom::dbg!(1); }").is_empty());
    }

    #[test]
    fn bind_isolation_covers_ipv6_wildcards_and_public_addresses() {
        for address in [
            "[::1]:1234",
            "[::]:0",
            "0.0.0.0:0",
            "192.0.2.1:0",
            "example.test:0",
        ] {
            assert_eq!(
                rules(
                    "tests/x.rs",
                    &format!("fn f(){{ tokio::net::TcpListener::bind({address:?}); }}")
                ),
                ["tests/isolation"]
            );
        }
        for address in ["[::1]:0", "127.0.0.2:0", "localhost:0"] {
            assert!(
                rules(
                    "tests/x.rs",
                    &format!("fn f(){{ std::net::TcpListener::bind({address:?}); }}")
                )
                .is_empty()
            );
        }
        assert!(
            rules(
                "tests/x.rs",
                "// hygiene-ok: exercise invalid endpoint validation
fn f(){ std::net::TcpListener::bind(\"invalid\"); }"
            )
            .is_empty()
        );
        for path in ["/tmp", "/var/tmp", "C:/shared", r"C:\shared"] {
            assert_eq!(
                rules(
                    "tests/x.rs",
                    &format!("fn f(){{ tokio::fs::read({path:?}); }}")
                ),
                ["tests/isolation"]
            );
        }
    }

    #[test]
    fn aliases_nested_imports_and_macro_arguments_are_checked() {
        assert!(
            rules(
                "gat-engine/src/x.rs",
                "use std::{env::{var as read}}; fn f(){ let _ = read(\"X\"); }"
            )
            .contains(&"environment/capture")
        );
        assert!(
            rules(
                "tests/x.rs",
                "use std::env as e; fn f(){ assert!(e::set_var(\"X\",\"Y\")); }"
            )
            .contains(&"environment/mutation")
        );
        assert!(
            rules(
                "gat-command/src/x.rs",
                "use std::fs as disk; fn f(){ disk::read(\"x\"); }"
            )
            .contains(&"ownership/filesystem")
        );
    }

    #[test]
    fn cfg_logic_does_not_hide_production_code_or_leak_test_imports() {
        assert!(
            rules(
                "gat-command/src/x.rs",
                "#[cfg(any(test, unix))] mod x { fn f(){ std::fs::read(\"x\"); } }"
            )
            .contains(&"ownership/filesystem")
        );
        assert!(
            rules(
                "gat-command/src/x.rs",
                "#[cfg(all(test, unix))] mod x { fn f(){ std::fs::read(\"x\"); } }"
            )
            .is_empty()
        );
        assert!(
            rules(
                "gat-engine/src/x.rs",
                "mod a { use std::env as e; } mod b { fn f(){ e::var(\"x\"); } }"
            )
            .is_empty()
        );
        assert!(
            rules("gat-engine/src/x.rs", "use std as s; use s::env::*;")
                .contains(&"imports/explicit")
        );
    }

    #[test]
    fn isolation_checks_operations_and_allows_ephemeral_ports() {
        assert!(
            rules(
                "tests/x.rs",
                r#"fn f(){ std::net::TcpListener::bind("127.0.0.1:0"); }"#
            )
            .is_empty()
        );
        assert!(
            rules(
                "tests/x.rs",
                r#"fn f(){ std::net::TcpListener::bind("127.0.0.1:1234"); }"#
            )
            .contains(&"tests/isolation")
        );
        assert!(
            rules(
                "tests/x.rs",
                r#"fn f(){ std::fs::write("/tmp/shared", "x"); }"#
            )
            .contains(&"tests/isolation")
        );
    }

    #[test]
    fn comments_literals_and_test_data_are_not_calls() {
        assert!(rules("tests/x.rs", "fn f(){ let _ = r###\"std::env::set_var( https://example.test )\"###; /* std::env::var(\"x\") */ }").is_empty());
    }

    #[test]
    fn test_context_ends_at_the_module_boundary() {
        let text = "#[cfg(test)] mod tests { fn f(){ std::thread::sleep(todo!()); } } fn f(){ std::thread::sleep(todo!()); }";
        assert_eq!(rules("gat-core/src/x.rs", text), ["tests/sleep"]);
    }

    #[test]
    fn qualified_and_aliased_error_macros_use_the_same_render_boundary() {
        for text in [
            r#"fn f(){ std::eprintln!("failure"); }"#,
            r#"use std::eprintln as report; fn f(){ report!("failure"); }"#,
        ] {
            assert_eq!(rules("src/x.rs", text), ["errors/render"]);
            assert!(rules("src/output/error.rs", text).is_empty());
        }
        assert!(rules("src/x.rs", r#"fn f(){ custom::eprintln!("data"); }"#).is_empty());
    }

    #[test]
    fn ownership_rules_apply_to_production_not_fixture_io() {
        assert!(
            rules(
                "gat-command/src/x.rs",
                "#[cfg(test)] mod tests { fn f(){ std::fs::read(\"x\"); } }"
            )
            .is_empty()
        );
        assert!(rules("src/x.rs", "fn f(){ eprintln!(\"failure\"); }").contains(&"errors/render"));
    }
}
