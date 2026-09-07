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
