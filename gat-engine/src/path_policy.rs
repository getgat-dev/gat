//! Compiled, indexed, operation-scoped path policy.
//!
//! Mount ownership and path-based remote routing are both derived once from
//! a [`Config`] snapshot and resolved cheaply via `PathPrefixMap`, so a
//! per-row lookup in a large operation is a bounded, indexed probe instead
//! of a linear scan over every configured mount/route.

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static POLICY_COMPARISONS: Cell<usize> = const { Cell::new(0) };
        static POLICY_COMPILATIONS: Cell<usize> = const { Cell::new(0) };
        static REMOTE_ROUTE_RESOLUTIONS: Cell<usize> = const { Cell::new(0) };
    }

    /// One prefix comparison performed by an indexed policy lookup. Bounded by
    /// the query path's depth, independent of the configured mount/route count.
    pub fn record_policy_comparison() {
        POLICY_COMPARISONS.with(|c| c.set(c.get() + 1));
    }

    /// One [`super::EffectivePathPolicy::from_config`] compilation. A
    /// well-behaved operation observes exactly one per `Operation`/`Snapshot`.
    pub fn record_policy_compilation() {
        POLICY_COMPILATIONS.with(|c| c.set(c.get() + 1));
    }

    /// One [`super::EffectivePathPolicy::resolved_remote_for_path`] call --
    /// a pure, side-effect-free policy lookup, never an I/O operation or a
    /// remote-operator initialization.
    pub fn record_remote_route_resolution() {
        REMOTE_ROUTE_RESOLUTIONS.with(|c| c.set(c.get() + 1));
    }

    pub fn policy_comparisons() -> usize {
        POLICY_COMPARISONS.with(Cell::get)
    }

    pub fn policy_compilations() -> usize {
        POLICY_COMPILATIONS.with(Cell::get)
    }

    pub fn remote_route_resolutions() -> usize {
        REMOTE_ROUTE_RESOLUTIONS.with(Cell::get)
    }
}

use super::remote_catalog::{RemoteCatalog, RemoteId};
use gat_core::config::{Config, MountOwner};
use std::collections::HashMap;

/// This module's own `Result` alias.
type Result<T> = std::result::Result<T, PathPolicyError>;

/// Every way compiling an [`EffectivePathPolicy`] from a [`Config`] can
/// fail: a route or the repository default names a remote absent from
/// the already-compiled [`RemoteCatalog`].
#[derive(Debug, thiserror::Error)]
pub enum PathPolicyError {
    /// A configured route names a remote that isn't in `remotes.by_name`.
    #[error(
        "route `{route_name}` (`{route_path}`) names remote `{remote}`, which is not configured"
    )]
    UnknownRouteRemote {
        route_name: String,
        route_path: String,
        remote: String,
    },

    /// `remotes.default` names a remote absent from the catalog. This
    /// mirrors [`super::remote_catalog::RemoteCatalogError::UnknownDefault`],
    /// which already rejects this at catalog-compilation time -- kept
    /// here too since [`EffectivePathPolicy::from_config`] independently
    /// re-resolves the default id against the catalog.
    #[error("remotes.default `{name}` is not a configured remote")]
    UnknownDefault { name: String },
}

/// An explicit CLI `--remote` override (or `--remote` on a route-derived
/// command) named a remote absent from the configured catalog. Kept as its
/// own structured error rather than an
/// opaque `.with_context()` string -- so the invalid name is a queryable
/// field, not text buried in a message.
#[derive(Debug, thiserror::Error)]
#[error("remote `{name}` is not configured")]
pub struct UnknownRemoteOverrideError {
    pub name: String,
}

/// An indexed, segment-aware, most-specific-prefix map keyed by
/// `gat.lock`-style root-relative, `/`-separated path prefixes.
///
/// Lookups walk the query path's own ancestor prefixes (from most-specific to
/// least) and probe an exact-match index, so a single lookup performs at most
/// `path.depth()` comparisons regardless of how many prefixes are configured.
/// This is the key difference from a linear `find`/`filter` over every
/// configured mount or route.
pub struct PathPrefixMap<T> {
    entries: HashMap<gat_core::lexical_path::GatPath, T>,
}

impl<T> PathPrefixMap<T> {
    const fn from_entries(entries: HashMap<gat_core::lexical_path::GatPath, T>) -> Self {
        Self { entries }
    }

    /// The most-specific (longest) configured prefix that is equal to, or a
    /// segment-wise ancestor of, `path`, together with its value. `None` when
    /// no configured prefix matches.
    fn longest_prefix(
        &self,
        path: &gat_core::lexical_path::GatPath,
    ) -> Option<(&gat_core::lexical_path::GatPath, &T)> {
        if self.entries.is_empty() {
            return None;
        }
        for prefix in ancestor_prefixes(path.as_str()) {
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_policy_comparison();
            if let Some((key, value)) = self.entries.get_key_value(prefix) {
                return Some((key, value));
            }
        }
        None
    }
}

/// Yields `path` and every segment-wise ancestor prefix of it, from the most
/// specific (the whole path) to the least (its first segment). Segment-aware:
/// `a/b/c` yields `a/b/c`, `a/b`, `a` -- never `a/b/c` matching a configured
/// `ab` prefix. This matches `config::is_within_prefix` semantics.
fn ancestor_prefixes(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(path), |p| p.rfind('/').map(|i| &p[..i]))
}

/// Indexed mount ownership derived once from an effective configuration.
/// Lookups probe only the query path's ancestors, independent of mount count.
/// This can be used by read-only reports without compiling remote routing.
pub struct MountOwnership {
    mounts: PathPrefixMap<gat_core::name::MountName>,
}

impl MountOwnership {
    /// Compile non-overlapping targets from validated effective mounts.
    #[must_use]
    pub fn new(mounts: &gat_core::config::MountsConfig) -> Self {
        Self {
            mounts: PathPrefixMap::from_entries(
                mounts
                    .by_name
                    .iter()
                    .map(|(name, mount)| (mount.target.clone(), name.clone()))
                    .collect(),
            ),
        }
    }

    /// The mount containing or equal to this path, or none for root ownership.
    #[must_use]
    pub fn owner_for_path(&self, path: &gat_core::lexical_path::GatPath) -> Option<MountOwner<'_>> {
        self.mounts
            .longest_prefix(path)
            .map(|(target, name)| MountOwner { name, target })
    }
}

/// Compiled, operation-scoped path policy: mount ownership and path-based
/// remote routing, both derived once from a [`Config`] snapshot and resolved
/// cheaply via `PathPrefixMap`. The two policies are semantically
/// independent -- ownership decides mutation authority, routing decides object
/// storage -- and neither result is written back into any desired-state row.
pub struct EffectivePathPolicy {
    mounts: MountOwnership,
    /// Keyed by route path, valued by the route's compact identity
    /// ([`RouteId`]) and the remote already compiled to a [`RemoteId`]
    /// against the operation's [`RemoteCatalog`], never re-resolved by name
    /// per row.
    routes: PathPrefixMap<CompiledRoute>,
    /// Every configured route's diagnostic-only name/path, indexed directly
    /// by [`RouteId`]. Route identity is compact in the hot resolution path;
    /// this
    /// vector is the sole place a `RouteId` is turned back into a name/
    /// path, and only ever for diagnostics/output, never for resolution
    /// itself).
    route_descriptors: Vec<RouteDescriptor>,
    default_remote_id: Option<RemoteId>,
}

/// A compact, `Copy` route identity, valid for the lifetime of the
/// [`EffectivePathPolicy`] that assigned it: every configured route is
/// assigned one `RouteId` at
/// [`EffectivePathPolicy::from_config`] time, in the same order as
/// the policy's private route descriptor table, so `id.0` is always a
/// valid index into that vector. Mirrors [`RemoteId`]'s existing shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RouteId(usize);

/// One configured route's diagnostic-only identity: its stable name and
/// configured path. Never consulted during resolution itself -- only
/// [`EffectivePathPolicy::route_descriptor`] turns a [`RouteId`] back
/// into one of these, and only when constructing a diagnostic/output
/// value, never on the [`EffectivePathPolicy::resolved_remote_for_path`]
/// success path.
pub(crate) struct RouteDescriptor {
    pub name: gat_core::name::RouteName,
    pub path: gat_core::lexical_path::GatPath,
}

/// One compiled route: only the compact [`RouteId`]/[`RemoteId`] pair
/// transfer planning's hot path actually uses, resolved once at
/// [`EffectivePathPolicy::from_config`] time against the operation's
/// [`RemoteCatalog`] rather than by string lookup on every selected row.
/// Mandatory: a route naming a remote absent
/// from the catalog fails policy compilation itself (see
/// [`EffectivePathPolicy::from_config`]), so every compiled route already
/// carries a valid id and never needs its configured remote name again.
struct CompiledRoute {
    id: RouteId,
    remote_id: RemoteId,
}

/// The remote a path's object bytes resolve to, plus the route that
/// selected it (if any), as a compact `Copy` value: no name/path text is
/// carried on the resolution success path at all; a caller that needs the
/// remote name or the route's name for a diagnostic resolves it lazily
/// through [`RemoteCatalog::remote_name`]/[`EffectivePathPolicy::route_name`].
/// `route` is `None` for an explicit `--remote` override or a
/// fall-through to the repository default remote.
///
/// `id` is mandatory: a configured route/default
/// match is always backed by a valid catalog id -- `RemoteCatalog::from_config`
/// rejects an invalid `remotes.default` and
/// `EffectivePathPolicy::from_config` rejects a route naming a remote
/// absent from the catalog (below), so every route/default path has an id.
/// An explicit CLI `--remote` override is the one case still resolved at
/// call time (it isn't known at policy-compilation time); an unconfigured
/// override name is propagated as an error by
/// [`EffectivePathPolicy::resolved_remote_for_path`] rather than producing
/// a `ResolvedRemote` with no id.
///
/// Only routing policy can pair a remote with its selecting route:
///
/// ```compile_fail
/// use gat_engine::{RemoteId, ResolvedRemote};
/// fn forge(id: RemoteId) -> ResolvedRemote {
///     ResolvedRemote { id, route: None }
/// }
/// ```
///
/// ```compile_fail
/// use gat_engine::ResolvedRemote;
/// fn rewrite(mut remote: ResolvedRemote) {
///     remote.route = None;
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedRemote {
    id: RemoteId,
    route: Option<RouteId>,
}

impl ResolvedRemote {
    /// The configured remote selected by routing policy.
    #[must_use]
    #[inline]
    pub const fn id(&self) -> RemoteId {
        self.id
    }

    /// The route that selected the remote, if any.
    #[must_use]
    #[inline]
    pub(crate) const fn route(&self) -> Option<RouteId> {
        self.route
    }

    #[cfg(test)]
    pub(crate) const fn for_test(id: RemoteId) -> Self {
        Self::from_compiled(id, None)
    }

    /// Resolves an explicit CLI `--remote` override `name` against
    /// `catalog`, deriving `id` from the same lookup that validates
    /// `name`. Errors if `name` is not a configured remote; an invalid
    /// override is a caller mistake that
    /// must surface immediately, not a `ResolvedRemote` whose `id`
    /// silently has no backing catalog entry.
    fn from_explicit_name(
        catalog: &RemoteCatalog,
        name: &gat_core::name::RemoteName,
    ) -> std::result::Result<Self, UnknownRemoteOverrideError> {
        let id = catalog
            .id_of(name)
            .ok_or_else(|| UnknownRemoteOverrideError {
                name: name.to_string(),
            })?;
        Ok(Self { id, route: None })
    }

    /// Builds a `ResolvedRemote` from an already-compiled route/default
    /// entry: `remote_id` was resolved once
    /// against the catalog at policy-compilation time (and already
    /// validated -- see [`EffectivePathPolicy::from_config`]), so this
    /// performs no catalog lookup, no allocation, and cannot fail.
    const fn from_compiled(remote_id: RemoteId, route: Option<RouteId>) -> Self {
        Self {
            id: remote_id,
            route,
        }
    }
}

impl EffectivePathPolicy {
    /// Compile the ownership + routing policies from one effective-config
    /// snapshot. `catalog` is the same operation-scoped [`RemoteCatalog`]
    /// `Snapshot::new` compiles first: every configured route's and the
    /// repository default's
    /// remote name is resolved to a [`RemoteId`] right here, once, rather
    /// than deferring that lookup to every selected row's
    /// [`Self::resolved_remote_for_path`] call. Rejects a route naming a
    /// remote absent from `catalog`, the same
    /// way `RemoteCatalog::from_config` already rejects an invalid
    /// `remotes.default` -- a stale/typo'd route target must fail
    /// operation setup, not silently resolve to no id at selection time.
    pub fn from_config(config: &Config, catalog: &RemoteCatalog) -> Result<Self> {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_policy_compilation();
        let mounts = MountOwnership::new(&config.mounts);
        let mut routes = HashMap::with_capacity(config.routes.by_name.len());
        let mut route_descriptors = Vec::with_capacity(config.routes.by_name.len());
        for (name, route) in &config.routes.by_name {
            let remote_id = catalog.id_of(&route.remote).ok_or_else(|| {
                PathPolicyError::UnknownRouteRemote {
                    route_name: name.to_string(),
                    route_path: route.path.to_string(),
                    remote: route.remote.to_string(),
                }
            })?;
            let id = RouteId(route_descriptors.len());
            route_descriptors.push(RouteDescriptor {
                name: name.clone(),
                path: route.path.clone(),
            });
            routes.insert(route.path.clone(), CompiledRoute { id, remote_id });
        }
        let default_remote_id = config
            .remotes
            .default
            .as_ref()
            .map(|name| {
                catalog
                    .id_of(name)
                    .ok_or_else(|| PathPolicyError::UnknownDefault {
                        name: name.to_string(),
                    })
            })
            .transpose()?;
        Ok(Self {
            mounts,
            routes: PathPrefixMap::from_entries(routes),
            route_descriptors,
            default_remote_id,
        })
    }

    /// The diagnostic-only name/path a compiled route's [`RouteId`] stands
    /// for. Never consulted by [`Self::resolved_remote_for_path`]'s
    /// success path itself -- only a caller building a diagnostic/output
    /// value from an already-resolved [`ResolvedRemote::route`] calls
    /// this.
    #[must_use]
    pub(crate) fn route_descriptor(&self, id: RouteId) -> &RouteDescriptor {
        &self.route_descriptors[id.0]
    }

    /// The route that selected this remote, borrowed from the same policy
    /// that resolved it. Explicit overrides and default remotes have no route.
    /// The lookup does not clone the name or repeat route resolution.
    #[must_use]
    #[inline]
    pub fn route_name(&self, remote: &ResolvedRemote) -> Option<&gat_core::name::RouteName> {
        remote.route.map(|id| &self.route_descriptors[id.0].name)
    }

    /// The mount that owns `path` (equal to, or nested beneath, its target),
    /// if any. `None` means `path` is root-owned. Mount targets never overlap
    /// (enforced by `MountsConfig::check_target`), so the most-specific match
    /// is the only match -- identical semantics to `MountsConfig::owner_of`,
    /// but indexed rather than a linear scan.
    #[must_use]
    pub fn owner_for_path<'a>(
        &'a self,
        path: &gat_core::lexical_path::GatPath,
    ) -> Option<MountOwner<'a>> {
        self.mounts.owner_for_path(path)
    }

    /// The effective remote for `path`, reporting which route selected it (for
    /// diagnostics): an explicit `--remote` override wins, then the
    /// most-specific configured route, then the repository default remote. An
    /// explicit override or default fall-through carries no route. `Ok(None)`
    /// means no remote is configured for `path` at all (no route matched and
    /// no default is configured). `catalog` is the same operation-scoped
    /// [`RemoteCatalog`] the resulting [`ResolvedRemote::id`] is derived
    /// from. Only the explicit override path performs
    /// a catalog lookup here -- a route/default match was already compiled
    /// (and validated) by [`Self::from_config`] --
    /// and only the explicit override path can fail: an invalid `--remote`
    /// name is propagated as `Err` instead of
    /// silently producing a `ResolvedRemote` with no id.
    pub fn resolved_remote_for_path(
        &self,
        catalog: &RemoteCatalog,
        explicit: Option<&gat_core::name::RemoteName>,
        path: &gat_core::lexical_path::GatPath,
    ) -> std::result::Result<Option<ResolvedRemote>, UnknownRemoteOverrideError> {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_remote_route_resolution();
        if let Some(name) = explicit {
            return Ok(Some(ResolvedRemote::from_explicit_name(catalog, name)?));
        }
        if let Some((_, compiled)) = self.routes.longest_prefix(path) {
            return Ok(Some(ResolvedRemote::from_compiled(
                compiled.remote_id,
                Some(compiled.id),
            )));
        }
        Ok(self
            .default_remote_id
            .map(|id| ResolvedRemote::from_compiled(id, None)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::config::{MountConfig, MountsConfig, RemotesConfig, RouteConfig, RoutesConfig};
    use gat_core::name::{MountName, RemoteName, RouteName};
    use std::collections::BTreeMap;

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path).unwrap()
    }

    fn rn(name: &str) -> RemoteName {
        RemoteName::from_string(name.to_string())
    }

    fn mount(target: &str) -> MountConfig {
        MountConfig {
            // hygiene-ok: pure config-value string for a fixture MountConfig; never dialed as a real URL.
            url: "https://example.invalid/repo.git".to_string().into(),
            target: gp(target),
            path: gat_core::lexical_path::GatSubpath::Root,
            rev: None,
            rev_lock: None,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    /// Builds a policy and its backing [`RemoteCatalog`] together from one
    /// consistent config, so `remotes.by_name` always contains every
    /// remote a configured route or the default names.
    fn policy_with_routes(
        routes: &[(&str, &str)],
        default: Option<&str>,
    ) -> (EffectivePathPolicy, RemoteCatalog) {
        let by_name: BTreeMap<RemoteName, gat_core::endpoint::RemoteUrlTemplate> = routes
            .iter()
            .map(|(_, r)| *r)
            .chain(default)
            .map(|name| {
                (
                    RemoteName::from_string(name.to_string()),
                    gat_core::endpoint::RemoteUrlTemplate::from_string(format!(
                        "file:///tmp/{name}"
                    )),
                )
            })
            .collect();
        let cfg = Config {
            routes: RoutesConfig {
                by_name: routes
                    .iter()
                    .map(|(p, r)| {
                        (
                            RouteName::from_string(p.to_string()),
                            RouteConfig {
                                path: gp(p),
                                remote: RemoteName::from_string(r.to_string()),
                            },
                        )
                    })
                    .collect(),
            },
            remotes: RemotesConfig {
                by_name: by_name
                    .into_iter()
                    .map(|(name, url)| (name, url.into()))
                    .collect(),
                default: default.map(|d| RemoteName::from_string(d.to_string())),
            },
            ..Config::default()
        };
        let catalog = RemoteCatalog::from_config(&cfg.remotes).unwrap();
        let policy = EffectivePathPolicy::from_config(&cfg, &catalog).unwrap();
        (policy, catalog)
    }

    #[test]
    fn remote_for_path_prefers_explicit_then_most_specific_route_then_default() {
        let (policy, catalog) = policy_with_routes(
            &[
                ("vendor", "bulk"),
                ("vendor/models", "models"),
                ("vendor/models/private", "secure"),
            ],
            Some("origin"),
        );
        let remote = |explicit: Option<&str>, path| {
            let explicit = explicit.map(rn);
            policy
                .resolved_remote_for_path(&catalog, explicit.as_ref(), &gp(path))
                .unwrap()
                .map(|r| catalog.name(r.id).to_string())
        };

        assert_eq!(
            remote(Some("bulk"), "vendor/models/private/a.bin"),
            Some("bulk".to_string())
        );
        assert_eq!(
            remote(None, "vendor/models/private/a.bin"),
            Some("secure".to_string())
        );
        assert_eq!(
            remote(None, "vendor/models/public/a.bin"),
            Some("models".to_string())
        );
        assert_eq!(remote(None, "vendor/other/a.bin"), Some("bulk".to_string()));
        assert_eq!(remote(None, "other/a.bin"), Some("origin".to_string()));

        let (no_default, no_default_catalog) = policy_with_routes(&[("vendor", "bulk")], None);
        assert_eq!(
            no_default
                .resolved_remote_for_path(&no_default_catalog, None, &gp("other/a.bin"))
                .unwrap(),
            None
        );
    }

    /// `ResolvedRemote::id` is mandatory and derived from the same catalog
    /// lookup that produced `name`, not an independent authority -- so it
    /// always matches `catalog.id_of(name)` for a genuinely configured
    /// name. A configured route/default cannot name a remote absent
    /// from the catalog at all (`RemoteCatalog::from_config` rejects that
    /// at construction); a stale/misconfigured
    /// *explicit* override name that isn't in the catalog is rejected as
    /// an error at the policy boundary instead;
    /// see `resolved_remote_for_path_rejects_an_unconfigured_explicit_override`.
    #[test]
    fn resolved_remote_id_matches_the_catalog_lookup_for_the_same_name() {
        let (policy, catalog) = policy_with_routes(&[("vendor", "bulk")], Some("origin"));

        let routed = policy
            .resolved_remote_for_path(&catalog, None, &gp("vendor/a.bin"))
            .unwrap()
            .unwrap();
        assert_eq!(routed.id, catalog.id_of(&rn("bulk")).unwrap());
        assert_eq!(&*catalog.name(routed.id), "bulk");
        let route_name = policy.route_name(&routed).unwrap();
        assert_eq!(route_name.as_str(), "vendor");
        assert!(std::ptr::eq(
            route_name,
            &raw const policy.route_descriptors[0].name
        ));

        let defaulted = policy
            .resolved_remote_for_path(&catalog, None, &gp("other/a.bin"))
            .unwrap()
            .unwrap();
        assert_eq!(defaulted.id, catalog.id_of(&rn("origin")).unwrap());
        assert_eq!(policy.route_name(&defaulted), None);

        let overridden = policy
            .resolved_remote_for_path(&catalog, Some(&rn("origin")), &gp("vendor/a.bin"))
            .unwrap()
            .unwrap();
        assert_eq!(policy.route_name(&overridden), None);
    }

    /// An explicit `--remote` override naming a
    /// remote absent from the catalog must propagate as an error at the
    /// policy boundary -- not manufacture a `ResolvedRemote` whose `id` has
    /// no backing catalog entry and defer failure to operator opening.
    #[test]
    fn resolved_remote_for_path_rejects_an_unconfigured_explicit_override() {
        let (policy, catalog) = policy_with_routes(&[("vendor", "bulk")], Some("origin"));

        let err = policy
            .resolved_remote_for_path(&catalog, Some(&rn("removed")), &gp("other/a.bin"))
            .unwrap_err();
        assert!(err.to_string().contains("removed"));
    }

    #[test]
    fn owner_for_path_reports_the_owning_mount_name_and_target() {
        let cfg = Config {
            mounts: MountsConfig {
                by_name: BTreeMap::from([
                    (
                        MountName::from_string("weights".to_string()),
                        mount("vendor/models"),
                    ),
                    (
                        MountName::from_string("data".to_string()),
                        mount("datasets"),
                    ),
                ]),
            },
            ..Config::default()
        };
        let catalog = RemoteCatalog::from_config(&cfg.remotes).unwrap();
        let policy = EffectivePathPolicy::from_config(&cfg, &catalog).unwrap();

        let owner = policy
            .owner_for_path(&gp("vendor/models/weights.bin"))
            .unwrap();
        assert_eq!(owner.name, "weights");
        assert_eq!(owner.target, "vendor/models");
        assert_eq!(policy.owner_for_path(&gp("vendor/other/a.bin")), None);
        assert!(policy.owner_for_path(&gp("vendor/models")).is_some());
        // Segment-aware: `vendor/models2` is not within `vendor/models`.
        assert_eq!(policy.owner_for_path(&gp("vendor/models2/a.bin")), None);
    }

    /// `owner_for_path` (used by `add`/`mv`/`rm`/push
    /// ownership filtering) must resolve in a number of comparisons bounded
    /// by the query path's depth, not the number of configured mounts --
    /// the whole point of compiling mounts into the indexed policy instead
    /// of a per-row `MountsConfig::owner_of` linear scan.
    #[test]
    fn owner_for_path_lookup_is_bounded_by_path_depth_not_mount_count() {
        let mounts: BTreeMap<MountName, MountConfig> = (0..200)
            .map(|i| {
                (
                    MountName::from_string(format!("mount{i}")),
                    mount(&format!("vendor/target-{i}")),
                )
            })
            .collect();
        let cfg = Config {
            mounts: MountsConfig { by_name: mounts },
            ..Config::default()
        };
        let ownership = MountOwnership::new(&cfg.mounts);
        for raw in [
            "vendor/target-150",
            "vendor/target-150/weights.bin",
            "vendor/target-1500",
            "vendor",
            "unowned/a/b/c.bin",
        ] {
            let path = gp(raw);
            let before = test_support::policy_comparisons();
            assert_eq!(ownership.owner_for_path(&path), cfg.mounts.owner_of(&path));
            assert!(test_support::policy_comparisons() - before <= raw.split('/').count());
        }
        let catalog = RemoteCatalog::from_config(&cfg.remotes).unwrap();
        let policy = EffectivePathPolicy::from_config(&cfg, &catalog).unwrap();

        // A depth-3 hit resolves in at most 3 comparisons (its own ancestor
        // prefixes), not 200 (one per configured mount).
        let before = test_support::policy_comparisons();
        let owner = policy.owner_for_path(&gp("vendor/target-150/weights.bin"));
        let after = test_support::policy_comparisons();
        assert_eq!(
            owner.map(|o| o.name.to_string()),
            Some("mount150".to_string())
        );
        let comparisons = after - before;
        assert!(
            comparisons <= 3,
            "expected <= 3 comparisons for a depth-3 path, got {comparisons}"
        );

        // A miss walks only the query path's depth too, never the mount count.
        let before = test_support::policy_comparisons();
        let owner = policy.owner_for_path(&gp("unowned/a/b/c.bin"));
        let after = test_support::policy_comparisons();
        assert_eq!(owner, None);
        let comparisons = after - before;
        assert!(
            comparisons <= 4,
            "expected <= 4 comparisons for a depth-4 miss, got {comparisons}"
        );
    }

    /// The whole point of the indexed policy: comparisons per lookup stay
    /// bounded by the query path's depth regardless of how many routes are
    /// configured, instead of scanning every route per row.
    #[test]
    fn indexed_lookup_is_bounded_by_path_depth_not_route_count() {
        let routes: Vec<(String, String)> = (0..200)
            .map(|i| (format!("route/{i}"), format!("remote{i}")))
            .collect();
        let by_name: BTreeMap<RemoteName, gat_core::endpoint::RemoteUrlTemplate> = routes
            .iter()
            .map(|(_, r)| r.clone())
            .chain(std::iter::once("origin".to_string()))
            .map(|name| {
                (
                    RemoteName::from_string(name.clone()),
                    gat_core::endpoint::RemoteUrlTemplate::from_string(format!(
                        "file:///tmp/{name}"
                    )),
                )
            })
            .collect();
        let cfg = Config {
            routes: RoutesConfig {
                by_name: routes
                    .iter()
                    .map(|(p, r)| {
                        (
                            RouteName::from_string(p.clone()),
                            RouteConfig {
                                path: gp(p),
                                remote: RemoteName::from_string(r.clone()),
                            },
                        )
                    })
                    .collect(),
            },
            remotes: RemotesConfig {
                by_name: by_name
                    .into_iter()
                    .map(|(name, url)| (name, url.into()))
                    .collect(),
                default: Some(RemoteName::from_string("origin".to_string())),
            },
            ..Config::default()
        };
        let catalog = RemoteCatalog::from_config(&cfg.remotes).unwrap();
        let policy = EffectivePathPolicy::from_config(&cfg, &catalog).unwrap();
        // comparisons (its own ancestor prefixes), not 200.
        let before = test_support::policy_comparisons();
        let remote = policy
            .resolved_remote_for_path(&catalog, None, &gp("route/42/file.bin"))
            .unwrap()
            .map(|r| catalog.name(r.id).to_string());
        let after = test_support::policy_comparisons();
        assert_eq!(remote, Some("remote42".to_string()));
        let comparisons = after - before;
        assert!(
            comparisons <= 3,
            "expected <= 3 comparisons for a depth-3 path, got {comparisons}"
        );

        // A miss walks only the query path's depth too, never the route count.
        let before = test_support::policy_comparisons();
        let _ = policy.resolved_remote_for_path(&catalog, None, &gp("unrouted/a/b/c.bin"));
        let after = test_support::policy_comparisons();
        let comparisons = after - before;
        assert!(
            comparisons <= 4,
            "expected <= 4 comparisons for a depth-4 miss, got {comparisons}"
        );
    }
}
