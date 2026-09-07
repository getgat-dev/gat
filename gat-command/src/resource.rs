//! Shared scope rules for named configuration resources.
use gat_core::config::{Config, ConfigScope};
use gat_engine::ConfigLayers;

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

pub(crate) fn defining_scope(
    layers: &ConfigLayers,
    contains: impl Fn(&Config) -> bool,
) -> Option<ConfigScope> {
    [
        ConfigScope::Local,
        ConfigScope::Project,
        ConfigScope::Global,
    ]
    .into_iter()
    .find(|scope| contains(layers.scoped(*scope)))
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
    [
        ConfigScope::Local,
        ConfigScope::Project,
        ConfigScope::Global,
    ]
    .into_iter()
    .find(|scope| *scope != removed && contains(layers.scoped(*scope)))
}
