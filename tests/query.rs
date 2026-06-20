#![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use async_trait::async_trait;
use bytes::Bytes;
use sana::error::Error;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, GetResult, ObjectMeta, ObjectStore, ObjectVersion};
use sana::query::{
    Aggregate, AggregateResult, ApproxVectorQuery, ExactVectorQuery, FilterExpr, MAX_QUERY_RESULTS,
    MultiQuery, OrderBy, OrderTarget, Query, QueryOptions, RangeBound, RecallRequest,
    SortDirection, TextQuery,
};
use sana::schema::DistanceMetric;
use sana::sst::SstReader;
use sana::text::Bm25Params;
use sana::value::{Document, Id, Value, VectorValue};
use sana::vector;
use sana::wal::WalOp;
use sana::{attr, indexer};
use tempfile::TempDir;

fn store(dir: &TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

/// Test-only `ObjectStore` decorator that counts object reads (`get`/`get_range`)
/// so a test can assert the query path's round-trip count, not just its output.
struct CountingStore {
    inner: Arc<dyn ObjectStore>,
    gets: AtomicUsize,
}

impl CountingStore {
    fn new(inner: Arc<dyn ObjectStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            gets: AtomicUsize::new(0),
        })
    }

    fn gets(&self) -> usize {
        self.gets.load(AtomicOrdering::Relaxed)
    }

    fn reset(&self) {
        self.gets.store(0, AtomicOrdering::Relaxed);
    }
}

#[async_trait]
impl ObjectStore for CountingStore {
    async fn get(&self, key: &str) -> sana::Result<GetResult> {
        self.gets.fetch_add(1, AtomicOrdering::Relaxed);
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> sana::Result<Bytes> {
        self.gets.fetch_add(1, AtomicOrdering::Relaxed);
        self.inner.get_range(key, range).await
    }

    async fn put(&self, key: &str, bytes: Bytes) -> sana::Result<ObjectVersion> {
        self.inner.put(key, bytes).await
    }

    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> sana::Result<ObjectVersion> {
        self.inner.put_if_absent(key, bytes).await
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> sana::Result<ObjectVersion> {
        self.inner.compare_and_set(key, expected, bytes).await
    }

    async fn list(&self, prefix: &str) -> sana::Result<Vec<ObjectMeta>> {
        self.inner.list(prefix).await
    }

    async fn delete(&self, key: &str) -> sana::Result<()> {
        self.inner.delete(key).await
    }
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
            text: None,
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
async fn strong_queries_reject_oversized_overlay_until_flush() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "alpha", 10, &["search"], [1.0, 0.0]))
        .await
        .unwrap();
    let options = QueryOptions {
        max_unindexed_wal_bytes: 0,
    };

    let error = ns
        .query_with_options(Query::all(), options)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Backpressure { limit_bytes: 0, .. }));
    let error = ns
        .multi_query_with_options(
            MultiQuery {
                queries: vec![Query::all(), Query::all()],
            },
            options,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Backpressure { .. }));

    indexer::flush(&ns).await.unwrap();
    assert_eq!(
        ns.query_with_options(Query::all(), options)
            .await
            .unwrap()
            .rows
            .len(),
        1
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
            text: None,
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
            text: None,
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
            text: None,
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
            text: None,
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
            text: None,
        })
        .await
        .unwrap();

    let ids: Vec<Id> = result.rows.into_iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![Id::U64(1)]);
}

#[tokio::test]
async fn bm25_text_query_reads_flushed_text_sst() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "rust rust database", 10, &["public"], [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(
        2,
        "rust database storage engine",
        20,
        &["public"],
        [2.0, 0.0],
    ))
    .await
    .unwrap();
    ns.upsert(doc(3, "python database", 30, &["public"], [3.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.text_ssts.len(), 1);

    let result = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: None,
            aggregates: vec![Aggregate::Count],
            exact_vector: None,
            approx_vector: None,
            text: Some(TextQuery {
                column: "title".into(),
                query: "rust database".into(),
                k: 3,
                params: Bm25Params::default(),
            }),
        })
        .await
        .unwrap();

    assert_eq!(result.aggregates, vec![AggregateResult::Count(3)]);
    let ids: Vec<Id> = result.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(1), Id::U64(2), Id::U64(3)]);
    assert!(result.rows[0].score.unwrap() > result.rows[1].score.unwrap());
}

#[tokio::test]
async fn bm25_text_query_filters_and_falls_back_to_wal_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "rust database", 10, &["public"], [1.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.upsert(doc(
        2,
        "rust database internals",
        20,
        &["private"],
        [2.0, 0.0],
    ))
    .await
    .unwrap();

    let result = ns
        .query(Query {
            filter: Some(FilterExpr::Eq {
                column: "tags".into(),
                value: Value::String("private".into()),
            }),
            order_by: None,
            limit: None,
            aggregates: vec![Aggregate::Count],
            exact_vector: None,
            approx_vector: None,
            text: Some(TextQuery {
                column: "title".into(),
                query: "database internals".into(),
                k: 5,
                params: Bm25Params::default(),
            }),
        })
        .await
        .unwrap();

    assert_eq!(result.aggregates, vec![AggregateResult::Count(1)]);
    let ids: Vec<Id> = result.rows.into_iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![Id::U64(2)]);
}

#[tokio::test]
async fn multi_query_batches_text_and_vector_results() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(
        1,
        "rust database internals",
        10,
        &["public"],
        [0.05, 0.0],
    ))
    .await
    .unwrap();
    ns.upsert(doc(2, "python database", 20, &["public"], [5.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(3, "rust search engine", 30, &["public"], [0.2, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(4, "cold storage", 40, &["public"], [10.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let result = ns
        .multi_query(MultiQuery {
            queries: vec![
                Query {
                    filter: None,
                    order_by: None,
                    limit: None,
                    aggregates: Vec::new(),
                    exact_vector: None,
                    approx_vector: None,
                    text: Some(TextQuery {
                        column: "title".into(),
                        query: "rust database".into(),
                        k: 3,
                        params: Bm25Params::default(),
                    }),
                },
                Query {
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
                    text: None,
                },
            ],
        })
        .await
        .unwrap();

    assert_eq!(result.results.len(), 2);
    assert_eq!(result.results[0].rows[0].id, Id::U64(1));
    let vector_ids: Vec<Id> = result.results[1]
        .rows
        .iter()
        .map(|row| row.id.clone())
        .collect();
    assert_eq!(vector_ids, vec![Id::U64(1), Id::U64(3)]);
}

#[tokio::test]
async fn multi_query_rejects_empty_batches() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    let err = ns
        .multi_query(MultiQuery {
            queries: Vec::new(),
        })
        .await
        .unwrap_err();

    assert!(matches!(err, Error::InvalidQuery(_)));
}

#[tokio::test]
async fn query_cardinality_limits_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    let error = ns
        .multi_query(MultiQuery {
            queries: vec![Query::all(); 17],
        })
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidQuery(_)));

    let mut query = Query::all();
    query.limit = Some(10_001);
    let error = ns.query(query).await.unwrap_err();
    assert!(matches!(error, Error::InvalidQuery(_)));

    let mut text = Query::all();
    text.text = Some(TextQuery {
        column: "body".into(),
        query: "x".repeat(1_025),
        k: 1,
        params: Bm25Params::default(),
    });
    let error = ns.query(text).await.unwrap_err();
    assert!(matches!(error, Error::InvalidQuery(_)));
}

#[tokio::test]
async fn omitted_limit_defaults_to_max_results_and_aggregates_count_all_matches() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    let total = MAX_QUERY_RESULTS + 3;
    let mut operations = Vec::with_capacity(total);
    for id in 0..total {
        let mut document = Document::new(Id::U64(id as u64));
        document
            .attributes
            .insert("group".into(), Value::String("all".into()));
        operations.push(WalOp::Upsert {
            id: document.id.clone(),
            document,
        });
    }
    ns.append(operations, None).await.unwrap();

    let mut query = Query::all();
    query.aggregates = vec![Aggregate::Count];
    let result = ns.query(query.clone()).await.unwrap();
    assert_eq!(result.rows.len(), MAX_QUERY_RESULTS);
    assert_eq!(
        result.aggregates,
        vec![AggregateResult::Count(total as u64)]
    );

    let multi = ns
        .multi_query(MultiQuery {
            queries: vec![query.clone(), query],
        })
        .await
        .unwrap();
    assert_eq!(multi.results.len(), 2);
    for result in multi.results {
        assert_eq!(result.rows.len(), MAX_QUERY_RESULTS);
        assert_eq!(
            result.aggregates,
            vec![AggregateResult::Count(total as u64)]
        );
    }
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
            text: None,
        })
        .await
        .unwrap();

    let ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(1), Id::U64(2)]);

    let empty = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: Some(0),
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 4,
                probes: Some(16),
                metric: Some(DistanceMetric::L2),
            }),
            text: None,
        })
        .await
        .unwrap();
    assert!(empty.rows.is_empty());
}

#[tokio::test]
async fn filtered_query_resolution_does_not_scale_with_candidate_count() {
    let dir = tempfile::tempdir().unwrap();
    let counting = CountingStore::new(store(&dir));
    let ns = Namespace::create(counting.clone(), "docs").await.unwrap();
    let n: u64 = 20;
    for id in 1..=n {
        ns.upsert(doc(
            id,
            &format!("doc-{id}"),
            id as i64,
            &["keep"],
            [id as f32, 0.0],
        ))
        .await
        .unwrap();
    }
    indexer::flush(&ns).await.unwrap();

    // Measure only the query: candidate resolution should read each SST once,
    // not run a full lookup (manifest + cursor + SSTs + overlay) per matched id.
    counting.reset();
    let result = ns
        .query(Query {
            filter: Some(FilterExpr::Eq {
                column: "tags".into(),
                value: Value::String("keep".into()),
            }),
            order_by: None,
            limit: None,
            aggregates: vec![Aggregate::Count],
            exact_vector: None,
            approx_vector: None,
            text: None,
        })
        .await
        .unwrap();

    assert_eq!(result.rows.len(), n as usize);
    assert_eq!(result.aggregates, vec![AggregateResult::Count(n)]);
    let gets = counting.gets();
    assert!(
        gets < n as usize,
        "resolution issued {gets} object reads for {n} candidates; should not scale with candidate count"
    );
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
            text: None,
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
            text: None,
        })
        .await
        .unwrap();

    let exact_ids: Vec<Id> = exact.rows.iter().map(|row| row.id.clone()).collect();
    let ann_ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(vector::recall_at(&exact_ids, &ann_ids, 3), 1.0);
    assert_eq!(ann_ids, exact_ids);
}

#[tokio::test]
async fn ann_l2_loads_rabitq_while_cosine_skips_it() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    ns.upsert(doc(1, "near", 1, &["v"], [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "far", 2, &["v"], [0.0, 1.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    let rabitq_key = manifest.vector_indexes["embedding"]
        .rabitq_key
        .as_ref()
        .unwrap();
    object_store
        .put(rabitq_key, Bytes::from_static(b"corrupt"))
        .await
        .unwrap();

    let query = |metric| Query {
        filter: None,
        order_by: None,
        limit: None,
        aggregates: Vec::new(),
        exact_vector: None,
        approx_vector: Some(ApproxVectorQuery {
            column: "embedding".into(),
            vector: vec![1.0, 0.0],
            k: 1,
            probes: Some(16),
            metric: Some(metric),
        }),
        text: None,
    };
    let cosine = ns.query(query(DistanceMetric::Cosine)).await.unwrap();
    assert_eq!(cosine.rows[0].id, Id::U64(1));
    assert!(matches!(
        ns.query(query(DistanceMetric::L2)).await,
        Err(Error::Corrupt(_))
    ));
}

#[tokio::test]
async fn ann_vector_query_reads_flushed_append_postings() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc(1, "blue-far", 10, &["blue"], [10.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.upsert(doc(2, "red-near", 20, &["red"], [0.05, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.vector_indexes["embedding"].append_indexes.len(), 1);

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
                k: 1,
                probes: Some(16),
                metric: Some(DistanceMetric::L2),
            }),
            text: None,
        })
        .await
        .unwrap();

    let ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(2)]);
    assert_eq!(ann.rows[0].score, Some(-0.0025000002));
}

#[tokio::test]
async fn ann_recall_survives_flushed_insert_update_delete_churn_without_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    for (id, x) in [(1, 0.10), (2, 1.0), (3, 5.0), (4, 10.0)] {
        ns.upsert(doc(id, &format!("base-{id}"), id as i64, &["v"], [x, 0.0]))
            .await
            .unwrap();
    }
    indexer::flush(&ns).await.unwrap();
    let base_key = ns.load_manifest().await.unwrap().vector_indexes["embedding"]
        .key
        .clone();

    ns.upsert(doc(5, "insert-near", 50, &["v"], [0.05, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(3, "update-near", 30, &["v"], [0.20, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.delete(Id::U64(1)).await.unwrap();
    ns.upsert(doc(6, "insert-nearer", 60, &["v"], [0.08, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.upsert(doc(2, "update-far", 20, &["v"], [20.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(7, "insert-nearest", 70, &["v"], [0.03, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    let meta = manifest.vector_indexes.get("embedding").unwrap();
    assert_eq!(meta.key, base_key);
    assert_eq!(meta.append_indexes.len(), 3);

    let exact = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: Some(ExactVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 0.0],
                k: 4,
                metric: Some(DistanceMetric::L2),
            }),
            approx_vector: None,
            text: None,
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
                k: 4,
                probes: Some(16),
                metric: Some(DistanceMetric::L2),
            }),
            text: None,
        })
        .await
        .unwrap();
    let exact_ids: Vec<Id> = exact.rows.iter().map(|row| row.id.clone()).collect();
    let ann_ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ann_ids, exact_ids);

    let recall = ns
        .recall(RecallRequest {
            num: 8,
            top_k: 4,
            column: Some("embedding".into()),
            probes: Some(16),
            metric: Some(DistanceMetric::L2),
            filter: None,
        })
        .await
        .unwrap();
    assert_eq!(recall.avg_recall, 1.0);
    for sample in recall.samples {
        assert_eq!(sample.exhaustive_ids, sample.ann_ids);
    }
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
            text: None,
        })
        .await
        .unwrap();

    let ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(3), Id::U64(2)]);
    assert_eq!(ann.rows[0].score, Some(-0.0025000002));
}

#[tokio::test]
async fn ann_vector_query_drops_stale_index_versions() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    ns.upsert(doc(1, "old", 10, &["v"], [0.01, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "near", 20, &["v"], [10.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();
    let old_version = ns.load_manifest().await.unwrap().generation;

    ns.upsert(doc(1, "new", 30, &["v"], [100.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    let meta = manifest.vector_indexes.get("embedding").unwrap();
    assert!(manifest.generation > old_version);
    let mut index =
        vector::VectorIndex::decode(&object_store.get(&meta.key).await.unwrap().bytes).unwrap();
    let stale_local_id = index.postings[0].vectors.len() as u32;
    index.postings[0].vectors.push(vector::VectorEntry {
        id: Id::U64(1),
        vector: vec![0.0, 0.0],
        local_id: stale_local_id,
        version: old_version,
    });
    object_store
        .put(&meta.key, Bytes::from(index.encode().unwrap()))
        .await
        .unwrap();
    object_store
        .put(
            meta.rabitq_key.as_ref().unwrap(),
            Bytes::from(sana::rabitq::build_index(&index).unwrap().encode().unwrap()),
        )
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
                k: 1,
                probes: Some(16),
                metric: Some(DistanceMetric::L2),
            }),
            text: None,
        })
        .await
        .unwrap();

    let ids: Vec<Id> = ann.rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids, vec![Id::U64(2)]);
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
            text: None,
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
            text: None,
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

#[tokio::test]
async fn search_metrics_count_ann_and_text_work() {
    let dir = tempfile::tempdir().unwrap();
    let metrics = sana::metrics::Metrics::shared();
    let ns = Namespace::create(store(&dir), "docs")
        .await
        .unwrap()
        .with_metrics(metrics.clone());
    ns.upsert(doc(1, "blue sky", 1, &["v"], [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc(2, "red sun", 2, &["v"], [0.0, 1.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let ann = Query {
        approx_vector: Some(ApproxVectorQuery {
            column: "embedding".into(),
            vector: vec![1.0, 0.0],
            k: 1,
            probes: Some(16),
            metric: Some(DistanceMetric::L2),
        }),
        ..Query::all()
    };
    let result = ns.query(ann).await.unwrap();
    assert_eq!(result.rows[0].id, Id::U64(1));

    let text = Query {
        text: Some(TextQuery {
            column: "title".into(),
            query: "blue".into(),
            k: 5,
            params: Bm25Params::default(),
        }),
        ..Query::all()
    };
    let result = ns.query(text).await.unwrap();
    assert_eq!(result.rows[0].id, Id::U64(1));

    let search = metrics.snapshot().search;
    assert_eq!(search.ann_queries, 1);
    assert!(search.ann_candidates >= 1, "scan returned candidates");
    assert!(search.ann_estimated >= 1, "L2 path estimated codes");
    assert!(search.ann_reranked >= 1, "survivors were exact-reranked");
    assert_eq!(search.text_queries, 1);
    assert!(search.text_blocks_read >= 1, "MAXSCORE decoded blocks");
}
