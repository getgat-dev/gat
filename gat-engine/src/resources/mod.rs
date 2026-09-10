//! Semantic resource operations; generic document publication stays internal.
mod remote;
pub use remote::{RemoteDefault, RemoteError, RemoteOutcome, RemoteRecord, RemoteRequest, remote};
mod route;
pub use route::{
    DefaultRemoteRoute, RouteDetails, RouteError, RouteOutcome, RouteRecord, RouteRequest, route,
};
mod saved_selection;
pub use saved_selection::{
    SavedSelectionError, SelectionDefault, SelectionOutcome, SelectionRecord, SelectionRequest,
    named_selection, saved_selection,
};
mod mount;
pub use mount::{
    MatchedRoute, MountDetails, MountError, MountOutcome, MountRecord, MountRequest,
    MountRouteBootstrap, mount,
};
mod resource;
pub use resource::{ResourceKind, ResourceScopeError};

/// An exclusive action on a named-resource default.
///
/// A request cannot combine setting a name with unsetting the default:
/// ```compile_fail
/// use gat_engine::{DefaultAction, RemoteRequest};
/// use gat_core::config::ConfigScope;
/// let request = RemoteRequest::Default {
///     action: DefaultAction::Unset,
///     name: Some("archive".into()),
///     scope: ConfigScope::Project,
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultAction<Name> {
    Get,
    Set(Name),
    Unset,
}

impl<Name> DefaultAction<Name> {
    fn into_name(self) -> Option<Name> {
        match self {
            Self::Set(name) => Some(name),
            Self::Get | Self::Unset => None,
        }
    }
}
