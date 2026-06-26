#![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use sana::api::{
    QueryRequest, QueryResponse, RecallApiRequest, WriteRequest, WriteResponse, router,
    router_with_metrics,
};
use sana::indexer;
use sana::metadata::{IndexStatus, NamespaceMetadata};
use sana::metrics::Metrics;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, MeteredObjectStore, ObjectStore};
use sana::query::{FilterExpr, MultiQuery, Query, QueryOptions, RecallRequest};
use sana::reader_lease::READER_LEASE_PREFIX;
use sana::value::{Document, Id, Value, VectorValue};
use sana::wal::WalOp;
use sana::write::{ConditionalWriteOp, DeleteByFilterRequest, PatchByFilterRequest, WriteOptions};
use tower::ServiceExt;

fn store(dir: &tempfile::TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

fn document(id: u64) -> Document {
    let mut document = Document::new(Id::U64(id));
    document
        .attributes
        .insert("title".into(), Value::String("alpha".into()));
    document
        .vectors
        .insert("embedding".into(), VectorValue::F32(vec![1.0, 0.0]));
    document
}

async fn request(app: &Router, method: Method, uri: &str, body: Option<Vec<u8>>) -> Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    app.clone()
        .oneshot(builder.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap()
}

async fn json_body<T: serde::de::DeserializeOwned>(response: Response) -> T {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn http_write_query_metadata_recall_and_warm_cache_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let app = router(object_store.clone());

    let write = WriteRequest::Append {
        operations: vec![WalOp::Upsert {
            id: Id::U64(1),
            document: document(1),
        }],
        idempotency_key: Some("api-write-1".into()),
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs",
        Some(serde_json::to_vec(&write).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let write_response: WriteResponse = json_body(response).await;
    assert_eq!(write_response.cursor.seq, 1);

    let response = request(&app, Method::GET, "/v1/namespaces/docs/metadata", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let metadata: NamespaceMetadata = json_body(response).await;
    assert_eq!(metadata.index.status, IndexStatus::Updating);
    assert!(metadata.index.unindexed_bytes > 0);

    let query = QueryRequest::Single {
        query: Box::new(Query::all()),
        options: QueryOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs/query",
        Some(serde_json::to_vec(&query).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    match json_body::<QueryResponse>(response).await {
        QueryResponse::Single(result) => assert_eq!(result.rows[0].id, Id::U64(1)),
        QueryResponse::Multi(_) => panic!("expected single-query response"),
    }

    let multi_query = QueryRequest::Multi {
        query: MultiQuery {
            queries: vec![Query::all(), Query::all()],
        },
        options: QueryOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs/query",
        Some(serde_json::to_vec(&multi_query).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    match json_body::<QueryResponse>(response).await {
        QueryResponse::Multi(result) => assert_eq!(result.results.len(), 2),
        QueryResponse::Single(_) => panic!("expected multi-query response"),
    }
    assert_eq!(
        object_store.list(READER_LEASE_PREFIX).await.unwrap().len(),
        1,
        "HTTP query path should publish a durable per-process reader lease object"
    );

    let blocked_query = QueryRequest::Single {
        query: Box::new(Query::all()),
        options: QueryOptions {
            max_unindexed_wal_bytes: 0,
        },
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs/query",
        Some(serde_json::to_vec(&blocked_query).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let error: serde_json::Value = json_body(response).await;
    assert_eq!(error["error"]["code"], "backpressure");

    let namespace = Namespace::open(object_store, "docs").await.unwrap();
    indexer::flush(&namespace).await.unwrap();

    let recall = RecallApiRequest {
        request: RecallRequest {
            num: 1,
            top_k: 1,
            column: Some("embedding".into()),
            probes: Some(1),
            metric: None,
            filter: None,
        },
        options: QueryOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v1/namespaces/docs/_debug/recall",
        Some(serde_json::to_vec(&recall).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let recall: sana::query::RecallResult = json_body(response).await;
    assert_eq!(recall.sampled, 1);
    assert_eq!(recall.avg_recall, 1.0);

    let response = request(
        &app,
        Method::GET,
        "/v1/namespaces/docs/hint_cache_warm?max_bytes=1048576&max_concurrency=2",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let warm: serde_json::Value = json_body(response).await;
    assert_eq!(warm["status"], "ACCEPTED");

    let response = request(&app, Method::GET, "/v1/namespaces/docs/metadata", None).await;
    let metadata: NamespaceMetadata = json_body(response).await;
    assert_eq!(metadata.index.status, IndexStatus::UpToDate);
    assert_eq!(metadata.index.unindexed_bytes, 0);
}

#[tokio::test]
async fn http_conditional_and_filter_writes_return_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(store(&dir));

    let append = WriteRequest::Append {
        operations: vec![WalOp::Upsert {
            id: Id::U64(1),
            document: document(1),
        }],
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs",
        Some(serde_json::to_vec(&append).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let title_is_alpha = FilterExpr::Eq {
        column: "title".into(),
        value: Value::String("alpha".into()),
    };
    let conditional = WriteRequest::Conditional {
        writes: vec![ConditionalWriteOp {
            operation: WalOp::Patch {
                id: Id::U64(1),
                attributes: BTreeMap::from([("version".into(), Value::Int(2))]),
                vectors: BTreeMap::new(),
            },
            condition: Some(title_is_alpha.clone()),
        }],
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs",
        Some(serde_json::to_vec(&conditional).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result: WriteResponse = json_body(response).await;
    assert_eq!(result.outcome.unwrap().rows_patched, 1);

    let patch = WriteRequest::PatchByFilter {
        request: PatchByFilterRequest {
            filter: title_is_alpha,
            attributes: BTreeMap::from([("state".into(), Value::String("ready".into()))]),
            vectors: BTreeMap::new(),
            max_rows: 10,
            allow_partial: false,
        },
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs",
        Some(serde_json::to_vec(&patch).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result: WriteResponse = json_body(response).await;
    assert_eq!(result.outcome.unwrap().rows_patched, 1);

    let delete = WriteRequest::DeleteByFilter {
        request: DeleteByFilterRequest {
            filter: FilterExpr::Eq {
                column: "state".into(),
                value: Value::String("ready".into()),
            },
            max_rows: 10,
            allow_partial: false,
        },
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs",
        Some(serde_json::to_vec(&delete).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result: WriteResponse = json_body(response).await;
    assert_eq!(result.outcome.unwrap().rows_deleted, 1);
}

#[tokio::test]
async fn http_vector_json_round_trips_existing_f16_and_f32_columns() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let app = router(object_store.clone());

    let half_ns = Namespace::create(object_store.clone(), "half")
        .await
        .unwrap();
    let mut half_doc = Document::new(Id::U64(1));
    half_doc.vectors.insert(
        "embedding".into(),
        VectorValue::F16(vec![
            half::f16::from_f32(1.0).to_bits(),
            half::f16::from_f32(0.5).to_bits(),
        ]),
    );
    half_ns.upsert(half_doc).await.unwrap();

    let query = QueryRequest::Single {
        query: Box::new(Query::all()),
        options: QueryOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/half/query",
        Some(serde_json::to_vec(&query).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let returned = match json_body::<QueryResponse>(response).await {
        QueryResponse::Single(result) => result.rows[0].document.clone(),
        QueryResponse::Multi(_) => panic!("expected single-query response"),
    };
    assert!(matches!(returned.vectors["embedding"], VectorValue::F32(_)));

    let write = WriteRequest::Append {
        operations: vec![WalOp::Upsert {
            id: returned.id.clone(),
            document: returned,
        }],
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/half",
        Some(serde_json::to_vec(&write).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let stored = half_ns.lookup(&Id::U64(1)).await.unwrap().unwrap();
    assert!(matches!(stored.vectors["embedding"], VectorValue::F16(_)));

    let float_ns = Namespace::create(object_store.clone(), "float")
        .await
        .unwrap();
    float_ns.upsert(document(1)).await.unwrap();
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/float/query",
        Some(serde_json::to_vec(&query).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let returned = match json_body::<QueryResponse>(response).await {
        QueryResponse::Single(result) => result.rows[0].document.clone(),
        QueryResponse::Multi(_) => panic!("expected single-query response"),
    };
    let write = WriteRequest::Append {
        operations: vec![WalOp::Upsert {
            id: returned.id.clone(),
            document: returned,
        }],
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/float",
        Some(serde_json::to_vec(&write).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let stored = float_ns.lookup(&Id::U64(1)).await.unwrap().unwrap();
    assert!(matches!(stored.vectors["embedding"], VectorValue::F32(_)));
}

#[tokio::test]
async fn http_errors_use_stable_status_and_json_envelopes() {
    let dir = tempfile::tempdir().unwrap();
    Namespace::create(store(&dir), "bounded").await.unwrap();
    let app = router(store(&dir));

    let response = request(&app, Method::GET, "/v1/namespaces/missing/metadata", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: serde_json::Value = json_body(response).await;
    assert_eq!(error["error"]["code"], "not_found");

    let invalid = WriteRequest::Append {
        operations: Vec::new(),
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs",
        Some(serde_json::to_vec(&invalid).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value = json_body(response).await;
    assert_eq!(error["error"]["code"], "invalid_request");

    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/docs/query",
        Some(b"{not-json".to_vec()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value = json_body(response).await;
    assert_eq!(error["error"]["code"], "invalid_json");

    let too_many = QueryRequest::Multi {
        query: MultiQuery {
            queries: vec![Query::all(); 17],
        },
        options: QueryOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/bounded/query",
        Some(serde_json::to_vec(&too_many).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value = json_body(response).await;
    assert_eq!(error["error"]["code"], "invalid_request");

    let response = request(
        &app,
        Method::GET,
        "/v1/namespaces/bounded/hint_cache_warm?max_concurrency=18446744073709551615",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value = json_body(response).await;
    assert_eq!(error["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn metrics_endpoint_reports_object_store_traffic() {
    let dir = tempfile::tempdir().unwrap();
    let metrics = Metrics::shared();
    let metered: Arc<dyn ObjectStore> =
        Arc::new(MeteredObjectStore::new(store(&dir), metrics.clone()));
    let app = router_with_metrics(metered, metrics.clone());

    let before = metrics.snapshot().object_store;
    assert_eq!(before.puts_if_absent, 0);

    let write = WriteRequest::Append {
        operations: vec![WalOp::Upsert {
            id: Id::U64(1),
            document: document(1),
        }],
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/metered",
        Some(serde_json::to_vec(&write).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = request(&app, Method::GET, "/metrics", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; version=0.0.4")
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    assert!(text.contains("# TYPE sana_object_store_puts_if_absent_total counter"));
    let after = metrics.snapshot().object_store;
    assert!(
        after.puts_if_absent > 0,
        "the write committed via put-if-absent"
    );
    assert!(text.contains(&format!(
        "\nsana_object_store_puts_if_absent_total {}\n",
        after.puts_if_absent
    )));
}

#[tokio::test]
async fn metrics_endpoint_reports_write_and_query_latency() {
    let dir = tempfile::tempdir().unwrap();
    let metrics = Metrics::shared();
    let metered: Arc<dyn ObjectStore> =
        Arc::new(MeteredObjectStore::new(store(&dir), metrics.clone()));
    let app = router_with_metrics(metered, metrics.clone());

    let write = WriteRequest::Append {
        operations: vec![WalOp::Upsert {
            id: Id::U64(1),
            document: document(1),
        }],
        idempotency_key: None,
        options: WriteOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/timed",
        Some(serde_json::to_vec(&write).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let query = QueryRequest::Single {
        query: Box::new(Query::all()),
        options: QueryOptions::default(),
    };
    let response = request(
        &app,
        Method::POST,
        "/v2/namespaces/timed/query",
        Some(serde_json::to_vec(&query).unwrap()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // One append: commit + notify phases and one total; no plan phase (that is
    // filter-mutation candidate discovery). One unfiltered query: plan, rank,
    // and full-scan materialize; no candidate or overlay seam was crossed.
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.latency.write_total.count(), 1);
    assert_eq!(snapshot.latency.write_plan.count(), 0);
    assert_eq!(snapshot.latency.write_commit.count(), 1);
    assert_eq!(snapshot.latency.write_notify.count(), 1);
    assert_eq!(snapshot.latency.query_total.count(), 1);
    assert_eq!(snapshot.latency.query_plan.count(), 1);
    assert_eq!(snapshot.latency.query_rank.count(), 1);
    assert_eq!(snapshot.latency.query_materialize.count(), 1);
    assert!(snapshot.object_store.request_latency.count() > 0);

    let response = request(&app, Method::GET, "/metrics", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("# TYPE sana_write_seconds histogram"));
    assert!(text.contains("\nsana_write_seconds_count 1\n"));
    assert!(text.contains("sana_write_phase_seconds_count{phase=\"commit\"} 1\n"));
    assert!(text.contains("sana_query_phase_seconds_count{phase=\"plan\"} 1\n"));
    assert!(text.contains("sana_object_store_request_seconds_bucket{le=\"+Inf\"}"));
}
