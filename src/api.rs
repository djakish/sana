//! Thin Axum service over the library contracts.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query as QueryParams, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::cache_warm::CacheWarmOptions;
use crate::error::Error;
use crate::metrics::Metrics;
use crate::namespace::Namespace;
use crate::object_store::ObjectStore;
use crate::query::{
    MultiQuery, MultiQueryResult, Query, QueryOptions, QueryResult, RecallRequest, RecallResult,
};
use crate::wal::{WalCursor, WalOp};
use crate::write::{
    ConditionalWriteOp, DeleteByFilterRequest, PatchByFilterRequest, WriteOptions, WriteOutcome,
};

const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_CONCURRENT_QUERY_SLOTS: usize = 16;
const QUERY_SLOT_WAIT: Duration = Duration::from_millis(800);
const READY_BACKEND_TIMEOUT: Duration = Duration::from_millis(500);
const READINESS_DRAIN_DELAY: Duration = Duration::from_secs(5);
const WORKER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct ApiState {
    store: Arc<dyn ObjectStore>,
    query_limiter: Arc<QueryLimiter>,
    metrics: Arc<Metrics>,
    health: Arc<HealthState>,
}

#[derive(Debug)]
pub struct HealthState {
    ready: AtomicBool,
    draining: AtomicBool,
    drain_notify: tokio::sync::Notify,
}

impl HealthState {
    fn ready() -> Self {
        Self {
            ready: AtomicBool::new(true),
            draining: AtomicBool::new(false),
            drain_notify: tokio::sync::Notify::new(),
        }
    }

    fn starting() -> Self {
        Self {
            ready: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            drain_notify: tokio::sync::Notify::new(),
        }
    }

    fn mark_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
        self.ready.store(false, Ordering::SeqCst);
        self.drain_notify.notify_waiters();
    }

    fn accepts_traffic(&self) -> bool {
        self.ready.load(Ordering::SeqCst) && !self.draining.load(Ordering::SeqCst)
    }

    fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    async fn wait_for_drain(&self) {
        loop {
            let notified = self.drain_notify.notified();
            if self.is_draining() {
                return;
            }
            notified.await;
        }
    }
}

struct QueryLimiter {
    namespaces: tokio::sync::Mutex<HashMap<String, Weak<tokio::sync::Semaphore>>>,
    max_slots: usize,
    wait: Duration,
}

impl QueryLimiter {
    fn new(max_slots: usize, wait: Duration) -> Self {
        Self {
            namespaces: tokio::sync::Mutex::new(HashMap::new()),
            max_slots,
            wait,
        }
    }

    async fn acquire(
        &self,
        namespace: &str,
        slots: u32,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
        let semaphore = {
            let mut namespaces = self.namespaces.lock().await;
            if namespaces.len() >= 1_024 {
                namespaces.retain(|_, semaphore| semaphore.strong_count() > 0);
            }
            match namespaces.get(namespace).and_then(Weak::upgrade) {
                Some(semaphore) => semaphore,
                None => {
                    let semaphore = Arc::new(tokio::sync::Semaphore::new(self.max_slots));
                    namespaces.insert(namespace.to_string(), Arc::downgrade(&semaphore));
                    semaphore
                }
            }
        };

        match tokio::time::timeout(self.wait, semaphore.acquire_many_owned(slots)).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(ApiError::Request {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "internal",
                message: "query concurrency limiter is unavailable".into(),
            }),
            Err(_) => Err(ApiError::Request {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "query_concurrency",
                message: format!(
                    "namespace query concurrency exceeded {0} slots",
                    self.max_slots
                ),
            }),
        }
    }

    async fn overloaded(&self) -> bool {
        let mut namespaces = self.namespaces.lock().await;
        namespaces.retain(|_, semaphore| semaphore.strong_count() > 0);
        namespaces
            .values()
            .filter_map(Weak::upgrade)
            .any(|semaphore| semaphore.available_permits() == 0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WriteRequest {
    Append {
        operations: Vec<WalOp>,
        #[serde(default)]
        idempotency_key: Option<String>,
        #[serde(default)]
        options: WriteOptions,
    },
    Conditional {
        writes: Vec<ConditionalWriteOp>,
        #[serde(default)]
        idempotency_key: Option<String>,
        #[serde(default)]
        options: WriteOptions,
    },
    PatchByFilter {
        request: PatchByFilterRequest,
        #[serde(default)]
        idempotency_key: Option<String>,
        #[serde(default)]
        options: WriteOptions,
    },
    DeleteByFilter {
        request: DeleteByFilterRequest,
        #[serde(default)]
        idempotency_key: Option<String>,
        #[serde(default)]
        options: WriteOptions,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteResponse {
    pub cursor: WalCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<WriteOutcome>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryRequest {
    Single {
        query: Box<Query>,
        #[serde(default)]
        options: QueryOptions,
    },
    Multi {
        query: MultiQuery,
        #[serde(default)]
        options: QueryOptions,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
pub enum QueryResponse {
    Single(QueryResult),
    Multi(MultiQueryResult),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallApiRequest {
    #[serde(flatten)]
    pub request: RecallRequest,
    #[serde(default)]
    pub options: QueryOptions,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
struct WarmCacheParams {
    max_bytes: Option<u64>,
    max_concurrency: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WarmCacheResponse {
    pub status: &'static str,
    pub message: &'static str,
    pub manifest_generation: u64,
    pub loaded_objects: usize,
    pub loaded_bytes: u64,
    pub skipped_objects: usize,
    pub skipped_bytes: u64,
}

#[derive(Debug)]
enum ApiError {
    Engine(Error),
    Request {
        status: StatusCode,
        code: &'static str,
        message: String,
    },
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self::Engine(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Engine(error) => {
                let (status, code) = match &error {
                    Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
                    Error::AlreadyExists(_)
                    | Error::CasMismatch { .. }
                    | Error::IdempotencyConflict(_) => (StatusCode::CONFLICT, "conflict"),
                    Error::Backpressure { .. } => (StatusCode::TOO_MANY_REQUESTS, "backpressure"),
                    Error::InvalidRange { .. }
                    | Error::InvalidWrite(_)
                    | Error::InvalidSchema(_)
                    | Error::InvalidQuery(_)
                    | Error::InvalidQueueClaim(_)
                    | Error::InvalidPinningClaim(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
                    Error::Corrupt(_) | Error::Codec(_) | Error::Io(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, "internal")
                    }
                };
                (status, code, error.to_string())
            }
            Self::Request {
                status,
                code,
                message,
            } => (status, code, message),
        };
        (
            status,
            Json(ErrorEnvelope {
                error: ErrorBody { code, message },
            }),
        )
            .into_response()
    }
}

pub fn router(store: Arc<dyn ObjectStore>) -> Router {
    router_with_metrics(store, Metrics::shared())
}

pub fn router_with_metrics(store: Arc<dyn ObjectStore>, metrics: Arc<Metrics>) -> Router {
    router_with_metrics_and_health(store, metrics, Arc::new(HealthState::ready()))
}

fn router_with_metrics_and_health(
    store: Arc<dyn ObjectStore>,
    metrics: Arc<Metrics>,
    health: Arc<HealthState>,
) -> Router {
    Router::new()
        .route("/healthz", get(livez))
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        .route("/v2/namespaces/{namespace}", post(write))
        .route("/v2/namespaces/{namespace}/query", post(query))
        .route("/v1/namespaces/{namespace}/metadata", get(metadata))
        .route("/v1/namespaces/{namespace}/_debug/recall", post(recall))
        .route(
            "/v1/namespaces/{namespace}/hint_cache_warm",
            get(warm_cache),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(ApiState {
            store,
            query_limiter: Arc::new(QueryLimiter::new(
                MAX_CONCURRENT_QUERY_SLOTS,
                QUERY_SLOT_WAIT,
            )),
            metrics,
            health,
        })
}

pub async fn serve(store: Arc<dyn ObjectStore>, address: SocketAddr) -> std::io::Result<()> {
    Box::pin(serve_with_shutdown(
        store,
        address,
        Metrics::shared(),
        std::future::pending(),
    ))
    .await
}

pub async fn serve_api_with_shutdown(
    store: Arc<dyn ObjectStore>,
    address: SocketAddr,
    metrics: Arc<Metrics>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    let health = Arc::new(HealthState::starting());
    let router = router_with_metrics_and_health(store, metrics, health.clone());
    health.mark_ready();
    axum::serve(listener, router)
        .with_graceful_shutdown(drain_on_shutdown(health, shutdown))
        .await
}

pub async fn serve_with_shutdown(
    store: Arc<dyn ObjectStore>,
    address: SocketAddr,
    metrics: Arc<Metrics>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    let health = Arc::new(HealthState::starting());
    let router = router_with_metrics_and_health(store.clone(), metrics.clone(), health.clone());
    health.mark_ready();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(drain_on_shutdown(health.clone(), shutdown))
        .into_future();
    let worker = async {
        let worker_health = health.clone();
        tokio::join!(
            run_index_worker(store.clone(), metrics, worker_health.clone()),
            run_maintenance_loop(store, worker_health),
        );
    };
    Box::pin(run_server_and_worker(server, worker, WORKER_DRAIN_TIMEOUT)).await
}

async fn drain_on_shutdown(health: Arc<HealthState>, shutdown: impl Future<Output = ()>) {
    shutdown.await;
    health.begin_drain();
    tokio::time::sleep(READINESS_DRAIN_DELAY).await;
}

/// Background maintenance: one policy pass per interval. The default policy
/// compacts and maintains vector topology but does not delete orphaned objects.
async fn run_maintenance_loop(store: Arc<dyn ObjectStore>, health: Arc<HealthState>) {
    const MAINTENANCE_INTERVAL_MS: u64 = 60_000;

    let policy = crate::maintenance::MaintenancePolicy::default();
    let mut state = crate::maintenance::MaintenanceState::default();
    loop {
        if sleep_or_drain(&health, Duration::from_millis(MAINTENANCE_INTERVAL_MS)).await {
            return;
        }
        match crate::maintenance::run_once(store.clone(), &policy, &mut state).await {
            Ok(report) => {
                for error in &report.errors {
                    eprintln!("maintenance: {error}");
                }
            }
            Err(error) => eprintln!("maintenance pass failed: {error}"),
        }
    }
}

async fn run_server_and_worker(
    server: impl Future<Output = std::io::Result<()>>,
    worker: impl Future<Output = ()>,
    worker_drain_timeout: Duration,
) -> std::io::Result<()> {
    tokio::pin!(server);
    tokio::pin!(worker);
    tokio::select! {
        result = &mut server => {
            let _ = tokio::time::timeout(worker_drain_timeout, &mut worker).await;
            result
        }
        () = &mut worker => server.await,
    }
}

async fn run_index_worker(
    store: Arc<dyn ObjectStore>,
    metrics: Arc<Metrics>,
    health: Arc<HealthState>,
) {
    const LEASE_MS: u64 = 30_000;
    const HEARTBEAT_MS: u64 = 1_000;
    const IDLE_POLL_MS: u64 = 100;
    const ERROR_RETRY_MS: u64 = 1_000;
    const RECONCILE_INTERVAL_MS: u64 = 30_000;

    let worker_id = format!("serve-{}", std::process::id());
    let mut next_reconcile = tokio::time::Instant::now();
    loop {
        if health.is_draining() {
            return;
        }
        if tokio::time::Instant::now() >= next_reconcile {
            // The reconcile scan doubles as the per-namespace lag observation.
            match crate::index_queue::reconcile_unindexed(store.clone()).await {
                Ok(report) => metrics.index_lag.record(report.lag),
                Err(error) => eprintln!("index reconciliation failed: {error}"),
            }
            next_reconcile = tokio::time::Instant::now()
                + std::time::Duration::from_millis(RECONCILE_INTERVAL_MS);
        }

        match crate::index_queue::run_worker_once(store.clone(), &worker_id, LEASE_MS, HEARTBEAT_MS)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                if sleep_or_drain(&health, Duration::from_millis(IDLE_POLL_MS)).await {
                    return;
                }
            }
            Err(error) => {
                eprintln!("index worker failed: {error}");
                if sleep_or_drain(&health, Duration::from_millis(ERROR_RETRY_MS)).await {
                    return;
                }
            }
        }
    }
}

async fn sleep_or_drain(health: &HealthState, duration: Duration) -> bool {
    tokio::select! {
        () = health.wait_for_drain() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

async fn livez() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn readyz(State(state): State<ApiState>) -> Response {
    if !state.health.accepts_traffic() {
        return readiness_response(StatusCode::SERVICE_UNAVAILABLE, "draining_or_starting");
    }
    if state.query_limiter.overloaded().await {
        return readiness_response(StatusCode::SERVICE_UNAVAILABLE, "overloaded");
    }
    match tokio::time::timeout(READY_BACKEND_TIMEOUT, state.store.list("namespaces/")).await {
        Ok(Ok(_)) => readiness_response(StatusCode::OK, "ready"),
        Ok(Err(_)) => readiness_response(StatusCode::SERVICE_UNAVAILABLE, "backend_unavailable"),
        Err(_) => readiness_response(StatusCode::SERVICE_UNAVAILABLE, "backend_timeout"),
    }
}

fn readiness_response(status: StatusCode, state: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "status": if status == StatusCode::OK { "ok" } else { "unavailable" },
            "state": state,
        })),
    )
        .into_response()
}

async fn metrics_endpoint(State(state): State<ApiState>) -> Response {
    (
        [("content-type", "text/plain; version=0.0.4")],
        state.metrics.snapshot().to_prometheus(),
    )
        .into_response()
}

async fn write(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
    request: Result<Json<WriteRequest>, JsonRejection>,
) -> Result<Json<WriteResponse>, ApiError> {
    ensure_accepting_traffic(&state)?;
    let Json(request) = request.map_err(json_rejection)?;
    let namespace = Namespace::create_or_open(state.store, &namespace)
        .await?
        .with_metrics(state.metrics.clone());
    let response = match request {
        WriteRequest::Append {
            operations,
            idempotency_key,
            options,
        } => WriteResponse {
            cursor: namespace
                .append_with_options(operations, idempotency_key, options)
                .await?,
            outcome: None,
        },
        WriteRequest::Conditional {
            writes,
            idempotency_key,
            options,
        } => {
            let result = namespace
                .conditional_write_with_options(writes, idempotency_key, options)
                .await?;
            WriteResponse {
                cursor: result.cursor,
                outcome: Some(result.outcome),
            }
        }
        WriteRequest::PatchByFilter {
            request,
            idempotency_key,
            options,
        } => {
            let result = namespace
                .patch_by_filter_with_options(request, idempotency_key, options)
                .await?;
            WriteResponse {
                cursor: result.cursor,
                outcome: Some(result.outcome),
            }
        }
        WriteRequest::DeleteByFilter {
            request,
            idempotency_key,
            options,
        } => {
            let result = namespace
                .delete_by_filter_with_options(request, idempotency_key, options)
                .await?;
            WriteResponse {
                cursor: result.cursor,
                outcome: Some(result.outcome),
            }
        }
    };
    Ok(Json(response))
}

async fn query(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
    request: Result<Json<QueryRequest>, JsonRejection>,
) -> Result<Json<QueryResponse>, ApiError> {
    ensure_accepting_traffic(&state)?;
    let Json(request) = request.map_err(json_rejection)?;
    let namespace_handle = Namespace::open(state.store, &namespace)
        .await?
        .with_metrics(state.metrics.clone());
    let _permit = state
        .query_limiter
        .acquire(&namespace, query_request_slots(&request))
        .await?;
    let response = match request {
        QueryRequest::Single { query, options } => {
            QueryResponse::Single(namespace_handle.query_with_options(*query, options).await?)
        }
        QueryRequest::Multi { query, options } => QueryResponse::Multi(
            namespace_handle
                .multi_query_with_options(query, options)
                .await?,
        ),
    };
    Ok(Json(response))
}

async fn metadata(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
) -> Result<Json<crate::metadata::NamespaceMetadata>, ApiError> {
    ensure_accepting_traffic(&state)?;
    let namespace = Namespace::open(state.store, &namespace).await?;
    Ok(Json(namespace.metadata().await?))
}

async fn recall(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
    request: Result<Json<RecallApiRequest>, JsonRejection>,
) -> Result<Json<RecallResult>, ApiError> {
    ensure_accepting_traffic(&state)?;
    let Json(request) = request.map_err(json_rejection)?;
    let namespace_handle = Namespace::open(state.store, &namespace)
        .await?
        .with_metrics(state.metrics.clone());
    let _permit = state.query_limiter.acquire(&namespace, 4).await?;
    Ok(Json(
        namespace_handle
            .recall_with_options(request.request, request.options)
            .await?,
    ))
}

async fn warm_cache(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
    params: Result<QueryParams<WarmCacheParams>, QueryRejection>,
) -> Result<Json<WarmCacheResponse>, ApiError> {
    ensure_accepting_traffic(&state)?;
    let QueryParams(params) = params.map_err(query_rejection)?;
    let namespace = Namespace::open(state.store, &namespace).await?;
    let defaults = CacheWarmOptions::default();
    let report = namespace
        .hint_cache_warm(CacheWarmOptions {
            max_bytes: params.max_bytes.unwrap_or(defaults.max_bytes),
            max_concurrency: params.max_concurrency.unwrap_or(defaults.max_concurrency),
        })
        .await?;
    Ok(Json(WarmCacheResponse {
        status: "ACCEPTED",
        message: "cache warm hint accepted",
        manifest_generation: report.plan.manifest_generation,
        loaded_objects: report.loaded_objects,
        loaded_bytes: report.loaded_bytes,
        skipped_objects: report.plan.skipped_objects,
        skipped_bytes: report.plan.skipped_bytes,
    }))
}

fn ensure_accepting_traffic(state: &ApiState) -> Result<(), ApiError> {
    if state.health.accepts_traffic() {
        Ok(())
    } else {
        Err(ApiError::Request {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "draining",
            message: "server is not ready to accept traffic".into(),
        })
    }
}

fn json_rejection(rejection: JsonRejection) -> ApiError {
    ApiError::Request {
        status: rejection.status(),
        code: "invalid_json",
        message: rejection.body_text(),
    }
}

fn query_rejection(rejection: QueryRejection) -> ApiError {
    ApiError::Request {
        status: rejection.status(),
        code: "invalid_query_parameters",
        message: rejection.body_text(),
    }
}

fn query_request_slots(request: &QueryRequest) -> u32 {
    match request {
        QueryRequest::Single { query, .. } => query_slots(query),
        QueryRequest::Multi { query, .. } => {
            query.queries.iter().map(query_slots).max().unwrap_or(1)
        }
    }
}

fn query_slots(query: &Query) -> u32 {
    if !query.aggregates.is_empty() {
        4
    } else if query.exact_vector.is_some() {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    use crate::object_store::{FsObjectStore, ObjectStore};

    async fn request(app: axum::Router, method: Method, uri: &str) -> axum::response::Response {
        app.oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn server_completion_cancels_worker_future() {
        let polls = Arc::new(AtomicUsize::new(0));
        let worker_polls = polls.clone();
        let worker = async move {
            loop {
                worker_polls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let server = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(())
        };

        super::run_server_and_worker(server, worker, Duration::from_millis(20))
            .await
            .unwrap();
        let polls_after_shutdown = polls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(polls.load(Ordering::SeqCst), polls_after_shutdown);
    }

    #[tokio::test]
    async fn query_limiter_times_out_and_recovers_per_namespace() {
        let limiter = super::QueryLimiter::new(2, Duration::from_millis(20));
        let all_slots = limiter.acquire("docs", 2).await.unwrap();

        let error = limiter.acquire("docs", 1).await.unwrap_err();
        assert!(matches!(
            error,
            super::ApiError::Request {
                status: axum::http::StatusCode::TOO_MANY_REQUESTS,
                code: "query_concurrency",
                ..
            }
        ));

        drop(limiter.acquire("other", 1).await.unwrap());
        drop(all_slots);
        drop(limiter.acquire("docs", 1).await.unwrap());
    }

    #[tokio::test]
    async fn readiness_tracks_drain_without_breaking_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let health = Arc::new(super::HealthState::ready());
        let app = super::router_with_metrics_and_health(
            store,
            crate::metrics::Metrics::shared(),
            health.clone(),
        );

        let response = request(app.clone(), Method::GET, "/livez").await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = request(app.clone(), Method::GET, "/readyz").await;
        assert_eq!(response.status(), StatusCode::OK);

        health.begin_drain();
        let response = request(app.clone(), Method::GET, "/livez").await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = request(app.clone(), Method::GET, "/readyz").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let response = request(app, Method::GET, "/v1/namespaces/docs/metadata").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["code"], "draining");
    }

    #[tokio::test]
    async fn readiness_fails_when_query_limiter_is_overloaded() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let limiter = Arc::new(super::QueryLimiter::new(1, Duration::from_millis(1)));
        let _permit = limiter.acquire("docs", 1).await.unwrap();
        let app = axum::Router::new()
            .route("/livez", get(super::livez))
            .route("/readyz", get(super::readyz))
            .with_state(super::ApiState {
                store,
                query_limiter: limiter,
                metrics: crate::metrics::Metrics::shared(),
                health: Arc::new(super::HealthState::ready()),
            });

        let response = request(app.clone(), Method::GET, "/livez").await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = request(app, Method::GET, "/readyz").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
