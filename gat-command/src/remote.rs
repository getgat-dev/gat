//! `gat remote` orchestration over repository-scoped configuration services.

use gat_core::config::{ConfigScope, RemoteConfig};
use gat_core::endpoint::RemoteUrlTemplate;
use gat_core::name::RemoteName;
use gat_engine::{RemoteUrlValidationError, Repository, RepositoryError, validate_remote_url};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteRequest {
    Default {
        name: Option<RemoteName>,
        unset: bool,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteOutcome {
    Default {
        name: Option<RemoteName>,
        chosen_in: Option<ConfigScope>,
        defined_in: Option<ConfigScope>,
    },
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
    Scope(#[from] crate::resource::ResourceScopeError),
    #[error("remote name `{name}` is reserved; choose a different name")]
    ReservedName { name: RemoteName },

    #[error("no remote named `{name}`")]
    UnknownRemote { name: RemoteName },

    #[error(
        "remote `{name}` already exists; use `gat remote update {name} --url <url>` to change its URL"
    )]
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

#[allow(
    clippy::missing_panics_doc,
    reason = "Every effective definition comes from a loaded configuration layer"
)]
pub fn remote(repo: &Repository, request: RemoteRequest) -> Result<RemoteOutcome> {
    match request {
        RemoteRequest::Default { name, unset, scope } => {
            let layers = repo.load_config_layers()?;
            if name.is_some() || unset {
                let mut cfg = layers.scoped(scope).clone();
                cfg.remotes.default = name;
                let effective = layers.candidate_effective(scope, &cfg)?;
                if let Some(name) = &effective.remotes.default
                    && !effective.remotes.by_name.contains_key(name)
                {
                    return Err(RemoteError::UnknownRemote { name: name.clone() });
                }
                repo.save_config_scoped(&cfg, scope)?;
                return remote(
                    repo,
                    RemoteRequest::Default {
                        name: None,
                        unset: false,
                        scope,
                    },
                );
            }
            let cfg = layers.effective()?;
            let chosen_in =
                crate::resource::defining_scope(&layers, |c| c.remotes.default.is_some());
            let defined_in = cfg.remotes.default.as_ref().and_then(|name| {
                crate::resource::defining_scope(&layers, |c| c.remotes.by_name.contains_key(name))
            });
            Ok(RemoteOutcome::Default {
                name: cfg.remotes.default,
                chosen_in,
                defined_in,
            })
        }
        RemoteRequest::List => {
            let cfg = repo.load_config()?;
            let default = cfg.remotes.default;
            let records = cfg
                .remotes
                .by_name
                .into_iter()
                .map(|(name, remote)| {
                    let is_default = default.as_ref() == Some(&name);
                    RemoteRecord {
                        name,
                        url: remote.url,
                        is_default,
                    }
                })
                .collect();
            Ok(RemoteOutcome::List(records))
        }
        RemoteRequest::Add { name, url, scope } => {
            let layers = repo.load_config_layers()?;
            crate::resource::check_add_scope(
                &layers,
                crate::resource::ResourceKind::Remote,
                name.as_str(),
                scope,
                |c| c.remotes.by_name.contains_key(&name),
            )?;
            if name.as_str() == "default" {
                return Err(RemoteError::ReservedName { name });
            }
            validate_remote_url(&url)?;
            let mut cfg = layers.scoped(scope).clone();
            if cfg.remotes.by_name.contains_key(&name) {
                return Err(RemoteError::DuplicateRemote { name });
            }
            cfg.remotes
                .by_name
                .insert(name.clone(), RemoteConfig { url: url.clone() });
            repo.save_config_scoped(&cfg, scope)?;
            Ok(RemoteOutcome::Added { name, url })
        }
        RemoteRequest::Remove { name, scope } => {
            let layers = repo.load_config_layers()?;
            crate::resource::check_scope(
                &layers,
                crate::resource::ResourceKind::Remote,
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
            repo.save_config_scoped(&cfg, scope)?;
            Ok(RemoteOutcome::Removed {
                revealed: crate::resource::revealed_scope(&layers, scope, |c| {
                    c.remotes.by_name.contains_key(&name)
                }),
                name,
            })
        }
        RemoteRequest::Show { name } => {
            let layers = repo.load_config_layers()?;
            let cfg = layers.effective()?;
            let remotes = cfg.remotes;
            let remote = remotes
                .by_name
                .get(&name)
                .cloned()
                .ok_or_else(|| RemoteError::UnknownRemote { name: name.clone() })?;
            let scope =
                crate::resource::defining_scope(&layers, |c| c.remotes.by_name.contains_key(&name))
                    .expect("effective definition comes from a layer");
            let is_default = remotes.default.as_ref() == Some(&name);
            Ok(RemoteOutcome::Show {
                record: RemoteRecord {
                    name,
                    url: remote.url,
                    is_default,
                },
                scope,
            })
        }
        RemoteRequest::Update { name, url, scope } => {
            let layers = repo.load_config_layers()?;
            crate::resource::check_scope(
                &layers,
                crate::resource::ResourceKind::Remote,
                name.as_str(),
                scope,
                |c| c.remotes.by_name.contains_key(&name),
            )?;
            if let Some(url) = &url {
                validate_remote_url(url)?;
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
            repo.save_config_scoped(&cfg, scope)?;
            Ok(RemoteOutcome::Updated { name, url })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::at(temp.path().to_path_buf());
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
