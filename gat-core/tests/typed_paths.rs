use gat_core::lexical_path::{GatPath, LexicalPathError};

#[test]
fn gat_path_normalizes_every_supported_user_spelling() {
    for (input, expected) in [
        ("data/a.bin", "data/a.bin"),
        ("./data/a.bin", "data/a.bin"),
        ("data/", "data"),
        ("data//./a.bin", "data/a.bin"),
        ("data\\a.bin", "data/a.bin"),
        ("file with spaces.bin", "file with spaces.bin"),
        ("C:foo", "C:foo"),
        ("C:/foo", "C:/foo"),
        // hygiene-ok: pure string literal exercising host-independent normalization.
        ("C:\\foo", "C:/foo"),
    ] {
        assert_eq!(GatPath::normalize(input).unwrap(), expected, "{input:?}");
    }
}

#[test]
fn gat_path_rejects_escape_root_and_empty_spellings() {
    for input in [
        "../secret",
        "data/../../secret",
        "data\\..\\..\\secret",
        "data/..\\secret",
        "/etc/passwd",
        "\\etc\\passwd",
        "//server/share",
        "\\\\server\\share",
        "",
        ".",
        "./",
    ] {
        assert!(GatPath::normalize(input).is_err(), "{input:?}");
    }
}

#[test]
fn gat_path_accepts_escaped_lock_characters() {
    for input in ["data/fi\tle.bin", "data/fi\nle.bin", "data/fi\rle.bin"] {
        assert_eq!(GatPath::normalize(input).unwrap(), input);
        assert_eq!(GatPath::parse_canonical(input).unwrap(), input);
    }
}

#[test]
fn canonical_parser_rejects_spellings_that_require_normalization() {
    for input in ["./data/a.bin", "data/", "data//a.bin", "data\\a.bin"] {
        assert!(
            GatPath::parse_canonical(input).is_err(),
            "{input:?} must not be silently normalized at a persisted-text boundary"
        );
    }
    assert_eq!(
        GatPath::parse_canonical("data/a.bin").unwrap(),
        "data/a.bin"
    );
}

#[cfg(unix)]
#[test]
fn gat_path_rejects_non_utf8_input() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::Path;

    let path = OsString::from_vec(b"invalid-\xFF-path".to_vec());
    assert!(matches!(
        GatPath::normalize(Path::new(&path)).unwrap_err(),
        LexicalPathError::NonUtf8 { .. }
    ));
}

#[test]
fn relative_paths_preserve_component_boundaries_and_typed_reparenting() {
    use gat_core::lexical_path::GatSubpathRef;
    use gat_core::path_scope::PathScope;
    use gat_core::selection::Selection;

    let prefix = GatPath::parse_canonical("données").unwrap();
    let destination = GatPath::parse_canonical("archive").unwrap();
    let selection = Selection::from_scope_patterns(PathScope::Path(prefix.clone()), vec![], vec![]);
    for (raw, expected) in [
        ("données", Some("")),
        ("données/a/b", Some("a/b")),
        ("données.bin", None),
        ("données-autres/a", None),
        ("autres/données", None),
        ("donné", None),
    ] {
        let path = GatPath::parse_canonical(raw).unwrap();
        let relative = path.strip_prefix(&prefix);
        assert_eq!(
            relative.map(|suffix| match suffix {
                GatSubpathRef::Root => "",
                GatSubpathRef::Path(path) => path.as_str(),
            }),
            expected,
            "{raw}"
        );
        assert_eq!(path.is_or_under(&prefix), expected.is_some());
        assert_eq!(selection.matches_str(raw), expected.is_some());
        assert_eq!(selection.reparent_relative(&path), relative);
        if let Some(relative) = relative {
            let replaced = path.with_replaced_prefix(&prefix, &destination);
            assert_eq!(replaced.strip_prefix(&destination), Some(relative));
        }
        assert_eq!(
            Selection::root().reparent_relative(&path),
            Some(GatSubpathRef::Path(path.as_borrowed()))
        );
    }
}
