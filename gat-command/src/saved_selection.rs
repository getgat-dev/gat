//! Configuration-only management and lookup of reusable path selections.
use crate::resource::{
    ResourceKind, ResourceScopeError, check_add_scope, check_scope, defining_scope, revealed_scope,
};
use gat_core::{
    config::{ConfigScope, SelectionConfig},
    globs::GatGlobPattern,
    lexical_path::GatSubpath,
    name::SelectionName,
    selection::Selection,
};
use gat_engine::{Repository, RepositoryError};

#[derive(Clone, Debug)]
pub enum SelectionRequest {
    List,
    Show {
        name: SelectionName,
    },
    Add {
        name: SelectionName,
        definition: SelectionConfig,
        scope: ConfigScope,
    },
    Update {
        name: SelectionName,
        path: Option<GatSubpath>,
        include: Option<Vec<GatGlobPattern>>,
        exclude: Option<Vec<GatGlobPattern>>,
        scope: ConfigScope,
    },
    Remove {
        name: SelectionName,
        scope: ConfigScope,
    },
    Default {
        name: Option<SelectionName>,
        unset: bool,
        scope: ConfigScope,
    },
}

#[derive(Clone, Debug)]
pub struct SelectionRecord {
    pub name: SelectionName,
    pub definition: SelectionConfig,
    pub scope: ConfigScope,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub enum SelectionOutcome {
    List(Vec<SelectionRecord>),
    Show(SelectionRecord),
    Saved {
        name: SelectionName,
    },
    Removed {
        name: SelectionName,
        revealed: Option<ConfigScope>,
    },
    Default {
        record: Option<SelectionRecord>,
        chosen_in: Option<ConfigScope>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SavedSelectionError {
    #[error("selection name is reserved")]
    ReservedName,
    #[error("selection `{name}` already exists")]
    AlreadyExists { name: SelectionName },
    #[error("no selection named `{name}`")]
    NotFound { name: SelectionName },
    #[error(transparent)]
    Scope(#[from] ResourceScopeError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

pub fn named_selection(
    repo: &Repository,
    name: &SelectionName,
) -> Result<Selection, SavedSelectionError> {
    let cfg = repo.load_config()?;
    let definition = cfg
        .selections
        .by_name
        .get(name)
        .ok_or_else(|| SavedSelectionError::NotFound { name: name.clone() })?;
    Ok(Selection::from_scope_patterns(
        definition.path.clone().into_path_scope(),
        definition.include.clone().unwrap_or_default(),
        definition.exclude.clone().unwrap_or_default(),
    ))
}

#[allow(
    clippy::missing_panics_doc,
    reason = "Every effective definition comes from a loaded configuration layer"
)]
pub fn saved_selection(
    repo: &Repository,
    request: SelectionRequest,
) -> Result<SelectionOutcome, SavedSelectionError> {
    let layers = repo.load_config_layers()?;
    // Mutations can repair a dangling default; validate their resulting configuration.
    let record = |name: SelectionName,
                  cfg: &gat_core::config::Config|
     -> Result<SelectionRecord, SavedSelectionError> {
        let definition = cfg
            .selections
            .by_name
            .get(&name)
            .cloned()
            .ok_or_else(|| SavedSelectionError::NotFound { name: name.clone() })?;
        let scope = defining_scope(&layers, |c| c.selections.by_name.contains_key(&name))
            .expect("effective definition comes from a layer");
        let is_default = cfg.selections.default.as_ref() == Some(&name);
        Ok(SelectionRecord {
            name,
            definition,
            scope,
            is_default,
        })
    };
    match request {
        SelectionRequest::List => {
            let cfg = layers.effective()?;
            Ok(SelectionOutcome::List(
                cfg.selections
                    .by_name
                    .keys()
                    .map(|name| record(name.clone(), &cfg))
                    .collect::<Result<_, _>>()?,
            ))
        }
        SelectionRequest::Show { name } => {
            Ok(SelectionOutcome::Show(record(name, &layers.effective()?)?))
        }
        SelectionRequest::Default { name, unset, scope } => {
            if name.is_some() || unset {
                let mut cfg = layers.scoped(scope).clone();
                cfg.selections.default = name;
                layers.candidate_effective(scope, &cfg)?;
                repo.save_config_scoped(&cfg, scope)?;
                return saved_selection(
                    repo,
                    SelectionRequest::Default {
                        name: None,
                        unset: false,
                        scope,
                    },
                );
            }
            let cfg = layers.effective()?;
            let chosen_in = defining_scope(&layers, |c| c.selections.default.is_some());
            Ok(SelectionOutcome::Default {
                record: cfg
                    .selections
                    .default
                    .clone()
                    .map(|name| record(name, &cfg))
                    .transpose()?,
                chosen_in,
            })
        }
        SelectionRequest::Add {
            name,
            definition,
            scope,
        } => {
            if name.as_str() == "default" {
                return Err(SavedSelectionError::ReservedName);
            }
            let mut cfg = layers.scoped(scope).clone();
            if cfg.selections.by_name.contains_key(&name) {
                return Err(SavedSelectionError::AlreadyExists { name });
            }
            check_add_scope(
                &layers,
                ResourceKind::Selection,
                name.as_str(),
                scope,
                |c| c.selections.by_name.contains_key(&name),
            )?;
            cfg.selections.by_name.insert(name.clone(), definition);
            layers.candidate_effective(scope, &cfg)?;
            repo.save_config_scoped(&cfg, scope)?;
            Ok(SelectionOutcome::Saved { name })
        }
        SelectionRequest::Update {
            name,
            path,
            include,
            exclude,
            scope,
        } => {
            check_scope(
                &layers,
                ResourceKind::Selection,
                name.as_str(),
                scope,
                |c| c.selections.by_name.contains_key(&name),
            )?;
            let mut cfg = layers.scoped(scope).clone();
            let definition = cfg
                .selections
                .by_name
                .get_mut(&name)
                .ok_or_else(|| SavedSelectionError::NotFound { name: name.clone() })?;
            if let Some(path) = path {
                definition.path = path;
            }
            if let Some(include) = include {
                definition.include = Some(include);
            }
            if let Some(exclude) = exclude {
                definition.exclude = Some(exclude);
            }
            layers.candidate_effective(scope, &cfg)?;
            repo.save_config_scoped(&cfg, scope)?;
            Ok(SelectionOutcome::Saved { name })
        }
        SelectionRequest::Remove { name, scope } => {
            check_scope(
                &layers,
                ResourceKind::Selection,
                name.as_str(),
                scope,
                |c| c.selections.by_name.contains_key(&name),
            )?;
            let mut cfg = layers.scoped(scope).clone();
            if cfg.selections.by_name.remove(&name).is_none() {
                return Err(SavedSelectionError::NotFound { name });
            }
            layers.candidate_effective(scope, &cfg)?;
            let revealed =
                revealed_scope(&layers, scope, |c| c.selections.by_name.contains_key(&name));
            repo.save_config_scoped(&cfg, scope)?;
            Ok(SelectionOutcome::Removed { name, revealed })
        }
    }
}
