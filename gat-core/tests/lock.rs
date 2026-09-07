use gat_core::lexical_path::GatPath;
use gat_core::lock::{Entry, Lock};
use gat_core::oid::Oid;

fn path(value: &str) -> GatPath {
    GatPath::parse_canonical(value).expect("canonical fixture path")
}

fn oid(digit: char) -> Oid {
    Oid::from_hex(&digit.to_string().repeat(64)).expect("valid fixture oid")
}

fn entry(path_value: &str, oid_digit: char) -> Entry {
    Entry {
        path: path(path_value),
        oid: oid(oid_digit),
    }
}

#[test]
fn typed_mutation_roundtrips_through_the_public_codec() {
    let mut lock = Lock::default();
    lock.upsert(path("a.bin"), oid('a'));
    lock.upsert(path("data/b.bin"), oid('b'));

    let encoded = lock.to_string();
    assert!(encoded.ends_with('\n'));
    assert!(!encoded.contains("\r\n"));
    assert_eq!(Lock::parse(&encoded).unwrap(), lock);
}

#[test]
fn typed_upsert_replaces_an_existing_path() {
    let mut lock = Lock::default();
    lock.upsert(path("a.bin"), oid('a'));
    lock.upsert(path("a.bin"), oid('b'));

    assert_eq!(lock.entries, vec![entry("a.bin", 'b')]);
}

#[test]
fn bulk_upsert_preserves_last_write_semantics_without_repeated_scans() {
    let mut lock = Lock {
        entries: vec![entry("existing.bin", 'a')],
    };
    lock.upsert_many([
        entry("new.bin", 'b'),
        entry("existing.bin", 'c'),
        entry("new.bin", 'd'),
    ]);

    assert_eq!(
        lock.entries,
        vec![entry("existing.bin", 'c'), entry("new.bin", 'd')]
    );
}

#[test]
fn prefix_removal_keeps_sibling_prefixes_distinct() {
    let mut lock = Lock {
        entries: vec![
            entry("data/a.bin", 'a'),
            entry("data/nested/b.bin", 'b'),
            entry("data.bin", 'c'),
            entry("other.bin", 'd'),
        ],
    };

    let removed = lock.remove_prefix(&path("data"));

    assert_eq!(removed, vec![path("data/a.bin"), path("data/nested/b.bin")]);
    assert_eq!(
        lock.entries,
        vec![entry("data.bin", 'c'), entry("other.bin", 'd')]
    );
}

#[test]
fn quoted_paths_roundtrip_and_sort_by_decoded_bytes() {
    use gat_core::lock::validated::{FilteredRowCursor, is_path_ordered};
    let paths = [
        "a\tfile", "a\nfile", "a\rfile", "a\"file", "a/file", "aZfile", "é file",
    ];
    let lock = Lock {
        entries: paths.iter().map(|p| entry(p, 'a')).collect(),
    };
    let text = lock.to_string();
    assert_eq!(text.lines().count(), paths.len() + 1);
    assert!(text.contains(r#""a\tfile""#));
    assert!(text.contains(r#""a\nfile""#));
    assert!(text.contains(r#""a\rfile""#));
    assert!(text.contains(r#""a\"file""#));
    assert!(is_path_ordered(&text));
    assert_eq!(Lock::parse(&text).unwrap(), lock);
    let mut cursor = FilteredRowCursor::new(&text, |_| true).unwrap();
    for expected in &lock.entries {
        assert_eq!(cursor.next().unwrap().as_ref(), Some(expected));
    }
    assert!(cursor.next().unwrap().is_none());
    let reversed = format!(
        "{}\n{}\n",
        gat_core::lock::VERSION,
        text.lines()
            .skip(1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(!is_path_ordered(&reversed));
    assert_eq!(Lock::parse(&reversed).unwrap().entries.len(), paths.len());
}

#[test]
fn quoted_path_syntax_is_strict() {
    for field in [
        "plain",
        r#""unterminated"#,
        r#""a"b""#,
        r#""a\x09b""#,
        r#""a\u0009b""#,
        r#""a\/b""#,
        r#""a\\b""#,
        r#""a\"#,
        "\"a\tb\"",
        "\"a\nb\"",
        "\"a\rb\"",
        "\"\"",
    ] {
        let text = format!(
            "{}\n{field}\tblake3:{}\n",
            gat_core::lock::VERSION,
            oid('a')
        );
        assert!(Lock::parse(&text).is_err(), "accepted {field:?}");
    }
}

#[test]
fn escaped_paths_keep_duplicate_and_directory_conflict_validation() {
    for fields in [[r#""a\tb""#, r#""a\tb""#], [r#""a\nb""#, r#""a\nb/child""#]] {
        let text = format!(
            "{}\n{}\tblake3:{}\n{}\tblake3:{}\n",
            gat_core::lock::VERSION,
            fields[0],
            oid('a'),
            fields[1],
            oid('b')
        );
        assert!(Lock::parse(&text).is_err());
        let mut cursor =
            gat_core::lock::validated::FilteredRowCursor::new(&text, |_| false).unwrap();
        assert!(cursor.next().is_err());
    }
}
