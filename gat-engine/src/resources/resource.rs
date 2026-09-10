//! Shared scope rules for named configuration resources.
use crate::ConfigLayers;
use gat_core::config::{Config, ConfigScope};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Remote,
    Route,
    Selection,
}

#[derive(Debug, thiserror::Error)]
#[error("resource `{name}` cannot be edited in {requested}; it is defined in {actual}")]
pub struct ResourceScopeError {
    pub kind: ResourceKind,
    pub name: String,
    pub requested: ConfigScope,
    pub actual: ConfigScope,
}

/// Resolve a whole definition and its provenance in one borrowed lookup.
pub(crate) fn definition<'a, T>(
    layers: &'a ConfigLayers,
    get: impl Fn(&'a Config) -> Option<&'a T>,
) -> Option<(&'a T, ConfigScope)> {
    [
        ConfigScope::Local,
        ConfigScope::Project,
        ConfigScope::Global,
    ]
    .into_iter()
    .find_map(|scope| get(layers.scoped(scope)).map(|value| (value, scope)))
}

/// Index only the requested resource family; definitions remain borrowed.
pub(crate) fn definitions<'a, K: Ord, T>(
    layers: &'a ConfigLayers,
    get: impl Fn(&'a Config) -> &'a std::collections::BTreeMap<K, T>,
) -> std::collections::BTreeMap<&'a K, (&'a T, ConfigScope)> {
    let mut result = std::collections::BTreeMap::new();
    for scope in [
        ConfigScope::Global,
        ConfigScope::Project,
        ConfigScope::Local,
    ] {
        result.extend(
            get(layers.scoped(scope))
                .iter()
                .map(|(name, value)| (name, (value, scope))),
        );
    }
    result
}

pub(crate) fn defining_scope(
    layers: &ConfigLayers,
    contains: impl Fn(&Config) -> bool,
) -> Option<ConfigScope> {
    find_scope(|scope| contains(layers.scoped(scope)))
}

/// Provenance in a candidate snapshot, without loading configuration again.
pub(crate) fn candidate_definition<'a, T>(
    layers: &'a ConfigLayers,
    changed: ConfigScope,
    candidate: &'a Config,
    get: impl Fn(&'a Config) -> Option<&'a T>,
) -> Option<(&'a T, ConfigScope)> {
    [
        ConfigScope::Local,
        ConfigScope::Project,
        ConfigScope::Global,
    ]
    .into_iter()
    .find_map(|scope| {
        get(if scope == changed {
            candidate
        } else {
            layers.scoped(scope)
        })
        .map(|value| (value, scope))
    })
}

fn find_scope(contains: impl Fn(ConfigScope) -> bool) -> Option<ConfigScope> {
    [
        ConfigScope::Local,
        ConfigScope::Project,
        ConfigScope::Global,
    ]
    .into_iter()
    .find(|scope| contains(*scope))
}

pub(crate) fn check_scope(
    layers: &ConfigLayers,
    kind: ResourceKind,
    name: &str,
    requested: ConfigScope,
    contains: impl Fn(&Config) -> bool,
) -> Result<(), ResourceScopeError> {
    if let Some(actual) = defining_scope(layers, contains)
        && actual != requested
    {
        return Err(ResourceScopeError {
            kind,
            name: name.into(),
            requested,
            actual,
        });
    }
    Ok(())
}

pub(crate) fn check_add_scope(
    layers: &ConfigLayers,
    kind: ResourceKind,
    name: &str,
    requested: ConfigScope,
    contains: impl Fn(&Config) -> bool,
) -> Result<(), ResourceScopeError> {
    if let Some(actual) = defining_scope(layers, contains)
        && actual.precedence() > requested.precedence()
    {
        return Err(ResourceScopeError {
            kind,
            name: name.into(),
            requested,
            actual,
        });
    }
    Ok(())
}

pub(crate) fn revealed_scope(
    layers: &ConfigLayers,
    removed: ConfigScope,
    contains: impl Fn(&Config) -> bool,
) -> Option<ConfigScope> {
    find_scope(|scope| scope != removed && contains(layers.scoped(scope)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_lookup_borrows_whole_winning_definitions_with_their_scope() {
        let config = |names: &[&str]| {
            let mut config = Config::default();
            for name in names {
                config
                    .selections
                    .by_name
                    .insert((*name).into(), Default::default());
            }
            config
        };
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(dir.path().to_path_buf());
        repo.save_config_scoped(&config(&["shared", "project"]), ConfigScope::Project)
            .unwrap();
        repo.save_config_scoped(&config(&["shared", "local"]), ConfigScope::Local)
            .unwrap();
        let layers = repo.load_config_layers().unwrap();
        let all = definitions(&layers, |c| &c.selections.by_name);
        assert_eq!(
            all.keys().map(|name| name.as_str()).collect::<Vec<_>>(),
            ["local", "project", "shared"],
        );
        for (name, expected_scope) in [
            ("project", ConfigScope::Project),
            ("local", ConfigScope::Local),
            ("shared", ConfigScope::Local),
        ] {
            let (value, scope) = definition(&layers, |c| c.selections.by_name.get(name)).unwrap();
            assert_eq!(scope, expected_scope);
            assert!(std::ptr::eq(
                value,
                &raw const layers.scoped(scope).selections.by_name[name]
            ));
            let (listed, listed_scope) =
                all.iter().find(|(key, _)| key.as_str() == name).unwrap().1;
            assert_eq!(*listed_scope, scope);
            assert!(std::ptr::eq(*listed, value));
        }
        assert!(definition(&layers, |c| c.selections.by_name.get("missing")).is_none());
    }
}
