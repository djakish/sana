//! Thin Axum service over the library contracts.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query as QueryParams, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::cache_warm::CacheWarmOptions;
use crate::error::Error;
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

#[derive(Clone)]
struct ApiState {
    store: Arc<dyn ObjectStore>,
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
    Router::new()
        .route("/healthz", get(health))
        .route("/v2/namespaces/{namespace}", post(write))
        .route("/v2/namespaces/{namespace}/query", post(query))
        .route("/v1/namespaces/{namespace}/metadata", get(metadata))
        .route("/v1/namespaces/{namespace}/_debug/recall", post(recall))
        .route(
            "/v1/namespaces/{namespace}/hint_cache_warm",
            get(warm_cache),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(ApiState { store })
}

pub async fn serve(store: Arc<dyn ObjectStore>, address: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    spawn_index_worker(store.clone());
    axum::serve(listener, router(store)).await
}

fn spawn_index_worker(store: Arc<dyn ObjectStore>) {
    const LEASE_MS: u64 = 30_000;
    const HEARTBEAT_MS: u64 = 1_000;
    const IDLE_POLL_MS: u64 = 100;
    const ERROR_RETRY_MS: u64 = 1_000;
    const RECONCILE_INTERVAL_MS: u64 = 30_000;

    let worker_id = format!("serve-{}", std::process::id());
    tokio::spawn(async move {
        let mut next_reconcile = tokio::time::Instant::now();
        loop {
            if tokio::time::Instant::now() >= next_reconcile {
                if let Err(error) = crate::index_queue::reconcile_unindexed(store.clone()).await {
                    eprintln!("index reconciliation failed: {error}");
                }
                next_reconcile = tokio::time::Instant::now()
                    + std::time::Duration::from_millis(RECONCILE_INTERVAL_MS);
            }

            match crate::index_queue::run_worker_once(
                store.clone(),
                &worker_id,
                LEASE_MS,
                HEARTBEAT_MS,
            )
            .await
            {
                Ok(Some(_)) => {}
                Ok(None) => {
                    tokio::time::sleep(std::time::Duration::from_millis(IDLE_POLL_MS)).await;
                }
                Err(error) => {
                    eprintln!("index worker failed: {error}");
                    tokio::time::sleep(std::time::Duration::from_millis(ERROR_RETRY_MS)).await;
                }
            }
        }
    });
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn write(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
    request: Result<Json<WriteRequest>, JsonRejection>,
) -> Result<Json<WriteResponse>, ApiError> {
    let Json(request) = request.map_err(json_rejection)?;
    let namespace = Namespace::create_or_open(state.store, &namespace).await?;
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
    let Json(request) = request.map_err(json_rejection)?;
    let namespace = Namespace::open(state.store, &namespace).await?;
    let response = match request {
        QueryRequest::Single { query, options } => {
            QueryResponse::Single(namespace.query_with_options(*query, options).await?)
        }
        QueryRequest::Multi { query, options } => {
            QueryResponse::Multi(namespace.multi_query_with_options(query, options).await?)
        }
    };
    Ok(Json(response))
}

async fn metadata(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
) -> Result<Json<crate::metadata::NamespaceMetadata>, ApiError> {
    let namespace = Namespace::open(state.store, &namespace).await?;
    Ok(Json(namespace.metadata().await?))
}

async fn recall(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
    request: Result<Json<RecallApiRequest>, JsonRejection>,
) -> Result<Json<RecallResult>, ApiError> {
    let Json(request) = request.map_err(json_rejection)?;
    let namespace = Namespace::open(state.store, &namespace).await?;
    Ok(Json(
        namespace
            .recall_with_options(request.request, request.options)
            .await?,
    ))
}

async fn warm_cache(
    State(state): State<ApiState>,
    Path(namespace): Path<String>,
    params: Result<QueryParams<WarmCacheParams>, QueryRejection>,
) -> Result<Json<WarmCacheResponse>, ApiError> {
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
