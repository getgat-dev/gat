//! `gat remote` orchestration over repository-scoped configuration services.
use super::DefaultAction;

use crate::{RemoteUrlValidationError, Repository, RepositoryError};
use gat_core::config::{ConfigScope, RemoteConfig};
use gat_core::endpoint::RemoteUrlTemplate;
use gat_core::name::RemoteName;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteRequest {
    Default {
        action: DefaultAction<RemoteName>,
        scope: ConfigScope,
    },
    List,
    Add {
        name: RemoteName,
        url: RemoteUrlTemplate,
        scope: ConfigScope,
    },
    Remove {
        name: RemoteName,
        scope: ConfigScope,
    },
    Show {
        name: RemoteName,
    },
    Update {
        name: RemoteName,
        url: Option<RemoteUrlTemplate>,
        scope: ConfigScope,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRecord {
    pub name: RemoteName,
    pub url: RemoteUrlTemplate,
    pub is_default: bool,
}

/// A selected remote and the scope that chose it.
///
/// ```compile_fail
/// use gat_engine::RemoteDefault;
/// let invalid = RemoteDefault { name: "origin".into(), chosen_in: None, defined_in: None };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDefault {
    pub name: RemoteName,
    pub chosen_in: ConfigScope,
    /// A hand-edited default may name a definition that is currently absent.
    pub defined_in: Option<ConfigScope>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteOutcome {
    Default(Option<RemoteDefault>),
    Show {
        record: RemoteRecord,
        scope: ConfigScope,
    },
    List(Vec<RemoteRecord>),
    Added {
        name: RemoteName,
        url: RemoteUrlTemplate,
    },
    Removed {
        name: RemoteName,
        revealed: Option<ConfigScope>,
    },
    Updated {
        name: RemoteName,
        url: RemoteUrlTemplate,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("removing remote `{name}` would leave the default dangling")]
    DefaultWouldDangle { name: RemoteName },
    #[error(transparent)]
    Scope(#[from] super::resource::ResourceScopeError),
    #[error("remote name is reserved")]
    ReservedName { name: RemoteName },

    #[error("no remote named `{name}`")]
    UnknownRemote { name: RemoteName },

    #[error("remote already exists")]
    DuplicateRemote { name: RemoteName },

    #[error(transparent)]
    ValidateUrl(#[from] RemoteUrlValidationError),

    #[error(transparent)]
    Repository(Box<RepositoryError>),
}

impl From<RepositoryError> for RemoteError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(Box::new(error))
    }
}

type Result<T> = std::result::Result<T, RemoteError>;

pub fn remote(repo: &Repository, request: RemoteRequest) -> Result<RemoteOutcome> {
    match request {
        RemoteRequest::Default { action, scope } => {
            if matches!(action, DefaultAction::Get) {
                let layers = repo.load_config_layers()?;
                return Ok(default_outcome(
                    &layers,
                    super::resource::definition(&layers, |c| c.remotes.default.as_ref()),
                ));
            }
            let edit = repo.begin_configuration_edit()?;
            let layers = repo.load_config_layers()?;
            let mut cfg = layers.scoped(scope).clone();
            cfg.remotes.default = action.into_name();
            let effective = layers.candidate_effective(scope, &cfg)?;
            if let Some(name) = &effective.remotes.default
                && !effective.remotes.by_name.contains_key(name)
            {
                return Err(RemoteError::UnknownRemote { name: name.clone() });
            }
            let outcome = default_outcome(
                &layers,
                super::resource::candidate_definition(&layers, scope, &cfg, |c| {
                    c.remotes.default.as_ref()
                }),
            );
            edit.commit(&layers, &cfg, scope)?;
            Ok(outcome)
        }

        RemoteRequest::List => {
            let layers = repo.load_config_layers()?;
            let default = super::resource::definition(&layers, |c| c.remotes.default.as_ref());
            let records = super::resource::definitions(&layers, |c| &c.remotes.by_name)
                .into_iter()
                .map(|(name, (remote, _))| RemoteRecord {
                    name: name.clone(),
                    url: remote.url.clone(),
                    is_default: default.is_some_and(|(default, _)| default == name),
                })
                .collect();
            Ok(RemoteOutcome::List(records))
        }
        RemoteRequest::Add { name, url, scope } => {
            let edit = repo.begin_configuration_edit()?;
            let layers = repo.load_config_layers()?;
            super::resource::check_add_scope(
                &layers,
                super::resource::ResourceKind::Remote,
                name.as_str(),
                scope,
                |c| c.remotes.by_name.contains_key(&name),
            )?;
            if name.as_str() == "default" {
                return Err(RemoteError::ReservedName { name });
            }
            repo.validate_remote_url(&url)?;
            let mut cfg = layers.scoped(scope).clone();
            if cfg.remotes.by_name.contains_key(&name) {
                return Err(RemoteError::DuplicateRemote { name });
            }
            cfg.remotes
                .by_name
                .insert(name.clone(), RemoteConfig { url: url.clone() });
            edit.commit(&layers, &cfg, scope)?;
            Ok(RemoteOutcome::Added { name, url })
        }
        RemoteRequest::Remove { name, scope } => {
            let edit = repo.begin_configuration_edit()?;
            let layers = repo.load_config_layers()?;
            super::resource::check_scope(
                &layers,
                super::resource::ResourceKind::Remote,
                name.as_str(),
                scope,
                |c| c.remotes.by_name.contains_key(&name),
            )?;
            let mut cfg = layers.scoped(scope).clone();
            if cfg.remotes.by_name.remove(&name).is_none() {
                return Err(RemoteError::UnknownRemote { name });
            }
            let candidate = layers.candidate_effective(scope, &cfg)?;
            if let Some(default) = candidate.remotes.default
                && !candidate.remotes.by_name.contains_key(&default)
            {
                return Err(RemoteError::DefaultWouldDangle { name: default });
            }
            edit.commit(&layers, &cfg, scope)?;
            Ok(RemoteOutcome::Removed {
                revealed: super::resource::revealed_scope(&layers, scope, |c| {
                    c.remotes.by_name.contains_key(&name)
                }),
                name,
            })
        }
        RemoteRequest::Show { name } => {
            let layers = repo.load_config_layers()?;
            let (remote, scope) =
                super::resource::definition(&layers, |c| c.remotes.by_name.get(&name))
                    .ok_or_else(|| RemoteError::UnknownRemote { name: name.clone() })?;
            let is_default = super::resource::definition(&layers, |c| c.remotes.default.as_ref())
                .is_some_and(|(default, _)| default == &name);
            Ok(RemoteOutcome::Show {
                record: RemoteRecord {
                    name,
                    url: remote.url.clone(),
                    is_default,
                },
                scope,
            })
        }
        RemoteRequest::Update { name, url, scope } => {
            let edit = repo.begin_configuration_edit()?;
            let layers = repo.load_config_layers()?;
            super::resource::check_scope(
                &layers,
                super::resource::ResourceKind::Remote,
                name.as_str(),
                scope,
                |c| c.remotes.by_name.contains_key(&name),
            )?;
            if let Some(url) = &url {
                repo.validate_remote_url(url)?;
            }
            let mut cfg = layers.scoped(scope).clone();
            let remote = cfg
                .remotes
                .by_name
                .get_mut(&name)
                .ok_or_else(|| RemoteError::UnknownRemote { name: name.clone() })?;
            if let Some(url) = url {
                remote.url = url;
            }
            let url = remote.url.clone();
            edit.commit(&layers, &cfg, scope)?;
            Ok(RemoteOutcome::Updated { name, url })
        }
    }
}

fn default_outcome(
    layers: &crate::ConfigLayers,
    choice: Option<(&RemoteName, ConfigScope)>,
) -> RemoteOutcome {
    RemoteOutcome::Default(choice.map(|(name, chosen_in)| RemoteDefault {
        name: name.clone(),
        chosen_in,
        defined_in: super::resource::defining_scope(layers, |c| {
            c.remotes.by_name.contains_key(name)
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(temp.path().to_path_buf());
        (temp, repo)
    }

    fn url(path: &std::path::Path) -> RemoteUrlTemplate {
        RemoteUrlTemplate::from_string(test_support_git::file_remote_url(path))
    }

    fn add(repo: &Repository, name: &str, url: RemoteUrlTemplate) -> Result<RemoteOutcome> {
        remote(
            repo,
            RemoteRequest::Add {
                name: RemoteName::from_string(name.to_string()),
                url,
                scope: ConfigScope::Project,
            },
        )
    }

    #[test]
    fn default_read_distinguishes_no_choice_from_an_undefined_chosen_remote() {
        let (_temp, repo) = repository();
        let request = || RemoteRequest::Default {
            action: DefaultAction::Get,
            scope: ConfigScope::Project,
        };
        assert!(matches!(
            remote(&repo, request()).unwrap(),
            RemoteOutcome::Default(None)
        ));
        let mut config = gat_core::config::Config::default();
        config.remotes.default = Some("missing".into());
        repo.save_config_scoped(&config, ConfigScope::Project)
            .unwrap();
        let RemoteOutcome::Default(Some(default)) = remote(&repo, request()).unwrap() else {
            panic!("expected a chosen remote");
        };
        assert_eq!(default.name.as_str(), "missing");
        assert_eq!(default.chosen_in, ConfigScope::Project);
        assert_eq!(default.defined_in, None);
    }

    #[test]
    fn adding_remotes_never_chooses_a_default_and_list_order_is_deterministic() {
        let (temp, repo) = repository();
        add(&repo, "zulu", url(temp.path())).unwrap();
        add(&repo, "alpha", url(temp.path())).unwrap();

        let RemoteOutcome::List(records) = remote(&repo, RemoteRequest::List).unwrap() else {
            panic!("expected remote list");
        };
        assert_eq!(
            records
                .iter()
                .map(|record| record.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zulu"]
        );
        assert!(!records[0].is_default);
        assert!(!records[1].is_default);
    }

    #[test]
    fn duplicate_add_does_not_replace_the_existing_url() {
        let (temp, repo) = repository();
        let first = url(temp.path());
        add(&repo, "origin", first.clone()).unwrap();

        let error = add(&repo, "origin", url(temp.path())).unwrap_err();
        assert!(matches!(error, RemoteError::DuplicateRemote { .. }));
        assert_eq!(
            repo.load_config().unwrap().remotes.by_name.get("origin"),
            Some(&first.into())
        );
    }

    #[test]
    fn read_only_lookup_uses_effective_config_but_mutation_uses_its_scope() {
        let (temp, repo) = repository();
        let configured = url(temp.path());
        add(&repo, "origin", configured.clone()).unwrap();

        let RemoteOutcome::Show { record, scope } = remote(
            &repo,
            RemoteRequest::Show {
                name: RemoteName::from_string("origin".to_string()),
            },
        )
        .unwrap() else {
            panic!("expected configured URL");
        };
        assert_eq!(record.url, configured);
        assert_eq!(scope, ConfigScope::Project);

        let error = remote(
            &repo,
            RemoteRequest::Remove {
                name: RemoteName::from_string("origin".to_string()),
                scope: ConfigScope::Local,
            },
        )
        .unwrap_err();
        assert!(matches!(error, RemoteError::Scope(_)));
        assert!(
            repo.load_config()
                .unwrap()
                .remotes
                .by_name
                .contains_key("origin")
        );
    }

    #[test]
    fn reserved_names_are_rejected_without_mutation() {
        let (temp, repo) = repository();
        let reserved = add(&repo, "default", url(temp.path())).unwrap_err();
        assert!(matches!(reserved, RemoteError::ReservedName { .. }));
    }

    #[test]
    fn update_updates_the_persisted_value_and_returns_the_typed_template() {
        let (temp, repo) = repository();
        add(&repo, "origin", url(temp.path())).unwrap();
        let replacement = url(&temp.path().join("replacement"));

        let RemoteOutcome::Updated { name, url } = remote(
            &repo,
            RemoteRequest::Update {
                name: RemoteName::from_string("origin".to_string()),
                url: Some(replacement.clone()),
                scope: ConfigScope::Project,
            },
        )
        .unwrap() else {
            panic!("expected updated outcome");
        };
        assert_eq!(name.as_str(), "origin");
        assert_eq!(url, replacement);
        assert_eq!(
            repo.load_config().unwrap().remotes.by_name.get("origin"),
            Some(&replacement.into())
        );
    }

    #[test]
    fn update_validates_before_checking_the_remote_name() {
        let (_temp, repo) = repository();
        let error = remote(
            &repo,
            RemoteRequest::Update {
                name: RemoteName::from_string("missing".to_string()),
                url: Some(RemoteUrlTemplate::from_string(
                    "unsupported://host/path".to_string(),
                )),
                scope: ConfigScope::Project,
            },
        )
        .unwrap_err();
        assert!(matches!(error, RemoteError::ValidateUrl(_)));
    }

    #[test]
    fn update_preserves_omitted_url_and_remove_deletes_definition() {
        let (temp, repo) = repository();
        let original = url(temp.path());
        add(&repo, "origin", original.clone()).unwrap();
        let name = RemoteName::from_string("origin".into());
        remote(
            &repo,
            RemoteRequest::Update {
                name: name.clone(),
                url: None,
                scope: ConfigScope::Project,
            },
        )
        .unwrap();
        assert_eq!(
            repo.load_config().unwrap().remotes.by_name[&name].url,
            original
        );
        remote(
            &repo,
            RemoteRequest::Remove {
                name: name.clone(),
                scope: ConfigScope::Project,
            },
        )
        .unwrap();
        assert!(
            !repo
                .load_config()
                .unwrap()
                .remotes
                .by_name
                .contains_key(&name)
        );
    }

    #[test]
    fn show_reports_highest_scope_and_update_stays_in_selected_scope() {
        let (temp, repo) = repository();
        add(&repo, "origin", url(temp.path())).unwrap();
        let name = RemoteName::from_string("origin".into());
        let local_url = url(&temp.path().join("local"));
        remote(
            &repo,
            RemoteRequest::Add {
                name: name.clone(),
                url: local_url.clone(),
                scope: ConfigScope::Local,
            },
        )
        .unwrap();
        remote(
            &repo,
            RemoteRequest::Update {
                name: name.clone(),
                url: Some(url(&temp.path().join("project"))),
                scope: ConfigScope::Project,
            },
        )
        .expect_err("a shadowed definition cannot be updated");
        let RemoteOutcome::Show { record, scope } =
            remote(&repo, RemoteRequest::Show { name }).unwrap()
        else {
            panic!("expected details");
        };
        assert_eq!(scope, ConfigScope::Local);
        assert_eq!(record.url, local_url);
        assert!(!record.is_default);
    }

    #[test]
    fn outcomes_keep_the_unexpanded_template_typed_and_debug_redacted() {
        let (_temp, repo) = repository();
        let mut cfg = repo.load_config_scoped(ConfigScope::Project).unwrap();
        let template =
            RemoteUrlTemplate::from_string("s3://bucket/${PREFIX}?token=${TOKEN}".to_string());
        cfg.remotes.by_name.insert(
            RemoteName::from_string("origin".to_string()),
            template.clone().into(),
        );
        cfg.remotes.default = Some(RemoteName::from_string("origin".to_string()));
        repo.save_config_scoped(&cfg, ConfigScope::Project).unwrap();

        let RemoteOutcome::Show { record, .. } = remote(
            &repo,
            RemoteRequest::Show {
                name: RemoteName::from_string("origin".into()),
            },
        )
        .unwrap() else {
            panic!("expected configured default");
        };
        assert_eq!(record.url, template);
        assert!(!format!("{record:?}").contains("TOKEN"));
    }
}
