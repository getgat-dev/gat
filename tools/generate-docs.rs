#![allow(
    clippy::format_push_string,
    reason = "Infallible string appends keep document templates readable without formatting-error boilerplate"
)]
//! Generates the CLI and configuration references from the clap command
//! definitions and shared configuration metadata.
//!
//! Subcommands are inlined into their top-level command page so they do not
//! create separate navigation pages.

mod docs;

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Command, CommandFactory, Parser};
use docs::{COMMAND_DOCS, CommandDoc, CommandExample};
use gat::cli;
use gat::lifecycle;
use gat_core::config::{
    self, CacheConfig, Config, GitConfig, LockConfig, MountConfig, MountsConfig, RemotesConfig,
    SyncConfig,
};
use gat_core::config_keys::{
    CONFIG_KEYS, ConfigDefault, ConfigDocValue, ConfigElementSpec, ConfigKeySpec, ConfigSection,
    ConfigSetter, ConfigValueSpec, ConfigWriteSurface, EffectiveDefault, EmptyListPolicy,
    PersistedDefault,
};
use gat_core::endpoint::RemoteUrlTemplate;
use gat_core::git_location::GitLocationSpec;
use gat_core::name::{MountName, RemoteName, RouteName};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const CONFIGURATION_REMOTE_GUIDE: &str = include_str!("docs/content/configuration-remotes.md");

#[derive(Parser)]
struct Options {
    #[arg(long, conflicts_with = "check")]
    write: bool,
    #[arg(long, conflicts_with = "write")]
    check: bool,
}

#[derive(Default)]
struct GeneratedDocs {
    files: BTreeMap<PathBuf, String>,
}

impl GeneratedDocs {
    fn insert(&mut self, path: impl Into<PathBuf>, content: String) {
        self.files.insert(path.into(), content);
    }
}

fn command_path(path: &[&str]) -> String {
    path.join(" ")
}

fn visible_commands<'a>(
    command: &'a Command,
    prefix: &mut Vec<&'a str>,
    out: &mut BTreeMap<String, &'a Command>,
) {
    for sub in real_subcommands(command) {
        prefix.push(sub.get_name());
        out.insert(command_path(prefix), sub);
        visible_commands(sub, prefix, out);
        prefix.pop();
    }
}

fn command_map(root: &Command) -> BTreeMap<String, &Command> {
    let mut commands = BTreeMap::new();
    visible_commands(root, &mut Vec::new(), &mut commands);
    commands
}

fn command_docs_map() -> Result<BTreeMap<&'static str, &'static CommandDoc>> {
    command_docs_map_from(COMMAND_DOCS)
}

fn command_docs_map_from(
    registry: &'static [CommandDoc],
) -> Result<BTreeMap<&'static str, &'static CommandDoc>> {
    let mut docs = BTreeMap::new();
    for doc in registry {
        if docs.insert(doc.path, doc).is_some() {
            bail!("duplicate command documentation: gat {}", doc.path);
        }
    }
    Ok(docs)
}

fn validate_command_docs(root: &Command) -> Result<()> {
    let commands = command_map(root);
    let docs = command_docs_map()?;
    let command_paths = commands.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let doc_paths = docs.keys().copied().collect::<BTreeSet<_>>();
    let mut problems = Vec::new();

    for path in command_paths.difference(&doc_paths) {
        problems.push(format!("undocumented command: gat {path}"));
    }

    for path in doc_paths.difference(&command_paths) {
        problems.push(format!("stale documentation:  gat {path}"));
    }

    fn validate_config_value(spec: &ConfigKeySpec, value: ConfigDocValue) -> Result<()> {
        match (spec.value, value) {
            (ConfigValueSpec::Boolean, ConfigDocValue::Boolean(_))
            | (
                ConfigValueSpec::String
                | ConfigValueSpec::Path
                | ConfigValueSpec::GitLocation
                | ConfigValueSpec::RemoteUrl,
                ConfigDocValue::String(_),
            ) => Ok(()),
            (ConfigValueSpec::Unsigned { min, max }, ConfigDocValue::Unsigned(value))
                if min.is_none_or(|min| value >= min) && max.is_none_or(|max| value <= max) =>
            {
                Ok(())
            }
            (ConfigValueSpec::Enum { values }, ConfigDocValue::String(value))
                if values.contains(&value) =>
            {
                if spec.path.canonical() == "cache.ingest_strategy" {
                    value
                        .parse::<config::IngestStrategy>()
                        .context("parse cache.ingest_strategy example")?;
                }
                Ok(())
            }
            (
                ConfigValueSpec::List {
                    element,
                    empty,
                    unique,
                    ..
                },
                ConfigDocValue::List(values),
            ) => {
                if values.is_empty() && empty == EmptyListPolicy::Forbidden {
                    bail!("{} example must not be empty", spec.path.canonical());
                }
                if unique && values.iter().collect::<BTreeSet<_>>().len() != values.len() {
                    bail!("{} example contains duplicates", spec.path.canonical());
                }
                match element {
                    ConfigElementSpec::String => {}
                    ConfigElementSpec::Glob => {
                        for value in values {
                            gat_core::globs::GatGlobPattern::parse(value)
                                .with_context(|| format!("invalid glob example `{value}`"))?;
                        }
                    }
                    ConfigElementSpec::GitIgnorePattern => {
                        config::validate_ignore_patterns(
                            values.iter().map(|value| (*value).to_string()).collect(),
                        )
                        .context("invalid Git-ignore example")?;
                    }
                    ConfigElementSpec::MaterializationMode => {
                        config::MaterializationStrategy::from_values(values)
                            .context("invalid materialization example")?;
                    }
                }
                Ok(())
            }
            _ => bail!(
                "{} has an example incompatible with its declared type",
                spec.path.canonical()
            ),
        }
    }

    fn validate_config_docs(commands: &BTreeMap<String, &Command>) -> Result<()> {
        let mut keys = BTreeSet::new();
        for spec in CONFIG_KEYS {
            if !keys.insert(spec.path.canonical()) {
                bail!(
                    "duplicate configuration documentation: {}",
                    spec.path.canonical()
                );
            }
            render_setter(spec, commands)
                .with_context(|| format!("invalid setter for {}", spec.path.canonical()))?;
            for example in spec.examples {
                validate_config_value(spec, *example)?;
            }
        }
        Ok(())
    }

    validate_config_docs(&commands)?;

    for (path, command) in &commands {
        if command.get_about().is_none() {
            problems.push(format!("missing description:   gat {path}"));
        }
        for arg in command.get_arguments() {
            if matches!(
                arg.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            ) {
                continue;
            }
            if arg.get_help().is_none() {
                problems.push(format!(
                    "missing arg help:      gat {path} {}",
                    render_arg_name(arg)
                ));
            }
        }
    }

    for doc in COMMAND_DOCS {
        if doc.examples.is_empty()
            && commands
                .get(doc.path)
                .is_some_and(|command| !command.is_subcommand_required_set())
        {
            problems.push(format!("command without example: gat {}", doc.path));
        }
        for example in doc.examples {
            validate_example(root, doc.path, example).with_context(|| {
                format!("invalid example under gat {}: {}", doc.path, example.title)
            })?;
        }
        if let Some(command) = commands.get(doc.path) {
            expand_checked_references(doc.overview, &commands)
                .with_context(|| format!("invalid reference in gat {} overview", doc.path))?;
            for example in doc.examples {
                expand_checked_references(example.explanation, &commands).with_context(|| {
                    format!(
                        "invalid reference in gat {} example {}",
                        doc.path, example.title
                    )
                })?;
            }
            let _ = command;
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        bail!(
            "documentation validation failed\n\n  {}",
            problems.join("\n  ")
        )
    }
}

/// Generic presentation logic for one [`lifecycle::DocWarning`]'s plain
/// text -- Mintlify's `<Warning>` markup is a renderer concern, kept here
/// rather than in the lifecycle model, which only produces plain text.
fn render_warning(text: &str, out: &mut String) {
    out.push_str("<Warning>\n");
    out.push_str(text);
    out.push_str("\n</Warning>\n\n");
}

/// Appends a `## gat <slug> <sub>` section per real subcommand of `cmd`
/// (and recurses into grandchildren the same way), so nested subcommands
/// still get their own heading and full help text on the same page. Unlike
/// the page's own top-level help, a subcommand's about text isn't shown
/// anywhere else on the page, so it's kept here.
/// Excludes clap's auto-generated `help` subcommand (and anything marked
/// hidden), which isn't a real user-facing command worth its own doc page.
fn real_subcommands(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands()
        .filter(|s| s.get_name() != "help" && !s.is_hide_set())
}

/// clap's default long-help template (used e.g. by [`Command::render_long_help`]):
/// about paragraph, blank line, usage, blank line, then args/subcommands.
/// See `DEFAULT_TEMPLATE` in `clap_builder`'s `output::help_template`.
const USAGE_TEMPLATE: &str = "{before-help}{usage-heading} {usage}\n\n{all-args}{after-help}";

/// Fixed line width for rendered `--help` usage blocks so output does not
/// depend on the terminal width of the machine running this tool.
const USAGE_WIDTH: usize = 80;

/// Keeps the command synopsis visible and puts full help behind an accordion.
/// Both forms come from clap so flags and constraints stay authoritative.
fn render_usage(cmd: &Command) -> String {
    let usage = cmd
        .clone()
        .term_width(USAGE_WIDTH)
        .help_template(USAGE_TEMPLATE)
        .render_long_help()
        .to_string()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let synopsis = cmd.clone().render_usage().to_string();
    let id = cmd
        .get_bin_name()
        .unwrap_or_else(|| cmd.get_name())
        .replace(' ', "-");
    format!(
        "```text\n{synopsis}\n```\n\n<Accordion title=\"All options\" id=\"{id}-options\">\n\n```text\n{usage}\n```\n\n</Accordion>\n"
    )
}

fn shell_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._~:@%+=,-".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\"'\"'"))
    }
}

fn render_examples(
    examples: &[CommandExample],
    commands: &BTreeMap<String, &Command>,
    out: &mut String,
) -> Result<()> {
    if examples.len() > 1 {
        out.push_str("<Tabs sync={false}>\n");
    }
    for example in examples {
        if examples.len() > 1 {
            out.push_str(&format!("<Tab title=\"{}\">\n\n", example.title));
        }
        if !example.explanation.is_empty() {
            out.push_str(&expand_checked_references(example.explanation, commands)?);
            out.push_str("\n\n");
        }
        let argv = example
            .argv
            .iter()
            .map(|arg| shell_arg(arg))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("```sh\ngat {argv}\n```\n\n"));
        if examples.len() > 1 {
            out.push_str("</Tab>\n");
        }
    }
    if examples.len() > 1 {
        out.push_str("</Tabs>\n\n");
    }
    Ok(())
}

fn render_arg_name(arg: &clap::Arg) -> String {
    if let Some(long) = arg.get_long() {
        format!("--{long}")
    } else if let Some(short) = arg.get_short() {
        format!("-{short}")
    } else if let Some([names_0, ..]) = arg.get_value_names() {
        format!("<{names_0}>")
    } else {
        format!("<{}>", arg.get_id())
    }
}

fn find_command<'a>(
    commands: &'a BTreeMap<String, &'a Command>,
    path: &str,
) -> Result<&'a Command> {
    commands
        .get(path)
        .copied()
        .with_context(|| format!("unknown command reference: gat {path}"))
}

fn expand_checked_references(
    source: &str,
    commands: &BTreeMap<String, &Command>,
) -> Result<String> {
    let mut out = String::new();
    let mut rest = source;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let token = &rest[start + 2..];
        let Some(end) = token.find("}}") else {
            bail!("unterminated documentation reference");
        };
        let reference = &token[..end];
        if let Some(path) = reference.strip_prefix("command:") {
            find_command(commands, path)?;
            let page = path
                .split_whitespace()
                .next()
                .context("empty command path")?;
            let anchor = if path.contains(' ') {
                format!("#gat-{}", path.replace(' ', "-"))
            } else {
                String::new()
            };
            out.push_str(&format!("[gat {path}](/commands/{page}{anchor})"));
        } else if let Some(reference) = reference.strip_prefix("arg:") {
            let Some((path, id)) = reference.rsplit_once(':') else {
                bail!("invalid argument reference: {reference}");
            };
            let command = find_command(commands, path)?;
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id().as_str() == id)
                .with_context(|| format!("unknown argument reference: gat {path}:{id}"))?;
            out.push('`');
            out.push_str(&render_arg_name(arg));
            out.push('`');
        } else {
            bail!("unknown documentation reference: {reference}");
        }
        rest = &token[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn parsed_command_path(root: &Command, argv: &[&str]) -> Result<String> {
    let matches = root
        .clone()
        .try_get_matches_from(std::iter::once("gat").chain(argv.iter().copied()))?;
    let mut path = Vec::new();
    let mut current = &matches;
    while let Some((name, sub)) = current.subcommand() {
        path.push(name);
        current = sub;
    }
    Ok(path.join(" "))
}

fn validate_example(root: &Command, owner: &str, example: &CommandExample) -> Result<()> {
    cli::Cli::try_parse_from(std::iter::once("gat").chain(example.argv.iter().copied()))?;
    let parsed = parsed_command_path(root, example.argv)?;
    if parsed != owner {
        bail!("example resolves to gat {parsed}, expected gat {owner}");
    }
    Ok(())
}

fn render_command_page(
    top_path: &str,
    commands: &BTreeMap<String, &Command>,
    docs: &BTreeMap<&str, &CommandDoc>,
) -> Result<String> {
    let command = find_command(commands, top_path)?;
    let doc = docs[top_path];
    let about = command
        .get_about()
        .expect("validated description")
        .to_string();
    let doc_page = lifecycle::command_page(top_path);
    let mut body = String::new();
    for warning in &doc_page.warnings {
        render_warning(&warning.text, &mut body);
    }
    body.push_str(expand_checked_references(doc.overview, commands)?.trim());
    if !doc.overview.trim().is_empty() {
        body.push_str("\n\n");
    }
    body.push_str("## Usage\n\n");
    body.push_str(&render_usage(command));
    if !doc.examples.is_empty() {
        body.push_str("\n## Examples\n\n");
        render_examples(doc.examples, commands, &mut body)?;
    }

    if real_subcommands(command).next().is_some() {
        body.push_str("\n## Subcommands\n\n<CardGroup cols={2}>\n");
        for subcommand in real_subcommands(command) {
            let name = subcommand.get_name();
            body.push_str(&format!(
                "  <Card title=\"gat {top_path} {name}\" href=\"#gat-{top_path}-{name}\" />\n"
            ));
        }
        body.push_str("</CardGroup>\n\n");
    }

    for (path, subcommand) in commands {
        if path.split_whitespace().next() != Some(top_path) || path == top_path {
            continue;
        }
        let depth = path.split_whitespace().count();
        let heading = "#".repeat(depth);
        body.push_str(&format!("{heading} gat {path}\n\n"));
        let sub_doc = docs[path.as_str()];
        let overview = expand_checked_references(sub_doc.overview, commands)?;
        if overview.trim().is_empty() {
            body.push_str(
                &subcommand
                    .get_about()
                    .expect("validated description")
                    .to_string(),
            );
        } else {
            body.push_str(overview.trim());
        }
        body.push_str("\n\n");
        body.push_str(&format!("{} Usage\n\n", "#".repeat(depth + 1)));
        body.push_str(&render_usage(subcommand));
        body.push_str(&format!("\n{} Examples\n\n", "#".repeat(depth + 1)));
        render_examples(sub_doc.examples, commands, &mut body)?;
    }

    Ok(render_mdx(
        &format!("gat {top_path}"),
        &about,
        &body,
        doc_page.tag,
    ))
}

/// Renders one MDX page's frontmatter + body. `tag`, if present, is the
/// generic status label (e.g. `"Experimental"`/`"Deprecated"`) a page's
/// own top-level surface carries -- supplied by
/// [`lifecycle::DocPage::tag`], never derived here from lifecycle status.
fn render_mdx(title: &str, about: &str, body: &str, tag: Option<&str>) -> String {
    let tag_line = match tag {
        Some(tag) => format!("tag: \"{tag}\"\n"),
        None => String::new(),
    };
    format!(
        "---\ntitle: \"{}\"\ndescription: \"{}\"\n{tag_line}---\n\n{}\n",
        title.replace('"', "\\\""),
        about.replace('"', "\\\""),
        body.trim_end()
    )
}

/// Replaces the `"Commands"` group's `pages` array in a parsed docs.json,
/// leaving every other key (colors, logo, Getting Started group, ...)
/// untouched. Panics with a clear message if the group is missing.
fn set_commands_pages(docs_json: &mut Value, pages: Vec<Value>) {
    let groups = docs_json["navigation"]["groups"]
        .as_array_mut()
        .expect("docs.json: navigation.groups must be an array");
    let commands_group = groups
        .iter_mut()
        .find(|g| g["group"] == "Commands")
        .expect("docs.json: missing a navigation group named \"Commands\"");
    commands_group["pages"] = Value::Array(pages);
}

/// A `Config` with every key set, serialized with `serde/yaml_serde` (same
/// path `Repo::save_config` uses) so the example is guaranteed to actually
/// parse — not hand-typed YAML that could silently drift from the real
/// schema.
fn complete_config() -> Config {
    Config {
        remotes: RemotesConfig {
            default: Some(RemoteName::from_string("origin".to_string())),
            by_name: BTreeMap::from([
                (
                    RemoteName::from_string("origin".to_string()),
                    RemoteUrlTemplate::from_string(
                        "s3://bucket/prefix?region=eu-west-1".to_string(),
                    )
                    .into(),
                ),
                (
                    RemoteName::from_string("backup".to_string()),
                    RemoteUrlTemplate::from_string("file:///mnt/backup/prefix".to_string()).into(),
                ),
            ]),
        },
        cache: CacheConfig {
            location: Some(gat_core::cache_location::CacheLocation::from_path(
                "/var/cache/gat".into(),
            )),
            materialization_strategy: Some(
                config::MaterializationStrategy::from_values(&["hardlink", "copy"])
                    .expect("valid example materialization_strategy"),
            ),
            ingest_strategy: Some(config::IngestStrategy::Hybrid),
        },
        sync: SyncConfig {
            trust_state: Some(true),
            auto_fetch: Some(true),
            auto_repair: Some(true),
        },
        selections: gat_core::config::SelectionsConfig {
            default: Some("runtime".into()),
            by_name: std::collections::BTreeMap::from([(
                "runtime".into(),
                config::SelectionConfig {
                    path: gat_core::lexical_path::GatSubpath::normalize("data").unwrap(),
                    include: Some(vec![gat_core::globs::GatGlobPattern::parse("**").unwrap()]),
                    exclude: Some(vec![
                        gat_core::globs::GatGlobPattern::parse("tmp/**").unwrap(),
                    ]),
                },
            )]),
        },
        lock: LockConfig {
            shard_levels: Some(gat_core::lock::LockShardLevels::new(2).unwrap()),
        },
        mounts: MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("resnet50".to_string()),
                MountConfig {
                    url: GitLocationSpec::from_string("../models".to_string()),
                    target: gat_core::lexical_path::GatPath::parse_canonical("releases/resnet50")
                        .unwrap(),
                    path: gat_core::lexical_path::GatSubpath::Path(
                        gat_core::lexical_path::GatPath::parse_canonical("exports/resnet50")
                            .unwrap(),
                    ),
                    rev: Some("main".to_string().into()),
                    rev_lock: Some("6c73875a1b2c3d4e5f60718293a4b5c6d7e8f901".parse().unwrap()),
                    include: vec![gat_core::globs::GatGlobPattern::parse("**/*.onnx").unwrap()],
                    exclude: vec![gat_core::globs::GatGlobPattern::parse("tests/**").unwrap()],
                },
            )]),
        },
        routes: config::RoutesConfig {
            by_name: BTreeMap::from([
                (
                    RouteName::from_string("datasets".to_string()),
                    config::RouteConfig {
                        path: gat_core::lexical_path::GatPath::parse_canonical("datasets").unwrap(),
                        remote: RemoteName::from_string("backup".to_string()),
                    },
                ),
                (
                    RouteName::from_string("resnet50".to_string()),
                    config::RouteConfig {
                        path: gat_core::lexical_path::GatPath::parse_canonical("releases/resnet50")
                            .unwrap(),
                        remote: RemoteName::from_string("origin".to_string()),
                    },
                ),
            ]),
        },
        git: GitConfig {
            ignore_patterns: Some(vec![
                gat_core::git_ignore::GitIgnorePattern::parse("*.safetensors").unwrap(),
                gat_core::git_ignore::GitIgnorePattern::parse("/artifacts/**/*.bin").unwrap(),
            ]),
        },
    }
}

fn typical_config() -> Config {
    Config {
        remotes: RemotesConfig {
            default: Some(RemoteName::from_string("origin".to_string())),
            by_name: BTreeMap::from([(
                RemoteName::from_string("origin".to_string()),
                RemoteUrlTemplate::from_string(
                    "s3://example-bucket/project?region=eu-west-1".to_string(),
                )
                .into(),
            )]),
        },
        cache: CacheConfig {
            location: None,
            materialization_strategy: Some(
                config::MaterializationStrategy::from_values(&["reflink", "copy"]).unwrap(),
            ),
            ingest_strategy: None,
        },
        sync: SyncConfig {
            auto_fetch: Some(true),
            ..SyncConfig::default()
        },
        selections: gat_core::config::SelectionsConfig {
            default: Some("runtime".into()),
            by_name: std::collections::BTreeMap::from([(
                "runtime".into(),
                config::SelectionConfig {
                    exclude: Some(vec![
                        gat_core::globs::GatGlobPattern::parse("tmp/**").unwrap(),
                    ]),
                    ..config::SelectionConfig::default()
                },
            )]),
        },
        ..Config::default()
    }
}

fn validated_yaml(config: &Config) -> Result<String> {
    let temp = tempfile::tempdir().context("create temporary config directory")?;
    let path = temp.path().join("gat.yaml");
    gat_engine::save_config_file(&path, config).context("serialize example config")?;
    let yaml = fs::read_to_string(&path).context("read serialized example config")?;
    let parsed = gat_engine::load_config_file(&path).context("parse serialized example config")?;
    if parsed != *config {
        bail!("configuration example did not round-trip");
    }
    Ok(yaml)
}

fn render_doc_value(value: ConfigDocValue) -> String {
    match value {
        ConfigDocValue::Boolean(value) => format!("`{value}`"),
        ConfigDocValue::Unsigned(value) => format!("`{value}`"),
        ConfigDocValue::String(value) => format!("`{value}`"),
        ConfigDocValue::List(values) => {
            format!(
                "[{}]",
                values
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn render_value_type(value: ConfigValueSpec) -> (&'static str, Vec<String>) {
    match value {
        ConfigValueSpec::Boolean => ("boolean", vec!["Values: `true` | `false`".to_string()]),
        ConfigValueSpec::String => ("string", Vec::new()),
        ConfigValueSpec::Path => ("path", Vec::new()),
        ConfigValueSpec::GitLocation => ("Git location", Vec::new()),
        ConfigValueSpec::RemoteUrl => ("remote URL", Vec::new()),
        ConfigValueSpec::Unsigned { min, max } => {
            let range = match (min, max) {
                (Some(min), Some(max)) => format!("Range: `{min}`-`{max}`"),
                (Some(min), None) => format!("Minimum: `{min}`"),
                (None, Some(max)) => format!("Maximum: `{max}`"),
                (None, None) => String::new(),
            };
            (
                "integer",
                (!range.is_empty()).then_some(range).into_iter().collect(),
            )
        }
        ConfigValueSpec::Enum { values } => (
            "enum",
            vec![format!(
                "Values: {}",
                values
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(" | ")
            )],
        ),
        ConfigValueSpec::List {
            element,
            empty,
            ordered,
            unique,
        } => {
            let element = match element {
                ConfigElementSpec::String => "string",
                ConfigElementSpec::Glob => "glob",
                ConfigElementSpec::GitIgnorePattern => "Git-ignore pattern",
                ConfigElementSpec::MaterializationMode => "materialization mode",
            };
            let mut constraints = vec![format!("Elements: {element}")];
            if element == "materialization mode" {
                constraints.push(format!(
                    "Values: {}",
                    config::MaterializationMode::ALL
                        .iter()
                        .map(|mode| format!("`{}`", mode.as_str()))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
            constraints.push(match empty {
                EmptyListPolicy::Allowed => "Empty list: allowed".to_string(),
                EmptyListPolicy::MeansEverything => "Empty list: selects everything".to_string(),
                EmptyListPolicy::Forbidden => "Empty list: not allowed".to_string(),
            });
            if ordered {
                constraints.push("Order is significant.".to_string());
            }
            if unique {
                constraints.push("Duplicate values are not allowed.".to_string());
            }
            ("list", constraints)
        }
    }
}

fn render_persisted_default(default: ConfigDefault) -> String {
    match default.persisted {
        PersistedDefault::Unset => "unset".to_string(),
        PersistedDefault::Value(value) => render_doc_value(value),
    }
}

fn render_effective_default(default: ConfigDefault) -> Option<String> {
    match default.effective {
        EffectiveDefault::SameAsPersisted => None,
        EffectiveDefault::Value(value) => Some(render_doc_value(value)),
        EffectiveDefault::Derived(value) => Some(format!("`{value}`")),
    }
}

fn render_setter(spec: &ConfigKeySpec, commands: &BTreeMap<String, &Command>) -> Result<String> {
    match spec.setter {
        ConfigSetter::Command { path } => {
            find_command(commands, path)?;
            let command = format!("gat {path}");
            let top = path.split_whitespace().next().expect("validated command");
            let href = if path == top {
                format!("/commands/{top}")
            } else {
                format!("/commands/{top}#gat-{}", path.replace(' ', "-"))
            };
            if spec.write_surface == ConfigWriteSurface::Config {
                let value = match spec.value.cardinality() {
                    gat_core::config_keys::ValueCardinality::Scalar => "<value>",
                    gat_core::config_keys::ValueCardinality::List => "<value>...",
                };
                Ok(format!(
                    "[`{command} {} {value}`]({href})",
                    spec.path.canonical()
                ))
            } else {
                Ok(format!("[`{command}`]({href})"))
            }
        }
        ConfigSetter::Automatic { description } => expand_checked_references(description, commands),
    }
}

fn render_named_schema(section: ConfigSection, out: &mut String) {
    let specs = CONFIG_KEYS
        .iter()
        .filter(|spec| spec.section == section)
        .collect::<Vec<_>>();
    if !specs
        .iter()
        .any(|spec| matches!(spec.path, gat_core::config_keys::ConfigPath::Named { .. }))
    {
        return;
    }
    let section_name = match section {
        ConfigSection::Remotes => "remotes",
        ConfigSection::Mounts => "mounts",
        ConfigSection::Routes => "routes",
        _ => return,
    };
    out.push_str("```yaml\n");
    out.push_str(section_name);
    out.push_str(":\n");
    let mut named_started = false;
    for spec in specs {
        match spec.path {
            gat_core::config_keys::ConfigPath::Static(path) => {
                let field = path.rsplit('.').next().unwrap();
                out.push_str(&format!("  {field}: <value>\n"));
            }
            gat_core::config_keys::ConfigPath::Named { field: None, .. } => {
                out.push_str("  <name>: <remote-url>\n");
            }
            gat_core::config_keys::ConfigPath::Named {
                field: Some(field), ..
            } => {
                if !named_started {
                    out.push_str("  <name>:\n");
                    named_started = true;
                }
                out.push_str(&format!("    {field}: <value>\n"));
            }
        }
    }
    out.push_str("```\n\n");
}

fn config_field_id(key: &str) -> String {
    let mut id = String::new();
    let mut separator = false;
    for character in key.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !id.is_empty() {
                id.push('-');
            }
            id.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    id
}

fn lifecycle_badge(status: lifecycle::Status) -> String {
    let color = match status {
        lifecycle::Status::Stable => "green",
        lifecycle::Status::Experimental => "yellow",
        lifecycle::Status::Deprecated => "orange",
    };
    format!(
        r#"<Badge color="{color}" size="sm">{}</Badge>"#,
        status.label()
    )
}

fn warning_detail(text: &str) -> &str {
    text.split_once(": ").map_or(text, |(_, detail)| detail)
}

fn config_value_lifecycle_status(key: &str, value: &str) -> Option<lifecycle::Status> {
    lifecycle::REGISTRY
        .iter()
        .find_map(|spec| match spec.surface {
            lifecycle::Surface::ConfigValue {
                key: spec_key,
                value: spec_value,
            } if spec_key == key && spec_value == value => Some(spec.status),
            _ => None,
        })
}

fn config_key_lifecycle_warning(key: &str) -> Option<String> {
    let page = lifecycle::config_key(key);
    for warning in &page.warnings {
        if !matches!(warning.surface, lifecycle::Surface::ConfigKey(_)) {
            continue;
        }
        return Some(warning.text.clone());
    }
    None
}

fn render_lifecycle_values(key: &str, values: &[&str], out: &mut String) {
    out.push_str("  **Values:**\n\n");
    for value in values {
        out.push_str(&format!("  - `{value}`"));
        if let Some(status) = config_value_lifecycle_status(key, value) {
            out.push(' ');
            out.push_str(&lifecycle_badge(status));
        }
        out.push('\n');
    }
    out.push_str(
        "\n  See [Feature lifecycle](/references/feature-lifecycle) for the guarantees attached \
         to each status.\n\n",
    );
}

fn render_constraint(constraint: &str, out: &mut String) {
    if let Some((label, value)) = constraint.split_once(": ") {
        out.push_str(&format!("    - **{label}:** {value}\n"));
    } else {
        out.push_str(&format!("    - {constraint}\n"));
    }
}

fn render_configuration_field(
    spec: &ConfigKeySpec,
    commands: &BTreeMap<String, &Command>,
    out: &mut String,
) -> Result<()> {
    let key = spec.path.canonical();
    let (kind, mut constraints) = render_value_type(spec.value);
    let lifecycle_warning = config_key_lifecycle_warning(key);
    out.push_str(&format!(r#"<ResponseField name="{key}" type="{kind}">"#));
    if let Some(text) = lifecycle_warning {
        out.push_str(&format!("\n<Warning>\n{text}\n</Warning>\n"));
    }
    out.push_str(&format!("\n  {}\n\n", spec.description));

    if let ConfigValueSpec::Enum { values } = spec.value
        && values
            .iter()
            .any(|value| config_value_lifecycle_status(key, value).is_some())
    {
        constraints.retain(|constraint| !constraint.starts_with("Values: "));
        render_lifecycle_values(key, values, out);
    }

    if let Some(effective_default) = render_effective_default(spec.default) {
        out.push_str(&format!("  **Default:** {effective_default}\n\n"));
    }

    out.push_str(&format!(
        "  **Set with:** {}\n\n",
        render_setter(spec, commands)?
    ));

    for warning in &lifecycle::config_key(key).warnings {
        if matches!(
            warning.surface,
            lifecycle::Surface::ConfigKey(_) | lifecycle::Surface::ConfigValue { .. }
        ) {
            continue;
        }
        let subject = match warning.surface {
            lifecycle::Surface::ConfigAlias { alias, .. } => format!("`{alias}`"),
            lifecycle::Surface::ConfigKey(key) => format!("`{key}`"),
            _ => continue,
        };
        let status = match warning.tag {
            "Experimental" => lifecycle::Status::Experimental,
            "Deprecated" => lifecycle::Status::Deprecated,
            tag => bail!("unsupported lifecycle documentation tag `{tag}`"),
        };
        out.push_str(&format!(
            "  {subject} {} — {}\n\n",
            lifecycle_badge(status),
            warning_detail(&warning.text)
        ));
    }

    out.push_str(&format!(
        "  <Accordion title=\"Configuration details\" id=\"{}\">\n",
        config_field_id(key)
    ));
    for constraint in constraints {
        render_constraint(&constraint, out);
    }
    out.push_str(&format!(
        "    - **Persisted default:** {}\n",
        render_persisted_default(spec.default)
    ));
    if let Some(environment) = spec.environment_override {
        out.push_str(&format!(
            "    - **Environment override:** `{environment}` (not persisted to `gat.yaml`)\n"
        ));
    }
    if !spec.examples.is_empty() {
        out.push_str(&format!(
            "    - **Example:** {}\n",
            spec.examples
                .iter()
                .copied()
                .map(render_doc_value)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    out.push_str("  </Accordion>\n</ResponseField>\n\n");
    Ok(())
}

fn render_configuration_mdx(commands: &BTreeMap<String, &Command>) -> Result<String> {
    let mut out = String::from(
        "---\ntitle: \"Configuration\"\ndescription: \"Settings, defaults, and examples for gat.yaml.\"\n---\n\n",
    );
    out.push_str(
        "## How configuration works\n\nUnset keys are normally omitted from `gat.yaml`. Reads use \
         the <Tooltip tip=\"The values Gat uses after built-in defaults and all configuration \
         layers are resolved.\">effective configuration</Tooltip>; mutating commands write only \
         the selected layer, with Project as the default.\n\n",
    );
    out.push_str("## Configuration locations\n\n");
    let mut scopes = config::ConfigScope::ALL;
    scopes.sort_by_key(|scope| scope.precedence());
    out.push_str("<Columns cols={3}>\n");
    for scope in scopes {
        let doc = scope.documentation();
        let title = doc.name;
        let icon = match scope {
            config::ConfigScope::Global => "user",
            config::ConfigScope::Project => "folder",
            config::ConfigScope::Local => "laptop",
        };
        out.push_str(&format!(
            "  <Card title=\"{title}\" icon=\"{icon}\">\n    `{}`\n\n    {}\n\n    \
             **Committed:** {}\n  </Card>\n",
            doc.path,
            doc.intended_use,
            if doc.committed { "Yes" } else { "No" }
        ));
    }
    out.push_str("</Columns>\n\n<Note>\nConfiguration precedence is **Global → Project → Local**. Named selections, mounts, routes, and remotes replace whole definitions on a same-name collision; different names coexist. Optional default pointers for selections and remotes inherit independently. Adding a resource never chooses a default. Other settings inherit field by field.\n</Note>\n\n");
    out.push_str("Use `gat config` for general settings and `gat remote`, `gat mount`, `gat route`, or `gat selection` for named resources. See [Config inheritance](/concepts/config-inheritance) for overrides and defaults.\n\n");

    out.push_str("## Configuration examples\n\n<CodeGroup>\n```yaml Typical\n");
    out.push_str(&validated_yaml(&typical_config())?);
    out.push_str("```\n\n```yaml Complete\n");
    out.push_str(&validated_yaml(&complete_config())?);
    out.push_str("```\n</CodeGroup>\n\n## Configuration reference\n\n");
    for section in ConfigSection::ALL {
        out.push_str(&format!("### {}\n\n", section.title()));
        if section == ConfigSection::Remotes {
            let guide = expand_checked_references(CONFIGURATION_REMOTE_GUIDE, commands)?;
            out.push_str(&guide);
            out.push('\n');
        }
        render_named_schema(section, &mut out);
        for spec in CONFIG_KEYS.iter().filter(|spec| spec.section == section) {
            render_configuration_field(spec, commands, &mut out)?;
        }
    }
    out.truncate(out.trim_end_matches('\n').len());
    out.push('\n');
    Ok(out)
}

fn generate_docs(docs_dir: &Path) -> Result<GeneratedDocs> {
    let mut root = cli::Cli::command();
    root.build();
    validate_command_docs(&root)?;
    let commands = command_map(&root);
    let docs = command_docs_map()?;
    let mut generated = GeneratedDocs::default();
    let mut pages = Vec::new();

    let visible_commands: BTreeSet<_> = real_subcommands(&root).map(Command::get_name).collect();
    for path in visible_commands {
        generated.insert(
            PathBuf::from("commands").join(format!("{path}.mdx")),
            render_command_page(path, &commands, &docs)?,
        );
        pages.push(Value::String(format!("commands/{path}")));
    }

    let docs_json_path = docs_dir.join("docs.json");
    let text = fs::read_to_string(&docs_json_path)
        .with_context(|| format!("read {}", docs_json_path.display()))?;
    let mut docs_json: Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", docs_json_path.display()))?;
    set_commands_pages(&mut docs_json, pages);
    let mut out = serde_json::to_string_pretty(&docs_json).context("serialize docs.json")?;
    out.push('\n');
    generated.insert("docs.json", out);
    generated.insert(
        "references/configuration.mdx",
        render_configuration_mdx(&commands)?,
    );
    Ok(generated)
}

fn write_docs(docs_dir: &Path, generated: &GeneratedDocs) -> Result<()> {
    let expected_commands = generated
        .files
        .keys()
        .filter(|path| path.starts_with("commands"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let commands_dir = docs_dir.join("commands");
    if commands_dir.exists() {
        for entry in fs::read_dir(&commands_dir).context("read docs/commands")? {
            let entry = entry?;
            let path = PathBuf::from("commands").join(entry.file_name());
            if entry.path().extension().is_some_and(|ext| ext == "mdx")
                && !expected_commands.contains(&path)
            {
                fs::remove_file(entry.path())
                    .with_context(|| format!("remove stale {}", entry.path().display()))?;
            }
        }
    }
    for (relative, content) in &generated.files {
        let path = docs_dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn check_docs(docs_dir: &Path, generated: &GeneratedDocs) -> Result<()> {
    let mut problems = Vec::new();
    for (relative, expected) in &generated.files {
        let path = docs_dir.join(relative);
        match fs::read_to_string(&path) {
            Ok(actual) if actual == *expected => {}
            Ok(_) => problems.push(format!("changed: {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                problems.push(format!("missing: {}", path.display()));
            }
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        }
    }
    let expected_commands = generated
        .files
        .keys()
        .filter(|path| path.starts_with("commands"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let commands_dir = docs_dir.join("commands");
    if commands_dir.exists() {
        for entry in fs::read_dir(&commands_dir).context("read docs/commands")? {
            let entry = entry?;
            let relative = PathBuf::from("commands").join(entry.file_name());
            if entry.path().extension().is_some_and(|ext| ext == "mdx")
                && !expected_commands.contains(&relative)
            {
                problems.push(format!("stale:   {}", entry.path().display()));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        bail!(
            "docs are out of date\n\n  {}\n\nrun: task docs:generate",
            problems.join("\n  ")
        )
    }
}

fn main() -> Result<()> {
    let options = Options::parse();
    let workspace_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("gat-tools must live directly beneath the workspace root")?;
    let docs_dir = workspace_dir.join("docs");
    let generated = generate_docs(&docs_dir)?;
    if options.check {
        check_docs(&docs_dir, &generated)?;
    } else {
        write_docs(&docs_dir, &generated)?;
        println!(
            "generated {} documentation artifact(s)",
            generated.files.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn root() -> Command {
        let mut root = cli::Cli::command();
        root.build();
        root
    }

    #[test]
    fn command_registry_exactly_matches_visible_clap_paths() {
        let root = root();
        let actual = command_map(&root).into_keys().collect::<BTreeSet<_>>();
        let documented = COMMAND_DOCS
            .iter()
            .map(|doc| doc.path.to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, documented);
        assert_eq!(
            documented.len(),
            COMMAND_DOCS.len(),
            "duplicate command documentation paths"
        );
    }

    #[test]
    fn every_example_parses_and_resolves_to_its_owner() {
        let root = root();
        for doc in COMMAND_DOCS {
            for example in doc.examples {
                validate_example(&root, doc.path, example)
                    .unwrap_or_else(|err| panic!("{} / {}: {err:#}", doc.path, example.title));
            }
        }
    }

    #[test]
    fn stale_status_positional_fails_but_path_option_succeeds() {
        assert!(cli::Cli::try_parse_from(["gat", "status", "data/"]).is_err());
        assert!(cli::Cli::try_parse_from(["gat", "status", "--path", "data/"]).is_ok());
    }

    #[test]
    fn checked_references_expand_and_unknown_references_fail() {
        let root = root();
        let commands = command_map(&root);
        assert_eq!(
            expand_checked_references(
                "{{command:remote add}} uses {{arg:remote add:url}}",
                &commands
            )
            .unwrap(),
            "[gat remote add](/commands/remote#gat-remote-add) uses `<URL>`"
        );
        assert_eq!(
            expand_checked_references("{{command:fetch}}", &commands).unwrap(),
            "[gat fetch](/commands/fetch)"
        );
        assert!(expand_checked_references("{{command:remote move}}", &commands).is_err());
        assert!(expand_checked_references("{{arg:status:nope}}", &commands).is_err());
    }

    #[test]
    fn examples_render_checked_references_and_preserve_shell_arguments() {
        let root = root();
        let commands = command_map(&root);
        let examples = [CommandExample {
            title: "Read a remote",
            argv: &["remote", "show", "origin"],
            explanation: "Inspect with {{command:remote show}}.",
        }];
        let mut output = String::new();
        render_examples(&examples, &commands, &mut output).unwrap();
        assert!(
            output.contains("Inspect with [gat remote show](/commands/remote#gat-remote-show).")
        );
        assert!(output.contains("gat remote show origin"));
        assert!(!output.contains("<Tabs"));
        assert_eq!(shell_arg(""), "''");
        assert_eq!(shell_arg("**/*.bin"), "'**/*.bin'");
        assert_eq!(shell_arg("a'b"), "'a'\"'\"'b'");
        assert_eq!(shell_arg("${TOKEN}"), "'${TOKEN}'");
    }

    #[test]
    fn command_pages_keep_synopsis_and_full_help_with_independent_examples() {
        let root = root();
        let commands = command_map(&root);
        let docs = command_docs_map().unwrap();
        let page = render_command_page("status", &commands, &docs).unwrap();
        let accordion = page.find("<Accordion title=\"All options\"").unwrap();
        assert!(page[..accordion].contains("Usage: gat status [OPTIONS]"));
        assert!(page[accordion..].contains("--exclude-rev"));
        assert!(page.contains("<Tabs sync={false}>"));
        let remote = render_command_page("remote", &commands, &docs).unwrap();
        assert!(remote.contains("id=\"gat-remote-options\""));
        assert!(remote.contains("id=\"gat-remote-add-options\""));
    }

    #[test]
    fn duplicate_documentation_paths_are_rejected() {
        static EXAMPLES: &[CommandExample] = &[CommandExample {
            title: "example",
            argv: &["init"],
            explanation: "",
        }];
        static DUPLICATES: &[CommandDoc] = &[
            CommandDoc {
                path: "init",
                overview: "",
                examples: EXAMPLES,
            },
            CommandDoc {
                path: "init",
                overview: "",
                examples: EXAMPLES,
            },
        ];
        assert!(command_docs_map_from(DUPLICATES).is_err());
    }

    #[test]
    fn render_configuration_mdx_uses_schema_components_for_every_key() {
        let root = root();
        let commands = command_map(&root);
        let mdx = render_configuration_mdx(&commands).unwrap();
        for k in CONFIG_KEYS {
            let key = k.path.canonical();
            assert!(
                mdx.contains(&format!(r#"<ResponseField name="{key}" "#)),
                "missing {key}"
            );
            assert!(mdx.contains(&format!(r#"id="{}""#, config_field_id(key))));
        }
        assert_eq!(
            mdx.matches("<ResponseField name=").count(),
            CONFIG_KEYS.len()
        );
        assert_eq!(
            mdx.matches("<Accordion title=\"Configuration details\"")
                .count(),
            CONFIG_KEYS.len()
        );
        assert!(mdx.contains("<Columns cols={3}>"));
        assert!(mdx.contains("<Card title=\"Project (default)\" icon=\"folder\">"));
        assert!(mdx.contains("<Note>"));
        assert!(mdx.contains("<CodeGroup>"));
        assert!(mdx.contains("```yaml Typical"));
        assert!(mdx.contains("```yaml Complete"));
        assert!(mdx.contains(r#"<Badge color="yellow" size="sm">Experimental</Badge>"#));
        assert!(mdx.contains(r#"<Badge color="orange" size="sm">Deprecated</Badge>"#));
        assert!(mdx.contains(r#"<ResponseField name="lock.shard_levels" type="integer">"#));
        assert!(!mdx.contains("#### `"));
    }

    #[test]
    fn configuration_remote_guide_uses_checked_references() {
        let root = root();
        let commands = command_map(&root);
        let guide = expand_checked_references(CONFIGURATION_REMOTE_GUIDE, &commands).unwrap();
        let mdx = render_configuration_mdx(&commands).unwrap();
        assert_eq!(mdx.matches(guide.as_str()).count(), 1);
        assert!(!guide.contains("{{command:"));
        assert!(guide.contains("<Tooltip tip="));
        assert!(guide.contains("<Tabs sync={false}>"));
        for scheme in ["file://", "s3://", "azblob://", "gcs://", "oss://"] {
            assert!(guide.contains(scheme), "missing provider: {scheme}");
        }
    }

    #[test]
    fn configuration_remote_provider_examples_parse_as_remote_add() {
        let root = root();
        for (name, url) in [
            ("origin", "s3://example-bucket/project?region=eu-west-1"),
            (
                "minio",
                concat!(
                    "s3://example-bucket/project?region=us-east-1&endpoint=",
                    // hygiene-ok: parser-only documentation example; no network request.
                    "http://localhost",
                    // hygiene-ok: parser-only documentation example; no socket is opened.
                    ":9000",
                ),
            ),
            (
                "origin",
                // hygiene-ok: parser-only documentation example; no network request.
                "azblob://example-container/project?endpoint=https://myaccount.blob.core.windows.net",
            ),
            ("origin", "gcs://example-bucket/project"),
            (
                "origin",
                // hygiene-ok: parser-only documentation example; no network request.
                "oss://example-bucket/project?endpoint=https://oss-cn-hangzhou.aliyuncs.com",
            ),
            ("origin", "file:///mnt/backup/project"),
            // hygiene-ok: parser-only Windows documentation example; no filesystem access.
            ("origin", "file:///C:/gat-storage/project"),
        ] {
            let line = format!("gat remote add {name} '{url}'");
            assert!(CONFIGURATION_REMOTE_GUIDE.contains(&line), "missing {line}");
            let argv = ["remote", "add", name, url];
            assert!(cli::Cli::try_parse_from(std::iter::once("gat").chain(argv)).is_ok());
            assert_eq!(parsed_command_path(&root, &argv).unwrap(), "remote add");
        }
    }

    #[test]
    fn minimal_typical_and_complete_configs_round_trip_through_production_io() {
        let minimal = validated_yaml(&Config::default()).unwrap();
        assert_eq!(minimal, "version: 1\n");
        let typical = validated_yaml(&typical_config()).unwrap();
        assert!(typical.contains("url: s3://example-bucket/project?region=eu-west-1"));
        let yaml = validated_yaml(&complete_config()).unwrap();
        assert!(yaml.contains("default: origin"));
        assert!(yaml.contains("url: s3://bucket/prefix?region=eu-west-1"));
        assert!(yaml.contains("url: file:///mnt/backup/prefix"));
        // `cache.materialization_strategy` is a YAML sequence, never a comma-joined scalar.
        assert!(yaml.contains("materialization_strategy:\n"));
        assert!(yaml.contains("- hardlink"));
        assert!(yaml.contains("- copy"));
        assert!(yaml.contains("location: /var/cache/gat"));
    }

    #[test]
    fn write_and_check_modes_detect_changed_missing_and_stale_files_without_mutating() {
        let temp = tempfile::tempdir().unwrap();
        let docs_dir = temp.path();
        fs::write(
            docs_dir.join("docs.json"),
            r#"{"navigation":{"groups":[{"group":"Commands","pages":[]}]}}"#,
        )
        .unwrap();
        let generated = generate_docs(docs_dir).unwrap();
        write_docs(docs_dir, &generated).unwrap();
        check_docs(docs_dir, &generated).unwrap();

        let status = docs_dir.join("commands/status.mdx");
        fs::write(&status, "changed").unwrap();
        let stale = docs_dir.join("commands/stale.mdx");
        fs::write(&stale, "stale").unwrap();
        let before_status = fs::read(&status).unwrap();
        let before_stale = fs::read(&stale).unwrap();
        let err = check_docs(docs_dir, &generated).unwrap_err().to_string();
        assert!(err.contains("changed:"));
        assert!(err.contains("stale:"));
        assert_eq!(fs::read(&status).unwrap(), before_status);
        assert_eq!(fs::read(&stale).unwrap(), before_stale);
    }
}
