#![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use sana::indexer;
use sana::maintenance::{MaintenancePolicy, MaintenanceState, run_once};
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::query::Query;
use sana::value::{Document, Id, Value};
use tempfile::TempDir;

fn store(dir: &TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

fn doc(id: u64, bucket: i64) -> Document {
    let mut doc = Document::new(Id::U64(id));
    doc.attributes.insert("bucket".into(), Value::Int(bucket));
    doc
}

async fn flushed_runs(ns: &Namespace, docs: u64) {
    for i in 0..docs {
        ns.upsert(doc(i, i as i64)).await.unwrap();
        assert!(indexer::flush(ns).await.unwrap());
    }
}

#[tokio::test]
async fn compaction_triggers_at_run_threshold_and_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let ns = Namespace::create(store.clone(), "docs").await.unwrap();
    flushed_runs(&ns, 3).await;
    assert!(ns.load_manifest().await.unwrap().doc_ssts.len() >= 3);

    let policy = MaintenancePolicy {
        compact_at_runs: 3,
        ..MaintenancePolicy::default()
    };
    let mut state = MaintenanceState::default();
    let report = run_once(store.clone(), &policy, &mut state).await.unwrap();
    assert_eq!(report.compacted, ["docs"]);
    assert!(report.errors.is_empty());

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.doc_ssts.len(), 1);
    assert_eq!(ns.query(Query::all()).await.unwrap().rows.len(), 3);
}

#[tokio::test]
async fn unindexed_namespaces_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let ns = Namespace::create(store.clone(), "docs").await.unwrap();
    flushed_runs(&ns, 3).await;
    // One committed-but-unindexed write: the flush worker owns catching up.
    ns.upsert(doc(99, 99)).await.unwrap();
    let runs_before = ns.load_manifest().await.unwrap().doc_ssts.len();

    let policy = MaintenancePolicy {
        compact_at_runs: 3,
        ..MaintenancePolicy::default()
    };
    let mut state = MaintenanceState::default();
    let report = run_once(store, &policy, &mut state).await.unwrap();
    assert!(report.compacted.is_empty());
    assert!(report.errors.is_empty());
    assert_eq!(
        ns.load_manifest().await.unwrap().doc_ssts.len(),
        runs_before
    );
}

#[tokio::test]
async fn default_policy_does_not_reclaim_or_remember_gc_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let ns = Namespace::create(store.clone(), "docs").await.unwrap();
    flushed_runs(&ns, 3).await;

    let policy = MaintenancePolicy {
        compact_at_runs: 3,
        ..MaintenancePolicy::default()
    };
    let mut state = MaintenanceState::default();

    let first = run_once(store.clone(), &policy, &mut state).await.unwrap();
    assert_eq!(first.compacted, ["docs"]);
    assert_eq!(first.gc_deleted_objects, 0);
    assert_eq!(first.gc_pending_objects, 0);

    let orphans = indexer::gc(&ns, false).await.unwrap().orphan_keys;
    assert!(!orphans.is_empty());

    let second = run_once(store.clone(), &policy, &mut state).await.unwrap();
    assert_eq!(second.gc_deleted_objects, 0);
    assert_eq!(second.gc_pending_objects, 0);
    for key in &orphans {
        store
            .get(key)
            .await
            .expect("default maintenance leaves GC to operators");
    }
}

#[tokio::test]
async fn gc_deletes_orphans_only_after_two_consecutive_passes() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let ns = Namespace::create(store.clone(), "docs").await.unwrap();
    flushed_runs(&ns, 3).await;

    let policy = MaintenancePolicy {
        compact_at_runs: 3,
        gc: true,
        ..MaintenancePolicy::default()
    };
    let mut state = MaintenanceState::default();

    // Pass 1 compacts, then its GC scan sees the superseded runs as fresh
    // orphans: remembered, not deleted.
    let first = run_once(store.clone(), &policy, &mut state).await.unwrap();
    assert_eq!(first.compacted, ["docs"]);
    assert_eq!(first.gc_deleted_objects, 0);
    assert!(first.gc_pending_objects > 0);

    let orphans = indexer::gc(&ns, false).await.unwrap().orphan_keys;
    assert!(!orphans.is_empty());
    for key in &orphans {
        store
            .get(key)
            .await
            .expect("orphan survives the first pass");
    }

    // Pass 2 agrees and reclaims them; the namespace stays readable.
    let second = run_once(store.clone(), &policy, &mut state).await.unwrap();
    assert!(second.gc_deleted_objects >= orphans.len());
    assert!(
        indexer::gc(&ns, false)
            .await
            .unwrap()
            .orphan_keys
            .is_empty()
    );
    assert_eq!(ns.query(Query::all()).await.unwrap().rows.len(), 3);
}
