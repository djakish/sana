use std::sync::Arc;

use sana::indexer;
use sana::metadata::IndexStatus;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::pinning::PinningController;
use sana::value::{Document, Id, Value};

fn store(dir: &tempfile::TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

fn document(id: u64) -> Document {
    let mut document = Document::new(Id::U64(id));
    document
        .attributes
        .insert("title".into(), Value::String("alpha".into()));
    document
}

#[tokio::test]
async fn metadata_tracks_index_lag_and_pinning() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let namespace = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();

    let fresh = namespace.metadata().await.unwrap();
    assert_eq!(fresh.namespace, "docs");
    assert_eq!(fresh.index.status, IndexStatus::UpToDate);
    assert_eq!(fresh.index.unindexed_bytes, 0);
    assert_eq!(fresh.approx_row_count, 0);
    assert!(fresh.pinning.is_none());

    namespace.upsert(document(1)).await.unwrap();
    let updating = namespace.metadata().await.unwrap();
    assert_eq!(updating.index.status, IndexStatus::Updating);
    assert!(updating.index.unindexed_bytes > 0);
    assert_eq!(updating.index.committed_cursor.seq, 1);

    indexer::flush(&namespace).await.unwrap();
    PinningController::new(object_store)
        .configure("docs", Some(2))
        .await
        .unwrap();
    let indexed = namespace.metadata().await.unwrap();
    assert_eq!(indexed.index.status, IndexStatus::UpToDate);
    assert_eq!(indexed.index.unindexed_bytes, 0);
    assert_eq!(indexed.approx_row_count, 1);
    assert_eq!(indexed.pinning.unwrap().replicas, 2);
}
