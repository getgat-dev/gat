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
fn escaped_paths_roundtrip_in_decoded_order() {
    use gat_core::lock::validated::FilteredRowCursor;
    let paths = [
        "a\tfile", "a\nfile", "a\rfile", "a\"file", "a/file", "aZfile", "é file",
    ];
    let lock = Lock {
        entries: paths.iter().map(|p| entry(p, 'a')).collect(),
    };
    let text = lock.to_string();
    assert!(text.contains(r"a\x09file"));
    assert!(text.contains(r"a\x0afile"));
    assert!(text.contains(r"a\x0dfile"));
    assert!(text.contains("a\"file"));
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
    assert!(Lock::parse(&reversed).is_err());
    assert!(FilteredRowCursor::new(&reversed, |_| true).is_err());
}

#[test]
fn malformed_file_never_invokes_callbacks() {
    use gat_core::lock::validated::visit_rows_validated;
    let oid = oid('a');
    for suffix in [
        format!("{oid}\ta\n"),
        format!("{oid}\ta.b\n{oid}\ta/b\n"),
        format!("{oid}\tz\\x2f\n"),
        format!("{oid}\t0\n"),
    ] {
        let text = format!("{}\n{oid}\ta\n{suffix}", gat_core::lock::VERSION);
        assert!(
            visit_rows_validated(
                &text,
                |_, _| panic!("selection before certification"),
                |_| panic!("emission before certification")
            )
            .is_err()
        );
    }
}

#[test]
fn writer_sorts_paths_and_uses_only_the_new_v1_grammar() {
    let lock = Lock {
        entries: vec![entry("z", 'b'), entry("a\t\"x", 'a')],
    };
    assert_eq!(
        lock.to_string(),
        format!(
            "{}\n{}\ta\\x09\"x\n{}\tz\n",
            gat_core::lock::VERSION,
            oid('a'),
            oid('b')
        )
    );
}
