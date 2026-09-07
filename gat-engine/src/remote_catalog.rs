//! Compiled, operation-scoped remote catalog.
//!
//! [`RemoteCatalog`] is compiled once per `super::snapshot::Snapshot`
//! from the effective `remotes` config: every configured remote is assigned
//! a compact, stable [`RemoteId`] for the lifetime of the operation. Route/default
//! resolution ([`crate::path_policy::EffectivePathPolicy`]) and
//! remote-operator lifecycle (`super::remote_session::RemoteSession`)
//! both key off this one catalog instead of each independently cloning
//! [`RemotesConfig`] or repeatedly allocating/cloning remote-name
//! `String`s for every selected row.

use gat_core::config::RemotesConfig;
use gat_core::endpoint::RemoteUrlTemplate;
use gat_core::name::RemoteName;
use std::collections::HashMap;
use std::sync::Arc;

/// Every way validating a configured remote URL can fail.
#[derive(Debug)]
pub struct RemoteUrlValidationError {
    source: Box<crate::remote_open::RemoteOpenError>,
}

impl RemoteUrlValidationError {
    /// Original endpoint template, retained without environment expansion.
    #[must_use]
    pub fn template(&self) -> &gat_core::endpoint::RemoteUrlTemplate {
        self.source.template()
    }

    #[must_use]
    pub fn kind(&self) -> &crate::remote_open::RemoteOpenFailureKind {
        self.source.kind()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn from_io(template: &RemoteUrlTemplate, source: gat_io::OpenRemoteError) -> Self {
        Self {
            source: Box::new(crate::remote_open::RemoteOpenError::from_io(
                template, source,
            )),
        }
    }
}

impl std::fmt::Display for RemoteUrlValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("configured remote URL is invalid")
    }
}

impl std::error::Error for RemoteUrlValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Validates an endpoint template before configuration persistence without
/// exposing the I/O implementation to command orchestration.
pub fn validate_remote_url(
    template: &RemoteUrlTemplate,
) -> std::result::Result<(), RemoteUrlValidationError> {
    gat_io::RemoteClient::validate(template.as_template_str()).map_err(|source| {
        RemoteUrlValidationError {
            source: Box::new(crate::remote_open::RemoteOpenError::from_io(
                template, source,
            )),
        }
    })?;
    Ok(())
}

/// This module's own `Result` alias.
type Result<T> = std::result::Result<T, RemoteCatalogError>;

/// Every way compiling or resolving against a [`RemoteCatalog`] can fail.
#[derive(Debug, thiserror::Error)]
pub enum RemoteCatalogError {
    /// `remotes.default` names a remote absent from `remotes.by_name`.
    #[error("remotes.default `{name}` is not a configured remote")]
    UnknownDefault { name: String },

    /// No remote was configured (neither an explicit `--remote` override
    /// nor a repository default) for an operation that required one.
    #[error("no remote is configured")]
    NoRemoteConfigured,

    /// An explicit `--remote` override named a remote absent from the
    /// configured catalog.
    #[error(transparent)]
    UnknownOverride(#[from] crate::path_policy::UnknownRemoteOverrideError),
}

/// A compact, `Copy`, operation-local identity for one configured remote --
/// stable for the lifetime of one [`RemoteCatalog`] (and so one
/// `super::snapshot::Snapshot`/operation), never
/// persisted or compared across operations. Cheap to store in obligation/
/// job structs instead of cloning a remote-name `String` per selected row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteId(usize);

/// One configured remote's interned name and raw configured URL,
/// structurally paired rather than correlated
/// only by matching index across two parallel vectors. `url` is the
/// secret-safe [`RemoteUrlTemplate`] domain type -- never a raw `Arc<str>`
/// -- so this struct's derived `Debug` can never print un-redacted
/// endpoint text (see [`RemoteUrlTemplate`]'s own module doc comment).
#[derive(Debug)]
struct RemoteSpec {
    name: Arc<str>,
    url: RemoteUrlTemplate,
}

/// Compiled once from the effective `remotes` config: every configured
/// remote name is interned exactly once (as an `Arc<str>`, cheaply
/// cloned rather than reallocated) and assigned a stable [`RemoteId`],
/// plus the catalog remembers which id (if any) is the repository
/// default. Also carries each remote's raw configured URL:
/// this is the one authoritative source of remote endpoint/config data
/// for the whole operation, so `super::remote_session::RemoteSession`
/// uses this catalog to build operators. Immutable after construction and
/// side-effect free (no operator/network I/O -- that stays in
/// `super::remote_session::RemoteSession`).
///
/// Stores endpoint data through [`RemoteUrlTemplate`], never a raw `Arc<str>`, so this struct's derived
/// `Debug` cannot leak a credential-bearing URL -- only the domain types'
/// redacted `Debug` rendering. Endpoint lookup stays inside the engine for
/// the remote session's private client builder.
///
/// ```compile_fail
/// use gat_engine::{RemoteCatalog, RemoteId};
/// fn endpoint(catalog: &RemoteCatalog, id: RemoteId) {
///     let _ = catalog.url(id);
/// }
/// ```
///
/// The interned name representation stays internal; callers obtain a typed
/// name through [`Self::remote_name`].
///
/// ```compile_fail
/// use gat_engine::{RemoteCatalog, RemoteId};
/// fn interned_name(catalog: &RemoteCatalog, id: RemoteId) {
///     let _ = catalog.name(id);
/// }
/// ```
#[derive(Debug)]
pub struct RemoteCatalog {
    specs: Vec<RemoteSpec>,
    by_name: HashMap<Arc<str>, RemoteId>,
    default: Option<RemoteId>,
}

impl RemoteCatalog {
    /// Compiles the catalog from `remotes`: every configured remote gets a
    /// stable id in configured (`BTreeMap`, so name-sorted) order, and the
    /// repository default (if any) is resolved to its id up front so later
    /// per-row lookups never need to compare against a `default` name
    /// string again. Rejects a `remotes.default` that names a remote not
    /// present in `remotes.by_name` instead of
    /// silently compiling it away to `default: None` -- a stale/typo'd
    /// default is a config error the operation should fail on up front,
    /// not a surprising "no default configured" behavior discovered later
    /// at resolution time.
    pub fn from_config(remotes: &RemotesConfig) -> Result<Self> {
        let specs: Vec<RemoteSpec> = remotes
            .by_name
            .iter()
            .map(|(name, remote)| RemoteSpec {
                name: Arc::from(name.as_str()),
                url: remote.url.clone(),
            })
            .collect();
        let by_name: HashMap<Arc<str>, RemoteId> = specs
            .iter()
            .enumerate()
            .map(|(i, spec)| (Arc::clone(&spec.name), RemoteId(i)))
            .collect();
        let default = match remotes
            .default
            .as_ref()
            .map(gat_core::name::RemoteName::as_str)
        {
            Some(name) => Some(by_name.get(name).copied().ok_or_else(|| {
                RemoteCatalogError::UnknownDefault {
                    name: name.to_string(),
                }
            })?),
            None => None,
        };
        Ok(Self {
            specs,
            by_name,
            default,
        })
    }

    /// Resolves an already-known remote name to its [`RemoteId`] --
    /// `None` if `name` is not a configured remote (e.g. an invalid
    /// explicit `--remote` override, or a route pointing at a name that
    /// was since removed from config).
    #[must_use]
    pub fn id_of(&self, name: &gat_core::name::RemoteName) -> Option<RemoteId> {
        self.by_name.get(name.as_str()).copied()
    }

    /// The interned name for `id` -- a cheap `Arc<str>` clone, never a
    /// fresh heap allocation, since every configured remote's name was
    /// interned exactly once at catalog compilation time.
    #[must_use]
    pub(crate) fn name(&self, id: RemoteId) -> Arc<str> {
        Arc::clone(&self.specs[id.0].name)
    }

    /// Reconstructs the semantic remote name for a command result or
    /// diagnostic boundary. Runtime routing, transfer obligations, and
    /// remote sessions retain [`RemoteId`] or an engine-internal interned
    /// name instead of allocating one typed name per selected object.
    #[must_use]
    pub fn remote_name(&self, id: RemoteId) -> RemoteName {
        RemoteName::from(self.specs[id.0].name.as_ref())
    }

    /// The raw configured URL for `id` (not yet `$ENV`-interpolated --
    /// the remote session's private client builder does that at open
    /// time). Returns a borrowed
    /// [`RemoteUrlTemplate`] rather than cloning: the raw un-interpolated
    /// text is only reached by calling the explicitly named
    /// [`RemoteUrlTemplate::as_template_str`] on it, at the interpolation
    /// boundary itself.
    #[must_use]
    pub(crate) fn url(&self, id: RemoteId) -> &RemoteUrlTemplate {
        &self.specs[id.0].url
    }

    /// Resolves `explicit` (an optional `--remote` override) to a
    /// [`RemoteId`], falling back to the repository default.
    pub fn resolve(&self, explicit: Option<&gat_core::name::RemoteName>) -> Result<RemoteId> {
        match explicit {
            Some(name) => self.id_of(name).ok_or_else(|| {
                RemoteCatalogError::UnknownOverride(
                    crate::path_policy::UnknownRemoteOverrideError {
                        name: name.to_string(),
                    },
                )
            }),
            None => self.default.ok_or(RemoteCatalogError::NoRemoteConfigured),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::name::RemoteName;
    use std::collections::BTreeMap;

    fn rn(s: &str) -> RemoteName {
        RemoteName::from_string(s.to_string())
    }

    fn remotes(names: &[&str], default: Option<&str>) -> RemotesConfig {
        RemotesConfig {
            by_name: names
                .iter()
                .map(|n| {
                    (
                        RemoteName::from_string(n.to_string()),
                        gat_core::endpoint::RemoteUrlTemplate::from_string(format!(
                            "file:///tmp/{n}"
                        ))
                        .into(),
                    )
                })
                .collect::<BTreeMap<_, _>>(),
            default: default.map(|d| RemoteName::from_string(d.to_string())),
        }
    }

    #[test]
    fn every_configured_remote_gets_a_stable_distinct_id() {
        let catalog = RemoteCatalog::from_config(&remotes(&["a", "b", "c"], Some("b"))).unwrap();
        let a = catalog.id_of(&rn("a")).unwrap();
        let b = catalog.id_of(&rn("b")).unwrap();
        let c = catalog.id_of(&rn("c")).unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(catalog.resolve(None).unwrap(), b);
        assert_eq!(&*catalog.name(a), "a");
        assert_eq!(&*catalog.name(b), "b");
        assert_eq!(&*catalog.name(c), "c");
        assert_eq!(catalog.remote_name(b), rn("b"));
    }

    /// The catalog carries each remote's
    /// endpoint URL directly, so `RemoteSession` never needs a second, independently
    /// cloned `RemotesConfig` to build an operator.
    #[test]
    fn catalog_carries_each_remotes_url() {
        let catalog = RemoteCatalog::from_config(&remotes(&["a", "b"], Some("a"))).unwrap();
        let a = catalog.id_of(&rn("a")).unwrap();
        let b = catalog.id_of(&rn("b")).unwrap();
        assert_eq!(catalog.url(a).as_template_str(), "file:///tmp/a");
        assert_eq!(catalog.url(b).as_template_str(), "file:///tmp/b");
    }

    #[test]
    fn resolve_prefers_explicit_then_falls_back_to_default() {
        let catalog = RemoteCatalog::from_config(&remotes(&["a", "b"], Some("a"))).unwrap();
        let explicit = catalog.resolve(Some(&rn("b"))).unwrap();
        assert_eq!(&*catalog.name(explicit), "b");
        let default = catalog.resolve(None).unwrap();
        assert_eq!(&*catalog.name(default), "a");
    }

    #[test]
    fn resolve_rejects_an_unconfigured_explicit_remote_name() {
        let catalog = RemoteCatalog::from_config(&remotes(&["a"], Some("a"))).unwrap();
        assert!(catalog.resolve(Some(&rn("nonexistent"))).is_err());
    }

    #[test]
    fn resolve_with_no_default_and_no_explicit_name_is_an_error() {
        let catalog = RemoteCatalog::from_config(&remotes(&["a"], None)).unwrap();
        assert!(catalog.resolve(None).is_err());
    }

    /// A `remotes.default` naming a remote absent
    /// from `remotes.by_name` must fail catalog construction itself,
    /// rather than silently compiling to `default: None` and only
    /// surfacing as "no remote configured" much later at resolution time.
    #[test]
    fn from_config_rejects_a_default_naming_an_unconfigured_remote() {
        let err = RemoteCatalog::from_config(&remotes(&["a"], Some("nonexistent"))).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn configured_url_validation_is_semantic_and_retains_the_source_without_leaking_the_url() {
        let template = RemoteUrlTemplate::from_string(
            "unsupported://host/path?token=SYNTHETIC-SECRET".to_string(),
        );
        let error = validate_remote_url(&template).unwrap_err();
        assert!(matches!(
            error.kind(),
            crate::remote_open::RemoteOpenFailureKind::DisallowedScheme
        ));
        assert_eq!(error.template(), &template);
        assert!(!format!("{error:?}").contains("SYNTHETIC-SECRET"));
        let mut chain = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(error) = source {
            chain.push_str(&error.to_string());
            source = error.source();
        }
        assert!(!chain.contains("SYNTHETIC-SECRET"), "{chain}");
        assert!(!chain.contains(template.as_template_str()), "{chain}");
    }
}
