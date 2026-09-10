//! Command-only policy. Engine reads always receive a resolved predicate.
use gat_core::{config::Config, selection::Selection};
use std::borrow::Cow;

/// Keep the effective predicate and its coverage metadata consistent.
pub(crate) struct ResolvedSelection<'a> {
    pub selection: Cow<'a, Selection>,
    pub scope: SelectionScope,
}

impl<'a> ResolvedSelection<'a> {
    fn new(selection: Cow<'a, Selection>, origin: SelectionScope) -> Self {
        let scope = if selection.is_unrestricted() {
            SelectionScope::Unrestricted
        } else {
            origin
        };
        Self { selection, scope }
    }
}

/// Omission uses the complete configured selection. Any explicit selection
/// replaces it, including an unrestricted root selection.
pub(crate) fn resolve<'a>(
    selection: Option<&'a Selection>,
    config: &Config,
) -> Result<ResolvedSelection<'a>, gat_engine::RepositoryError> {
    if let Some(selection) = selection {
        Ok(ResolvedSelection::new(
            Cow::Borrowed(selection),
            SelectionScope::Explicit,
        ))
    } else {
        let defaults = match &config.selections.default {
            Some(name) => config
                .selections
                .by_name
                .get(name)
                .cloned()
                .ok_or_else(|| {
                    gat_engine::RepositoryError::InvalidEffectiveSelections(
                        gat_core::config::ConfigError::UnknownSelection { name: name.clone() },
                    )
                })?,
            None => Default::default(),
        };
        Ok(ResolvedSelection::new(
            Cow::Owned(defaults.into()),
            SelectionScope::Configured,
        ))
    }
}

/// Read-only commands need configuration only when selectors were omitted.
pub(crate) fn resolve_for_read<'a>(
    selection: Option<&'a Selection>,
    repo: &gat_engine::Repository,
) -> Result<ResolvedSelection<'a>, gat_engine::RepositoryError> {
    match selection {
        Some(selection) => Ok(ResolvedSelection::new(
            Cow::Borrowed(selection),
            SelectionScope::Explicit,
        )),
        None => resolve(None, &repo.load_config()?),
    }
}

/// Coverage of a command result. A scoped result makes no claim about paths
/// outside its predicate, including whether their objects are published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionScope {
    Unrestricted,
    Explicit,
    Configured,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::{config::SelectionConfig, globs::GatGlobPattern};

    #[test]
    fn configured_path_and_patterns_match_cli_and_explicit_selectors_replace_all() {
        let config = Config {
            selections: gat_core::config::SelectionsConfig {
                default: Some("runtime".into()),
                by_name: std::collections::BTreeMap::from([(
                    "runtime".into(),
                    SelectionConfig {
                        path: gat_core::lexical_path::GatSubpath::normalize("models").unwrap(),
                        include: Some(vec![GatGlobPattern::parse("**").unwrap()]),
                        exclude: Some(vec![GatGlobPattern::parse("experimental/**").unwrap()]),
                    },
                )]),
            },
            ..Default::default()
        };
        let paths = [
            "models/a.onnx",
            "models/experimental/b.onnx",
            "archive/c.onnx",
            "models/d.bin",
        ];
        let configured = resolve(None, &config).unwrap();
        assert_eq!(
            paths.map(|path| configured.selection.matches_str(path)),
            [true, false, false, true]
        );
        assert_eq!(configured.scope, SelectionScope::Configured);
        assert_eq!(
            resolve(None, &Config::default()).unwrap().scope,
            SelectionScope::Unrestricted
        );
        for (path, include, exclude, expected) in [
            (".", vec![], vec![], [true, true, true, true]),
            ("models", vec![], vec![], [true, true, false, true]),
            ("archive", vec![], vec![], [false, false, true, false]),
            (".", vec!["**/*.onnx"], vec![], [true, true, true, false]),
            (
                "models",
                vec![],
                vec!["experimental/**"],
                [true, false, false, true],
            ),
        ] {
            let cli = Selection::from_scope_patterns(
                gat_core::path_scope::normalize_path_scope(std::path::Path::new(path)).unwrap(),
                include
                    .into_iter()
                    .map(|p| GatGlobPattern::parse(p).unwrap())
                    .collect(),
                exclude
                    .into_iter()
                    .map(|p| GatGlobPattern::parse(p).unwrap())
                    .collect(),
            );
            let resolved = resolve(Some(&cli), &config).unwrap();
            assert_eq!(paths.map(|p| resolved.selection.matches_str(p)), expected);
            assert_eq!(
                resolved.scope,
                if cli.is_unrestricted() {
                    SelectionScope::Unrestricted
                } else {
                    SelectionScope::Explicit
                }
            );
        }
    }
}
