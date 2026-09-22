use gat_io::{ObjectVerification, RepositoryLayout};

#[test]
fn foreign_ticket_is_verified_against_the_receiving_cache() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let a = RepositoryLayout::at(a.path().to_path_buf()).resolve_cache_root(None);
    let b = RepositoryLayout::at(b.path().to_path_buf()).resolve_cache_root(None);
    let (ingested, _) = a.writer().ingest(&b"present only in A"[..]).unwrap();
    let completed = a
        .open_client()
        .prepare_verification(&[ingested.oid])
        .verify()
        .unwrap();
    let receiver = b.open_client();
    assert_eq!(
        receiver.commit_verification(completed).unwrap(),
        vec![ObjectVerification::Missing]
    );
    assert_eq!(
        receiver.verify(&ingested.oid).unwrap(),
        ObjectVerification::Missing
    );
}

#[test]
fn publication_invalidates_both_pending_and_memo_only_tickets() {
    let temp = tempfile::tempdir().unwrap();
    let root = RepositoryLayout::at(temp.path().to_path_buf()).resolve_cache_root(None);
    let client = root.open_client();
    let bytes = b"newly published";
    let oid = gat_core::oid::Oid::from_bytes(*blake3::hash(bytes).as_bytes());
    let pending = client.prepare_verification(&[oid]).verify().unwrap();
    assert_eq!(client.verify(&oid).unwrap(), ObjectVerification::Missing);
    let known = client.prepare_verification(&[oid]).verify().unwrap();
    let (_, receipt) = root.writer().ingest(&bytes[..]).unwrap();
    client.apply_publications(&[receipt]).unwrap();
    for ticket in [pending, known] {
        assert_eq!(
            client.commit_verification(ticket).unwrap(),
            vec![ObjectVerification::Valid]
        );
    }
    assert_eq!(client.verify(&oid).unwrap(), ObjectVerification::Valid);
}
