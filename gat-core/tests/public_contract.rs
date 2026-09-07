use gat_core::globs::{GatGlobPattern, GlobBound};
use gat_core::lock::{LockShardId, LockShardLevels};
use gat_core::oid::Oid;
use gat_core::progress::{ActivityBackend, ProgressActivity, ProgressTask};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn glob_contract_exposes_borrowed_query_bounds_and_compiled_matching() {
    let pattern = GatGlobPattern::parse(r".\data\model-?.bin").unwrap();

    assert_eq!(pattern.as_str(), "data/model-?.bin");
    assert_eq!(pattern.bound(), GlobBound::Prefix("data/model-"));
    assert!(pattern.matches("data/model-a.bin"));
    assert!(!pattern.matches("data/nested/model-a.bin"));
}

#[test]
fn validated_lock_rows_hash_directly_to_the_typed_shard_identity() {
    let levels = LockShardLevels::new(2).unwrap();
    let path = gat_core::lexical_path::GatPath::parse_canonical("data/model.bin").unwrap();

    assert_eq!(
        gat_core::lock::validated::shard_id_for_path(path.as_str(), levels),
        LockShardId::for_path(&path, levels)
    );
}

#[test]
fn oid_text_and_serde_contract_is_canonical_and_strict() {
    for bytes in [
        [0; 32],
        [0xff; 32],
        std::array::from_fn(|index| (index).to_le_bytes()[0]),
    ] {
        let oid = Oid::from_bytes(bytes);
        let text = oid.to_hex();

        assert_eq!(Oid::from_hex(&text).unwrap(), oid);
        assert_eq!(serde_json::to_string(&oid).unwrap(), format!("\"{text}\""));
        assert_eq!(
            serde_json::from_str::<Oid>(&format!("\"{text}\"")).unwrap(),
            oid
        );
    }

    for malformed in [
        "0".repeat(63),
        "0".repeat(65),
        "A".repeat(64),
        "g".repeat(64),
        format!("{}é0", "0".repeat(61)),
    ] {
        assert!(Oid::from_hex(&malformed).is_err());
        assert!(serde_json::from_str::<Oid>(&format!("\"{malformed}\"")).is_err());
    }
}

#[derive(Default)]
struct ExternalBackend {
    increments: AtomicU64,
}

impl ActivityBackend for ExternalBackend {
    fn inc(&self, delta: u64) {
        self.increments.fetch_add(delta, Ordering::SeqCst);
    }

    fn set_activity(&self, _activity: &ProgressActivity) {}

    fn finish(&self) {}
}

#[test]
fn external_renderers_can_implement_the_public_progress_protocol() {
    let backend = Arc::new(ExternalBackend::default());
    let task = ProgressTask::from_backend(backend.clone());

    task.inc(3);
    task.finish();

    assert_eq!(backend.increments.load(Ordering::SeqCst), 3);
}
