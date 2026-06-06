use std::sync::Arc;

use sana::cache_warm::{CacheObjectKind, CacheWarmOptions};
use sana::error::Error;
use sana::indexer;
use sana::namespace::Namespace;
use sana::object_store::{CachingObjectStore, FsObjectStore, ObjectStore};
use sana::query::{ApproxVectorQuery, Query};
use sana::schema::DistanceMetric;
use sana::value::{Document, Id, VectorValue};

fn vector_doc(id: u64, vector: [f32; 2]) -> Document {
    let mut document = Document::new(Id::U64(id));
    document
        .vectors
        .insert("embedding".into(), VectorValue::F32(vector.to_vec()));
    document
}

#[tokio::test]
async fn warm_plan_is_budgeted_and_prioritizes_manifest_and_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
    let namespace = Namespace::create(store, "vectors").await.unwrap();
    namespace.upsert(vector_doc(1, [1.0, 0.0])).await.unwrap();
    namespace.upsert(vector_doc(2, [2.0, 0.0])).await.unwrap();
    indexer::flush(&namespace).await.unwrap();

    let full = namespace.cache_warm_plan(u64::MAX).await.unwrap();
    assert_eq!(full.objects[0].kind, CacheObjectKind::Manifest);
    assert!(
        full.objects
            .iter()
            .any(|object| object.kind == CacheObjectKind::VectorIndex)
    );
    assert!(
        full.objects
            .iter()
            .any(|object| object.kind == CacheObjectKind::Rabitq)
    );
    assert!(
        full.objects
            .iter()
            .any(|object| object.kind == CacheObjectKind::DocumentSst)
    );

    let manifest_budget = full.objects[0].size_bytes;
    let limited = namespace.cache_warm_plan(manifest_budget).await.unwrap();
    assert_eq!(limited.objects.len(), 1);
    assert_eq!(limited.objects[0].kind, CacheObjectKind::Manifest);
    assert!(limited.skipped_objects > 0);
    assert!(limited.planned_bytes <= manifest_budget);
}

#[tokio::test]
async fn warmed_generation_serves_ann_after_backing_objects_are_removed() {
    let dir = tempfile::tempdir().unwrap();
    let backing: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
    let cache = Arc::new(CachingObjectStore::new(backing.clone(), 64 * 1024 * 1024));
    let store: Arc<dyn ObjectStore> = cache.clone();
    let namespace = Namespace::create(store, "vectors").await.unwrap();
    namespace.upsert(vector_doc(1, [1.0, 0.0])).await.unwrap();
    namespace.upsert(vector_doc(2, [2.0, 0.0])).await.unwrap();
    namespace.upsert(vector_doc(3, [5.0, 0.0])).await.unwrap();
    indexer::flush(&namespace).await.unwrap();
    cache.clear().await;

    let report = namespace
        .hint_cache_warm(CacheWarmOptions {
            max_bytes: 64 * 1024 * 1024,
            max_concurrency: 4,
        })
        .await
        .unwrap();
    assert_eq!(report.loaded_objects, report.plan.objects.len());
    assert_eq!(report.loaded_bytes, report.plan.planned_bytes);
    assert_eq!(report.plan.skipped_objects, 0);

    for object in &report.plan.objects {
        backing.delete(&object.key).await.unwrap();
    }
    let stats_before_query = cache.stats().await;

    let mut query = Query::all();
    query.approx_vector = Some(ApproxVectorQuery {
        column: "embedding".into(),
        vector: vec![1.1, 0.0],
        k: 2,
        probes: Some(8),
        metric: Some(DistanceMetric::L2),
    });
    let result = namespace.query(query).await.unwrap();
    let ids: Vec<Id> = result.rows.into_iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![Id::U64(1), Id::U64(2)]);

    let stats_after_query = cache.stats().await;
    assert!(stats_after_query.hits > stats_before_query.hits);
    assert_eq!(
        stats_after_query.resident_bytes as u64,
        report.plan.planned_bytes
    );
}

#[tokio::test]
async fn warm_rejects_invalid_concurrency_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
    let namespace = Namespace::create(store, "vectors").await.unwrap();

    for max_concurrency in [0, usize::MAX] {
        let error = namespace
            .hint_cache_warm(CacheWarmOptions {
                max_bytes: 1,
                max_concurrency,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidQuery(_)));
    }
}
