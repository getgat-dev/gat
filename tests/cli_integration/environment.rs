//! `${VAR}` remote URL template interpolation, and `HOME`/`GAT_CACHE_DIR`
//! real process-environment wiring. Exercised via a child process
//! (`gat_with_env`, rather than in-process `std::env::set_var`/
//! `remove_var`, which is unsound to do concurrently across test
//! threads) so the production wrappers that read these variables get
//! coverage that they actually consume the real environment, not just
//! the pure resolvers unit-tested against explicit inputs elsewhere.

use crate::common;
use crate::common::{assert_ok, commit_all, gat, init_repo};
use crate::support::remote_url;

#[test]
fn remote_add_interpolates_a_template_url_from_the_real_environment() {
    let tmp = init_repo();
    let dir = tmp.path();
    let remote_dir = tempfile::tempdir().unwrap();

    let out = common::gat_with_env(
        dir,
        &["remote", "add", "origin", "${GAT_TEST_CLI_REMOTE_URL}"],
        &[(
            "GAT_TEST_CLI_REMOTE_URL",
            Some(remote_url(remote_dir.path()).as_str()),
        )],
    );
    assert_ok(&out, "gat remote add with a ${VAR} template url");
    assert_ok(
        &gat(dir, &["remote", "default", "origin"]),
        "choose default remote",
    );

    // gat.yaml stores the `${VAR}` template, never the interpolated URL.
    let yaml = std::fs::read_to_string(dir.join("gat.yaml")).unwrap();
    assert!(yaml.contains("${GAT_TEST_CLI_REMOTE_URL}"));
    assert!(!yaml.contains(&remote_url(remote_dir.path())));

    // The template resolves against the real environment when actually
    // used: pushing to it must succeed.
    std::fs::write(dir.join("big.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    commit_all(dir, "add big.bin");
    let push = common::gat_with_env(
        dir,
        &["push"],
        &[(
            "GAT_TEST_CLI_REMOTE_URL",
            Some(remote_url(remote_dir.path()).as_str()),
        )],
    );
    assert_ok(&push, "gat push through a ${VAR} template remote");
}

#[test]
fn config_global_writes_under_the_real_home_environment_variable() {
    let tmp = init_repo();
    let dir = tmp.path();
    let fake_home = tempfile::tempdir().unwrap();

    let home_var = common::HOME_ENV_VAR;

    let out = common::gat_with_env(
        dir,
        &[
            "config",
            "cache.materialization_strategy",
            "hardlink",
            "--global",
        ],
        &[(home_var, Some(fake_home.path().to_str().unwrap()))],
    );
    assert_ok(
        &out,
        "gat config cache.materialization_strategy hardlink --global",
    );

    let global_config = fake_home.path().join(".gat").join("gat.yaml");
    assert!(
        global_config.is_file(),
        "expected {home_var} to steer the global gat.yaml under the fake home directory"
    );
    let yaml = std::fs::read_to_string(&global_config).unwrap();
    assert!(yaml.contains("hardlink"));
}

#[test]
fn add_ingests_into_the_directory_named_by_gat_cache_dir() {
    let tmp = init_repo();
    let dir = tmp.path();
    let shared_cache = tempfile::tempdir().unwrap();
    std::fs::write(dir.join("big.bin"), b"payload routed via GAT_CACHE_DIR").unwrap();

    let out = common::gat_with_env(
        dir,
        &["add", "big.bin"],
        &[("GAT_CACHE_DIR", Some(shared_cache.path().to_str().unwrap()))],
    );
    assert_ok(&out, "gat add with GAT_CACHE_DIR set");

    let default_cache = dir.join(".gat").join("objects");
    let has_default_objects =
        std::fs::read_dir(&default_cache).is_ok_and(|mut entries| entries.next().is_some());
    assert!(
        !has_default_objects,
        "GAT_CACHE_DIR should have redirected ingestion away from the repo-local default cache"
    );

    let has_shared_objects = walk_has_any_file(shared_cache.path());
    assert!(
        has_shared_objects,
        "expected the ingested object under GAT_CACHE_DIR's directory: {}",
        shared_cache.path().display()
    );
}

/// Proves that an *ordinary* spawned integration command (`gat add`, run
/// through the plain [`common::gat`] helper with no `extra_env`
/// override, against an [`init_repo`] fixture) uses only the
/// child-specific environment [`common::gat_with_env`] establishes by
/// default, never whatever ambient-looking Gat state happens to exist elsewhere on
/// disk. Builds a deliberately conflicting fake global Gat config (a
/// `gat.yaml` relocating the cache) and a deliberately conflicting
/// `GAT_CACHE_DIR` relocation, first proving the spawned binary *does*
/// honor both when a test explicitly opts into them via `extra_env` (so
/// the assertion that follows is meaningful, not vacuous), then proving
/// a completely ordinary spawned `gat init`/`gat add` -- run without any
/// `extra_env` override, on a separate repo -- resolves to gat's own
/// deterministic `<repo>/.gat/objects` default regardless, unaffected by
/// either conflicting setting sitting on disk.
#[test]
fn spawned_ordinary_commands_ignore_a_conflicting_global_config_and_cache_dir_elsewhere() {
    // Deliberately conflicting fake global config + cache relocation.
    let conflicting_home = tempfile::tempdir().unwrap();
    let conflicting_global_config = conflicting_home.path().join(".gat");
    std::fs::create_dir_all(&conflicting_global_config).unwrap();
    let conflicting_cache_location = conflicting_home.path().join("conflicting-global-cache");
    std::fs::write(
        conflicting_global_config.join("gat.yaml"),
        format!(
            "cache:\n  location: {}\n",
            conflicting_cache_location.display()
        ),
    )
    .unwrap();
    let conflicting_cache_dir = tempfile::tempdir().unwrap();

    // Sanity: prove the spawned binary actually honors this conflicting
    // config/relocation when a test explicitly supplies it via
    // `extra_env`, so the isolation proven below is meaningful.
    let honoring_repo = init_repo();
    std::fs::write(honoring_repo.path().join("big.bin"), b"payload").unwrap();
    let honoring_out = common::gat_with_env(
        honoring_repo.path(),
        &["add", "big.bin"],
        &[(
            "GAT_CACHE_DIR",
            Some(conflicting_cache_dir.path().to_str().unwrap()),
        )],
    );
    assert_ok(
        &honoring_out,
        "gat add with an explicit conflicting override",
    );
    assert!(
        walk_has_any_file(conflicting_cache_dir.path()),
        "expected the explicit conflicting GAT_CACHE_DIR override to actually be honored"
    );

    // Now prove an ordinary spawned `gat add`, with no `extra_env`
    // override at all, still resolves to the deterministic repo-local
    // default -- never the conflicting global config/cache relocation
    // created above, which remains on disk throughout. `conflicting_cache_dir`
    // already contains the file the honoring step above wrote, so record
    // its file count first and assert it is unchanged, rather than
    // asserting it's empty.
    let conflicting_cache_dir_count_before = count_files(conflicting_cache_dir.path());
    let ordinary_repo = init_repo();
    std::fs::write(ordinary_repo.path().join("big.bin"), b"payload").unwrap();
    assert_ok(
        &common::gat(ordinary_repo.path(), &["add", "big.bin"]),
        "ordinary gat add",
    );

    let default_cache = ordinary_repo.path().join(".gat").join("objects");
    assert!(
        walk_has_any_file(&default_cache),
        "expected the ordinary spawned command to use the repo-local default cache: {}",
        default_cache.display()
    );
    assert!(
        !walk_has_any_file(&conflicting_cache_location),
        "ordinary spawned command must not have used the conflicting global cache.location"
    );
    assert_eq!(
        count_files(conflicting_cache_dir.path()),
        conflicting_cache_dir_count_before,
        "ordinary spawned command must not have reused the earlier explicit GAT_CACHE_DIR override"
    );
}

fn walk_has_any_file(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if walk_has_any_file(&path) {
                return true;
            }
        } else if path.is_file() {
            return true;
        }
    }
    false
}

/// Recursively counts regular files under `dir` to detect whether
/// a later spawned command added anything new to a directory that
/// already contained files from an earlier step in the same test.
fn count_files(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            count += count_files(&path);
        } else if path.is_file() {
            count += 1;
        }
    }
    count
}

#[test]
fn remote_template_display_does_not_depend_on_environment() {
    let tmp = init_repo();
    // hygiene-ok: configured template exercised only by display commands; never dialed.
    let template = "azblob://${CONTAINER}/gat?endpoint=https://${STORAGE_ACCOUNT}.blob.core.windows.net&sas_token=${SAS_TOKEN}";
    std::fs::write(
        tmp.path().join("gat.yaml"),
        format!("remotes:\n  default: origin\n  origin:\n    url: '{template}'\n"),
    )
    .unwrap();
    for args in [
        vec!["remote", "show", "origin"],
        vec!["remote", "list"],
        vec!["remote", "list", "--full-output"],
    ] {
        let unset = common::gat_with_env(
            tmp.path(),
            &args,
            &[
                ("CONTAINER", None),
                ("STORAGE_ACCOUNT", None),
                ("SAS_TOKEN", None),
            ],
        );
        let populated = common::gat_with_env(
            tmp.path(),
            &args,
            &[
                ("CONTAINER", Some("resolved-secret-container")),
                ("STORAGE_ACCOUNT", Some("resolved-secret-account")),
                ("SAS_TOKEN", Some("resolved-secret-token")),
            ],
        );
        assert_ok(&unset, "display with unset variables");
        assert_ok(&populated, "display with populated variables");
        assert_eq!(unset.stdout, populated.stdout);
        assert_eq!(unset.stderr, populated.stderr);
        let displayed = String::from_utf8_lossy(&unset.stdout);
        if args == ["remote", "list"] {
            assert!(displayed.contains("azblob://"));
            assert!(displayed.contains("..."));
        } else {
            assert!(displayed.contains(template));
        }
    }
}

#[test]
fn remote_validation_and_opening_never_display_expanded_environment_values() {
    let tmp = init_repo();
    std::fs::write(tmp.path().join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(tmp.path(), &["add", "asset.bin"]), "add tracked asset");
    commit_all(tmp.path(), "track asset");
    let template = "${GAT_TEST_DIAGNOSTIC_SCHEME}://${GAT_TEST_DIAGNOSTIC_HOST}/${GAT_TEST_DIAGNOSTIC_PATH}?token=${GAT_TEST_DIAGNOSTIC_TOKEN}";
    let variables = [
        ("GAT_TEST_DIAGNOSTIC_SCHEME", Some("resolved-secret-scheme")),
        ("GAT_TEST_DIAGNOSTIC_HOST", Some("resolved-secret-host")),
        ("GAT_TEST_DIAGNOSTIC_PATH", Some("resolved-secret-path")),
        ("GAT_TEST_DIAGNOSTIC_TOKEN", Some("resolved-secret-token")),
    ];
    for candidate in [template, "${GAT_TEST_DIAGNOSTIC_URL}"] {
        let mut environment = variables.to_vec();
        environment.push(("GAT_TEST_DIAGNOSTIC_URL", Some("resolved-secret-scheme://resolved-secret-host/resolved-secret-path?token=resolved-secret-token")));
        let validation = common::gat_with_env(
            tmp.path(),
            &["remote", "add", "origin", candidate],
            &environment,
        );
        assert!(!validation.status.success());
        common::assert_no_secret_leak(&validation, &["resolved-secret"]);
        assert!(String::from_utf8_lossy(&validation.stderr).contains(candidate));

        std::fs::write(
            tmp.path().join("gat.yaml"),
            format!("remotes:\n  default: origin\n  origin:\n    url: '{candidate}'\n"),
        )
        .unwrap();
        let opening = common::gat_with_env(tmp.path(), &["status", "--remote"], &environment);
        assert!(!opening.status.success());
        common::assert_no_secret_leak(&opening, &["resolved-secret"]);
        let stderr = String::from_utf8_lossy(&opening.stderr);
        assert!(
            stderr.contains(candidate) || stderr.contains("origin"),
            "{stderr}"
        );
        // Keep the next validation case independent of the persisted fixture.
        std::fs::write(tmp.path().join("gat.yaml"), "{}").unwrap();
    }
}

#[test]
fn file_remote_operations_hide_the_expanded_root() {
    let tmp = init_repo();
    std::fs::write(tmp.path().join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(tmp.path(), &["add", "asset.bin"]), "add tracked asset");
    commit_all(tmp.path(), "track asset");
    let remote = tempfile::tempdir().unwrap();
    let blocker = remote.path().join("RESOLVED-REMOTE-PATH-SECRET");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let url = remote_url(&blocker.join("root"));
    std::fs::write(
        tmp.path().join("gat.yaml"),
        "remotes:\n  default: origin\n  origin:\n    url: '${GAT_TEST_DIAGNOSTIC_URL}'\n",
    )
    .unwrap();
    for args in [vec!["status", "--remote"], vec!["push"]] {
        let output = common::gat_with_env(
            tmp.path(),
            &args,
            &[("GAT_TEST_DIAGNOSTIC_URL", Some(&url))],
        );
        assert!(!output.status.success());
        common::assert_no_secret_leak(&output, &["RESOLVED-REMOTE-PATH-SECRET"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("origin") || stderr.contains("asset.bin"),
            "{stderr}"
        );
    }
}
