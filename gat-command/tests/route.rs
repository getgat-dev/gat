use gat_command::{
    DefaultRemoteRoute, RemoteRequest, RouteDetails, RouteError, RouteOutcome, RouteRequest,
    remote, route,
};
use gat_core::config::{ConfigScope, normalize_route_path};
use gat_core::endpoint::RemoteUrlTemplate;
use gat_core::lexical_path::GatPath;
use gat_core::name::{RemoteName, RouteName};
use gat_engine::Repository;

fn repository() -> (tempfile::TempDir, Repository) {
    let temp = tempfile::tempdir().unwrap();
    test_support_git::run_git(temp.path(), &["init", "-q", "-b", "main"]);
    let repo = Repository::at(temp.path().to_path_buf());
    (temp, repo)
}

fn route_name(name: &str) -> RouteName {
    RouteName::from_string(name.to_string())
}

fn remote_name(name: &str) -> RemoteName {
    RemoteName::from_string(name.to_string())
}

fn path(path: &str) -> GatPath {
    normalize_route_path(path).unwrap()
}

fn add_remote(repo: &Repository, name: &str) {
    remote(
        repo,
        RemoteRequest::Add {
            name: remote_name(name),
            url: RemoteUrlTemplate::from_string(format!("file:///tmp/{name}")),
            scope: ConfigScope::Project,
        },
    )
    .unwrap();
}

fn add_route(
    repo: &Repository,
    name: &str,
    remote: &str,
    route_path: &str,
    scope: ConfigScope,
) -> Result<RouteOutcome, RouteError> {
    route(
        repo,
        RouteRequest::Add {
            name: route_name(name),
            remote: remote_name(remote),
            path: path(route_path),
            scope,
        },
    )
}

#[test]
fn add_validates_typed_inputs_and_most_specific_route_wins() {
    let (_temp, repo) = repository();
    add_remote(&repo, "bulk");
    add_remote(&repo, "secure");

    let error = add_route(
        &repo,
        "missing",
        "unknown",
        "vendor/missing",
        ConfigScope::Project,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RouteError::UnknownRemote { remote } if remote == remote_name("unknown")
    ));

    add_route(&repo, "models", "bulk", "vendor", ConfigScope::Project).unwrap();
    add_route(
        &repo,
        "private-models",
        "secure",
        "vendor/models/private",
        ConfigScope::Project,
    )
    .unwrap();

    let config = repo.load_config().unwrap();
    assert_eq!(
        config
            .routes
            .route_for(&path("vendor/models/public/a.bin"))
            .unwrap()
            .remote
            .as_str(),
        "bulk"
    );
    assert_eq!(
        config
            .routes
            .route_for(&path("vendor/models/private/a.bin"))
            .unwrap()
            .remote
            .as_str(),
        "secure"
    );
}

#[test]
fn add_rejects_reserved_names_duplicate_names_and_duplicate_paths_without_mutation() {
    let (_temp, repo) = repository();
    add_remote(&repo, "bulk");
    add_remote(&repo, "secure");

    let reserved =
        add_route(&repo, "*", "bulk", "vendor/reserved", ConfigScope::Project).unwrap_err();
    assert!(matches!(reserved, RouteError::ReservedName));
    assert!(!repo.load_config().unwrap().routes.by_name.contains_key("*"));

    add_route(
        &repo,
        "models",
        "bulk",
        "vendor/models",
        ConfigScope::Project,
    )
    .unwrap();

    let duplicate_name = add_route(
        &repo,
        "models",
        "secure",
        "vendor/other",
        ConfigScope::Project,
    )
    .unwrap_err();
    assert!(matches!(
        duplicate_name,
        RouteError::AlreadyExists { name } if name == route_name("models")
    ));

    let duplicate_path = add_route(
        &repo,
        "models-alt",
        "secure",
        "vendor/models",
        ConfigScope::Project,
    )
    .unwrap_err();
    assert!(matches!(duplicate_path, RouteError::Repository(_)));

    let config = repo.load_config().unwrap();
    assert_eq!(config.routes.by_name.len(), 1);
    assert_eq!(
        config.routes.by_name[&route_name("models")].remote,
        remote_name("bulk")
    );
}

#[test]
fn update_list_show_and_remove_preserve_identity_scope_and_default_fallback() {
    let (_temp, repo) = repository();
    add_remote(&repo, "origin");
    add_remote(&repo, "secure");
    add_route(
        &repo,
        "models",
        "origin",
        "vendor/models",
        ConfigScope::Project,
    )
    .unwrap();

    let updated = route(
        &repo,
        RouteRequest::Update {
            name: route_name("models"),
            remote: Some(remote_name("secure")),
            path: None,
            scope: ConfigScope::Project,
        },
    )
    .unwrap();
    assert!(matches!(
        updated,
        RouteOutcome::Updated { name, path: updated_path, remote }
            if name == route_name("models")
                && updated_path == path("vendor/models")
                && remote == remote_name("secure")
    ));

    let updated = route(
        &repo,
        RouteRequest::Update {
            name: route_name("models"),
            remote: None,
            path: Some(path("vendor/models/v2")),
            scope: ConfigScope::Project,
        },
    )
    .unwrap();
    assert!(matches!(
        updated,
        RouteOutcome::Updated { name, path: updated_path, remote }
            if name == route_name("models")
                && updated_path == path("vendor/models/v2")
                && remote == remote_name("secure")
    ));

    add_route(
        &repo,
        "models",
        "origin",
        "vendor/models/local",
        ConfigScope::Local,
    )
    .unwrap();
    assert!(matches!(
        route(
            &repo,
            RouteRequest::Update {
                name: route_name("models"),
                remote: None,
                path: None,
                scope: ConfigScope::Project,
            }
        ),
        Err(RouteError::Scope(_))
    ));
    let shown = route(
        &repo,
        RouteRequest::Show {
            name: route_name("models"),
        },
    )
    .unwrap();
    assert_eq!(
        shown,
        RouteOutcome::Show(RouteDetails {
            name: route_name("models"),
            path: path("vendor/models/local"),
            remote: remote_name("origin"),
            scope: ConfigScope::Local,
        })
    );

    let listed = route(&repo, RouteRequest::List).unwrap();
    let RouteOutcome::List {
        routes,
        default_remote,
    } = listed
    else {
        panic!("expected route list");
    };
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].name, route_name("models"));
    assert!(matches!(default_remote, DefaultRemoteRoute::Missing));

    route(
        &repo,
        RouteRequest::Remove {
            name: route_name("models"),
            scope: ConfigScope::Local,
        },
    )
    .unwrap();
    let shown = route(
        &repo,
        RouteRequest::Show {
            name: route_name("models"),
        },
    )
    .unwrap();
    assert!(matches!(
        shown,
        RouteOutcome::Show(RouteDetails {
            path: revealed_path,
            remote,
            scope: ConfigScope::Project,
            ..
        }) if revealed_path == path("vendor/models/v2") && remote == remote_name("secure")
    ));

    route(
        &repo,
        RouteRequest::Remove {
            name: route_name("models"),
            scope: ConfigScope::Project,
        },
    )
    .unwrap();
    let missing = route(
        &repo,
        RouteRequest::Remove {
            name: route_name("models"),
            scope: ConfigScope::Project,
        },
    )
    .unwrap_err();
    assert!(matches!(
        missing,
        RouteError::NotFound { name } if name == route_name("models")
    ));
}

#[test]
fn remove_validates_the_effective_routes_before_publishing_scope_changes() {
    let (_temp, repo) = repository();
    add_remote(&repo, "bulk");
    add_remote(&repo, "secure");
    add_route(
        &repo,
        "models",
        "bulk",
        "vendor/models",
        ConfigScope::Project,
    )
    .unwrap();
    add_route(
        &repo,
        "models",
        "bulk",
        "vendor/models-relocated",
        ConfigScope::Local,
    )
    .unwrap();
    add_route(
        &repo,
        "other",
        "secure",
        "vendor/models",
        ConfigScope::Project,
    )
    .unwrap();
    let before = repo.load_config_scoped(ConfigScope::Local).unwrap();

    let error = route(
        &repo,
        RouteRequest::Remove {
            name: route_name("models"),
            scope: ConfigScope::Local,
        },
    )
    .unwrap_err();
    assert!(matches!(error, RouteError::Repository(_)));
    assert_eq!(
        repo.load_config_scoped(ConfigScope::Local).unwrap(),
        before,
        "failed validation must leave the selected scope unchanged"
    );

    let (_safe_temp, safe_repo) = repository();
    add_remote(&safe_repo, "bulk");
    add_remote(&safe_repo, "secure");
    add_route(
        &safe_repo,
        "models",
        "bulk",
        "vendor/models",
        ConfigScope::Project,
    )
    .unwrap();
    add_route(
        &safe_repo,
        "models",
        "secure",
        "vendor/models",
        ConfigScope::Local,
    )
    .unwrap();
    route(
        &safe_repo,
        RouteRequest::Remove {
            name: route_name("models"),
            scope: ConfigScope::Local,
        },
    )
    .unwrap();
    assert_eq!(
        safe_repo
            .load_config()
            .unwrap()
            .routes
            .route_for(&path("vendor/models/a.bin"))
            .unwrap()
            .remote
            .as_str(),
        "bulk"
    );
}
