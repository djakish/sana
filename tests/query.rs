use std::sync::Arc;

use sana::error::Error;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::query::{
    Aggregate, AggregateResult, ApproxVectorQuery, ExactVectorQuery, FilterExpr, OrderBy,
    OrderTarget, Query, RangeBound, RecallRequest, SortDirection,
};
use sana::schema::DistanceMetric;
use sana::sst::SstReader;
use sana::value::{Document, Id, Value, VectorValue};
use sana::vector;
use sana::{attr, indexer};
use tempfile::TempDir;

fn store(dir: &TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

fn doc(id: u64, title: &str, score: i64, tags: &[&str], vector: [f32; 2]) -> Document {
    let mut doc = Document::new(Id::U64(id));
    doc.attributes
        .insert("title".into(), Value::String(title.into()));
    doc.attributes.insert("score".into(), Value::Int(score));
    doc.attributes.insert(
        "tags".into(),
        Value::Array(
            tags.iter()
                .map(|tag| Value::String((*tag).to_string()))
                .collect(),
        ),
    );
    doc.vectors
        .insert("embedding".into(), VectorValue::F32(vector.to_vec()));
    doc
}

#[tokio::test]
async fn query_filters_orders_and_aggregates() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["search", "rust"], [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "beta", 25, &["search"], [2.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(3, "gamma", 5, &["cold"], [10.0, 0.0]))
        .await
        .unwrap();

    let result = ns
        .query(Query {
            filter: Some(FilterExpr::And(vec![
                FilterExpr::Eq {
                    column: "tags".into(),
                    value: Value::String("search".into()),
                },
                FilterExpr::Range {
                    column: "score".into(),
                    lower: Some(RangeBound::Included(Value::Int(10))),
                    upper: Some(RangeBound::Excluded(Value::Int(30))),
                },
            ])),
            order_by: Some(OrderBy {
                target: OrderTarget::Attribute("score".into()),
                direction: SortDirection::Desc,
            }),
            limit: None,
            aggregates: vec![
                Aggregate::Count,
                Aggregate::Sum {
                    column: "score".into(),
                },
            ],
            exact_vector: None,
            approx_vector: None,
        })
        .await
        .unwrap();

    let ids: Vec<Id> = result.rows.into_iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![Id::U64(2), Id::U64(1)]);
    assert_eq!(
        result.aggregates,
        vec![
            AggregateResult::Count(2),
            AggregateResult::Sum {
                column: "score".into(),
                value_count: 2,
                total: 35.0,
            }
        ]
    );
}

#[tokio::test]
async fn query_supports_or_not_and_primary_key_order() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["search"], [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "beta", 25, &["search"], [2.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(3, "gamma", 5, &["cold"], [10.0, 0.0]))
        .await
        .unwrap();

    let result = ns
        .query(Query {
            filter: Some(FilterExpr::Or(vec![
                FilterExpr::Eq {
                    column: "title".into(),
                    value: Value::String("alpha".into()),
                },
                FilterExpr::Not(Box::new(FilterExpr::Eq {
                    column: "tags".into(),
                    value: Value::String("search".into()),
                })),
            ])),
            order_by: Some(OrderBy {
                target: OrderTarget::Id,
                direction: SortDirection::Desc,
            }),
            limit: Some(1),
            aggregates: vec![Aggregate::Count],
            exact_vector: None,
            approx_vector: None,
        })
        .await
        .unwrap();

    let ids: Vec<Id> = result.rows.into_iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![Id::U64(3)]);
    assert_eq!(result.aggregates, vec![AggregateResult::Count(2)]);
}

#[tokio::test]
async fn exact_vector_knn_scores_filtered_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["keep"], [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "beta", 20, &["keep"], [2.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(3, "gamma", 30, &["drop"], [0.1, 0.0]))
        .await
        .unwrap();

    let result = ns
        .query(Query {
            filter: Some(FilterExpr::Eq {
                column: "tags".into(),
                value: Value::String("keep".into()),
            }),
            order_by: None,
            limit: None,
            aggregates: vec![Aggregate::Count],
            exact_vector: Some(ExactVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 2,
                metric: Some(DistanceMetric::L2),
            }),
            approx_vector: None,
        })
        .await
        .unwrap();

    assert_eq!(result.aggregates, vec![AggregateResult::Count(2)]);
    let ids: Vec<Id> = result.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(1), Id::U64(2)]);
    assert_eq!(result.rows[0].score, Some(-1.0));
    assert_eq!(result.rows[1].score, Some(-4.0));
}

#[tokio::test]
async fn exact_vector_query_validates_schema_and_dimension() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["keep"], [1.0, 0.0]))
        .await
        .unwrap();

    let err = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: Some(ExactVectorQuery {
                column: "embedding".into(),
                vector: vec![1.0, 2.0, 3.0],
                k: 1,
                metric: None,
            }),
            approx_vector: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(err, Error::InvalidQuery(_)));
}

#[tokio::test]
async fn indexed_attribute_filter_rechecks_wal_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    ns.upsert(doc(1, "alpha", 10, &["search"], [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "beta", 20, &["cold"], [2.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.attr_ssts.len(), 1);
    let attr_reader = SstReader::open(
        object_store
            .get(&manifest.attr_ssts[0].key)
            .await
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(
        attr::ids_for_eq(&attr_reader, "tags", &Value::String("search".into()))
            .unwrap()
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![Id::U64(1)]
    );

    ns.delete(Id::U64(1)).await.unwrap();
    ns.upsert(doc(3, "gamma", 15, &["search"], [3.0, 0.0]))
        .await
        .unwrap();

    let result = ns
        .query(Query {
            filter: Some(FilterExpr::Eq {
                column: "tags".into(),
                value: Value::String("search".into()),
            }),
            order_by: Some(OrderBy {
                target: OrderTarget::Id,
                direction: SortDirection::Asc,
            }),
            limit: None,
            aggregates: vec![Aggregate::Count],
            exact_vector: None,
            approx_vector: None,
        })
        .await
        .unwrap();

    assert_eq!(result.aggregates, vec![AggregateResult::Count(1)]);
    let ids: Vec<Id> = result.rows.into_iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![Id::U64(3)]);
}

#[tokio::test]
async fn indexed_numeric_eq_matches_cross_type_query_value() {
    // `score` is inferred Int; after a flush the postings live only in the attr
    // SST. A Float query value that is numerically equal must still match, exactly
    // as the unindexed full-scan path does — the candidate generation may not be
    // type-strict where the recheck coerces.
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["search"], [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "beta", 25, &["search"], [2.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let result = ns
        .query(Query {
            filter: Some(FilterExpr::Eq {
                column: "score".into(),
                value: Value::Float(10.0),
            }),
            order_by: None,
            limit: None,
            aggregates: vec![Aggregate::Count],
            exact_vector: None,
            approx_vector: None,
        })
        .await
        .unwrap();

    let ids: Vec<Id> = result.rows.into_iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![Id::U64(1)]);
}

#[tokio::test]
async fn approx_vector_query_honors_limit_below_k() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    for (id, x) in [(1, 0.1), (2, 0.2), (3, 1.0), (4, 2.0)] {
        ns.upsert(doc(id, &format!("doc-{id}"), id as i64, &["v"], [x, 0.0]))
            .await
            .unwrap();
    }
    indexer::flush(&ns).await.unwrap();

    let ann = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: Some(2),
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 4,
                probes: Some(16),
                metric: Some(DistanceMetric::L2),
            }),
        })
        .await
        .unwrap();

    let ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(1), Id::U64(2)]);
}

#[tokio::test]
async fn ann_vector_query_matches_exact_with_full_probes() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    for (id, x) in [(1, 0.1), (2, 0.2), (3, 1.0), (4, 2.0), (5, 4.0), (6, 8.0)] {
        ns.upsert(doc(id, &format!("doc-{id}"), id as i64, &["v"], [x, 0.0]))
            .await
            .unwrap();
    }
    indexer::flush(&ns).await.unwrap();
    assert!(
        ns.load_manifest()
            .await
            .unwrap()
            .vector_indexes
            .contains_key("embedding")
    );

    let exact = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: Some(ExactVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 3,
                metric: Some(DistanceMetric::L2),
            }),
            approx_vector: None,
        })
        .await
        .unwrap();
    let ann = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 3,
                probes: Some(16),
                metric: Some(DistanceMetric::L2),
            }),
        })
        .await
        .unwrap();

    let exact_ids: Vec<Id> = exact.rows.iter().map(|row| row.id.clone()).collect();
    let ann_ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(vector::recall_at(&exact_ids, &ann_ids, 3), 1.0);
    assert_eq!(ann_ids, exact_ids);
}

#[tokio::test]
async fn ann_vector_query_rechecks_wal_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["v"], [0.1, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "beta", 20, &["v"], [10.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.delete(Id::U64(1)).await.unwrap();
    ns.upsert(doc(3, "gamma", 30, &["v"], [0.05, 0.0]))
        .await
        .unwrap();

    let ann = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 2,
                probes: Some(16),
                metric: Some(DistanceMetric::L2),
            }),
        })
        .await
        .unwrap();

    let ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(3), Id::U64(2)]);
    assert_eq!(ann.rows[0].score, Some(-0.0025000002));
}

#[tokio::test]
async fn ann_vector_query_uses_native_filtering_to_probe_matching_clusters() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    for (id, color, x) in [
        (1, "blue", 0.1),
        (2, "blue", 0.2),
        (3, "blue", 0.3),
        (4, "red", 10.0),
        (5, "red", 10.1),
        (6, "green", 20.0),
    ] {
        ns.upsert(doc(
            id,
            &format!("{color}-{id}"),
            id as i64,
            &[color],
            [x, 0.0],
        ))
        .await
        .unwrap();
    }
    indexer::flush(&ns).await.unwrap();

    let ann = ns
        .query(Query {
            filter: Some(FilterExpr::Eq {
                column: "tags".into(),
                value: Value::String("red".into()),
            }),
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 2,
                probes: Some(1),
                metric: Some(DistanceMetric::L2),
            }),
        })
        .await
        .unwrap();

    let ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(4), Id::U64(5)]);
}

#[tokio::test]
async fn ann_vector_query_applies_filters_to_wal_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "red-old", 10, &["red"], [10.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "blue", 20, &["blue"], [0.1, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.delete(Id::U64(1)).await.unwrap();
    ns.upsert(doc(3, "red-new", 30, &["red"], [0.05, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(4, "blue-new", 40, &["blue"], [0.01, 0.0]))
        .await
        .unwrap();

    let ann = ns
        .query(Query {
            filter: Some(FilterExpr::Eq {
                column: "tags".into(),
                value: Value::String("red".into()),
            }),
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 2,
                probes: Some(16),
                metric: Some(DistanceMetric::L2),
            }),
        })
        .await
        .unwrap();

    let ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(3)]);
}

#[tokio::test]
async fn recall_reports_perfect_recall_with_full_probes() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    for (id, x) in [(1, 0.1), (2, 0.2), (3, 1.0), (4, 2.0), (5, 4.0), (6, 8.0)] {
        ns.upsert(doc(id, &format!("doc-{id}"), id as i64, &["v"], [x, 0.0]))
            .await
            .unwrap();
    }
    indexer::flush(&ns).await.unwrap();

    let result = ns
        .recall(RecallRequest {
            num: 4,
            top_k: 3,
            column: Some("embedding".into()),
            probes: Some(16),
            metric: Some(DistanceMetric::L2),
            filter: None,
        })
        .await
        .unwrap();

    assert_eq!(result.column, "embedding");
    assert_eq!(result.requested, 4);
    assert_eq!(result.sampled, 4);
    assert_eq!(result.top_k, 3);
    assert_eq!(result.avg_recall, 1.0);
    assert_eq!(result.avg_exhaustive_count, 3.0);
    assert_eq!(result.avg_ann_count, 3.0);
    for sample in &result.samples {
        assert_eq!(sample.recall, 1.0);
        assert_eq!(sample.exhaustive_ids, sample.ann_ids);
    }
}

#[tokio::test]
async fn recall_rechecks_wal_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["v"], [0.1, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "beta", 20, &["v"], [10.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.delete(Id::U64(1)).await.unwrap();
    ns.upsert(doc(3, "gamma", 30, &["v"], [0.05, 0.0]))
        .await
        .unwrap();

    let result = ns
        .recall(RecallRequest {
            num: 2,
            top_k: 2,
            column: Some("embedding".into()),
            probes: Some(16),
            metric: Some(DistanceMetric::L2),
            filter: None,
        })
        .await
        .unwrap();

    assert_eq!(result.sampled, 2);
    assert_eq!(result.avg_recall, 1.0);
    for sample in &result.samples {
        assert_eq!(sample.exhaustive_ids, sample.ann_ids);
        assert!(!sample.ann_ids.contains(&Id::U64(1)));
    }
}

#[tokio::test]
async fn recall_supports_native_filtered_ann() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    for (id, color, x) in [
        (1, "blue", 0.1),
        (2, "blue", 0.2),
        (3, "red", 10.0),
        (4, "red", 10.1),
        (5, "green", 20.0),
    ] {
        ns.upsert(doc(
            id,
            &format!("{color}-{id}"),
            id as i64,
            &[color],
            [x, 0.0],
        ))
        .await
        .unwrap();
    }
    indexer::flush(&ns).await.unwrap();

    let result = ns
        .recall(RecallRequest {
            num: 4,
            top_k: 2,
            column: Some("embedding".into()),
            probes: Some(16),
            metric: Some(DistanceMetric::L2),
            filter: Some(FilterExpr::Eq {
                column: "tags".into(),
                value: Value::String("red".into()),
            }),
        })
        .await
        .unwrap();

    assert_eq!(result.sampled, 2);
    assert_eq!(result.avg_recall, 1.0);
    assert_eq!(result.avg_exhaustive_count, 2.0);
    assert_eq!(result.avg_ann_count, 2.0);
    for sample in &result.samples {
        assert_eq!(sample.exhaustive_ids, sample.ann_ids);
        assert_eq!(sample.exhaustive_count, 2);
    }
}

#[tokio::test]
async fn recall_requires_published_vector_index() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["v"], [0.1, 0.0]))
        .await
        .unwrap();

    let err = ns
        .recall(RecallRequest {
            num: 1,
            top_k: 1,
            column: Some("embedding".into()),
            probes: None,
            metric: Some(DistanceMetric::L2),
            filter: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(err, Error::InvalidQuery(_)));
}
