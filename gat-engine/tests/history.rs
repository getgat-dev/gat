use gat_core::history::{HistoryRoot, HistorySelection, HistoryTraversal};
use gat_core::lexical_path::GatPath;
use gat_core::lock::Lock;
use gat_core::oid::Oid;

#[test]
fn history_visitors_preserve_callback_errors_and_stop_immediately() {
    let temp = test_support_git::empty_git_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(temp.path().to_path_buf());
    let mut lock = Lock::default();
    for path in ["a.bin", "b.bin"] {
        lock.upsert(
            GatPath::parse_canonical(path).unwrap(),
            Oid::from_hex(&"1".repeat(64)).unwrap(),
        );
    }
    repo.save_lock(&lock).unwrap();
    test_support_git::commit_all(temp.path(), "tracked files");
    let selection = HistorySelection {
        roots: vec![HistoryRoot::Head],
        traversal: HistoryTraversal::Tips,
        ..HistorySelection::default()
    };
    for lock_entries in [false, true] {
        let error = Box::new(std::io::Error::other("callback stopped"));
        let original = std::ptr::from_ref(error.as_ref());
        let mut error: Option<Box<dyn std::error::Error>> = Some(error);
        let mut calls = 0;
        let mut stop = || {
            calls += 1;
            Err(error.take().unwrap())
        };
        let result = if lock_entries {
            repo.visit_history_lock_entries(&selection, |_| true, |_| stop())
        } else {
            repo.visit_history_commits(&selection, |_| stop())
        };
        let returned = result.unwrap_err();
        assert_eq!(calls, 1);
        assert!(std::ptr::eq(
            original,
            returned.downcast_ref::<std::io::Error>().unwrap()
        ));
    }
}
