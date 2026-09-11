//! Configuration-only management and lookup of reusable path selections.
use super::DefaultAction;
use super::resource::{
    ResourceKind, ResourceScopeError, check_add_scope, check_scope, revealed_scope,
};
use crate::{Repository, RepositoryError};
use gat_core::{
    config::{ConfigScope, SelectionConfig},
    globs::GatGlobPattern,
    lexical_path::GatSubpath,
    name::SelectionName,
    selection::Selection,
};

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
    /// Omitted fields preserve the saved value; an explicitly empty pattern
    /// list clears that filter. This distinction applies only to the edit,
    /// not to the complete persisted definition.
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
        action: DefaultAction<SelectionName>,
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
pub struct SelectionDefault {
    pub record: SelectionRecord,
    pub chosen_in: ConfigScope,
}

#[derive(Clone, Debug)]
pub enum SelectionOutcome {
    List(Vec<SelectionRecord>),
    Show(SelectionRecord),
    Saved {
        name: SelectionName,
        /// Extent of the saved definition, not the number of current matches.
        unrestricted: bool,
    },
    Removed {
        name: SelectionName,
        revealed: Option<ConfigScope>,
    },
    Default(Option<SelectionDefault>),
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
    let layers = repo.load_config_layers()?;
    let (definition, _) = super::resource::definition(&layers, |c| c.selections.by_name.get(name))
        .ok_or_else(|| SavedSelectionError::NotFound { name: name.clone() })?;
    Ok(definition.clone().into())
}

pub fn saved_selection(
    repo: &Repository,
    request: SelectionRequest,
) -> Result<SelectionOutcome, SavedSelectionError> {
    match request {
        SelectionRequest::List => {
            let layers = repo.load_config_layers()?;
            let default = super::resource::definition(&layers, |c| c.selections.default.as_ref());
            Ok(SelectionOutcome::List(
                super::resource::definitions(&layers, |c| &c.selections.by_name)
                    .into_iter()
                    .map(|(name, (definition, scope))| SelectionRecord {
                        name: name.clone(),
                        definition: definition.clone(),
                        scope,
                        is_default: default.is_some_and(|(default, _)| default == name),
                    })
                    .collect(),
            ))
        }

        SelectionRequest::Show { name } => {
            let layers = repo.load_config_layers()?;
            let is_default =
                super::resource::definition(&layers, |c| c.selections.default.as_ref())
                    .is_some_and(|(default, _)| default == &name);
            Ok(SelectionOutcome::Show(selection_record(
                &layers, name, is_default,
            )?))
        }
        SelectionRequest::Default { action, scope } => {
            if !matches!(action, DefaultAction::Get) {
                let edit = repo.begin_configuration_edit()?;
                let layers = repo.load_config_layers()?;
                let mut cfg = layers.scoped(scope).clone();
                cfg.selections.default = action.into_name();
                layers.candidate_effective(scope, &cfg)?;
                let result = default_outcome(
                    &layers,
                    super::resource::candidate_definition(&layers, scope, &cfg, |c| {
                        c.selections.default.as_ref()
                    }),
                )?;
                edit.commit(&layers, &cfg, scope)?;
                return Ok(result);
            }
            let layers = repo.load_config_layers()?;
            default_outcome(
                &layers,
                super::resource::definition(&layers, |c| c.selections.default.as_ref()),
            )
        }
        SelectionRequest::Add {
            name,
            definition,
            scope,
        } => {
            let edit = repo.begin_configuration_edit()?;
            let layers = repo.load_config_layers()?;
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
            let unrestricted = definition.is_unrestricted();
            cfg.selections.by_name.insert(name.clone(), definition);
            edit.commit(&layers, &cfg, scope)?;
            Ok(SelectionOutcome::Saved { name, unrestricted })
        }
        SelectionRequest::Update {
            name,
            path,
            include,
            exclude,
            scope,
        } => {
            let edit = repo.begin_configuration_edit()?;
            let layers = repo.load_config_layers()?;
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
                definition.include = include;
            }
            if let Some(exclude) = exclude {
                definition.exclude = exclude;
            }
            let unrestricted = definition.is_unrestricted();
            edit.commit(&layers, &cfg, scope)?;
            Ok(SelectionOutcome::Saved { name, unrestricted })
        }
        SelectionRequest::Remove { name, scope } => {
            let edit = repo.begin_configuration_edit()?;
            let layers = repo.load_config_layers()?;
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
            let revealed =
                revealed_scope(&layers, scope, |c| c.selections.by_name.contains_key(&name));
            edit.commit(&layers, &cfg, scope)?;
            Ok(SelectionOutcome::Removed { name, revealed })
        }
    }
}

fn selection_record(
    layers: &crate::ConfigLayers,
    name: SelectionName,
    is_default: bool,
) -> Result<SelectionRecord, SavedSelectionError> {
    let (definition, scope) =
        super::resource::definition(layers, |c| c.selections.by_name.get(&name))
            .ok_or_else(|| SavedSelectionError::NotFound { name: name.clone() })?;
    Ok(SelectionRecord {
        name,
        definition: definition.clone(),
        scope,
        is_default,
    })
}

fn default_outcome(
    layers: &crate::ConfigLayers,
    choice: Option<(&SelectionName, ConfigScope)>,
) -> Result<SelectionOutcome, SavedSelectionError> {
    Ok(SelectionOutcome::Default(
        choice
            .map(|(name, chosen_in)| {
                Ok::<_, SavedSelectionError>(SelectionDefault {
                    record: selection_record(layers, name.clone(), true)?,
                    chosen_in,
                })
            })
            .transpose()?,
    ))
}
