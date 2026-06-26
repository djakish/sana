//! HTTP transport for the global indexing queue broker.
//!
//! The queue file remains the durable source of truth. This module only moves
//! queue mutation requests between processes; the server delegates every
//! request to [`IndexQueueBroker`], which replies after its batched CAS is
//! durable.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::index_queue::{
    BrokerRegistration, ClaimHandle, ClaimedJob, EnqueueOutcome, IndexQueue, IndexQueueBroker,
    QueueClient,
};
use crate::metrics::Metrics;
use crate::object_store::ObjectStore;
use crate::wal::WalCursor;

const QUEUE_PROTOCOL_VERSION: u32 = 1;
const MAX_QUEUE_MESSAGE_BYTES: usize = 1024 * 1024;
const READY_BACKEND_TIMEOUT: Duration = Duration::from_millis(500);
const READINESS_DRAIN_DELAY: Duration = Duration::from_secs(5);
const BROKER_CHANNEL_CAPACITY: usize = 4_096;
const BROKER_MAX_BATCH: usize = 512;
const BROKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone)]
pub struct DiscoveredQueueClient {
    queue: IndexQueue,
    client: reqwest::Client,
    cached: Arc<tokio::sync::RwLock<Option<BrokerRegistration>>>,
}

#[derive(Clone)]
struct BrokerState {
    broker: IndexQueueBroker,
    registration: BrokerRegistration,
    store: Arc<dyn ObjectStore>,
    metrics: Arc<Metrics>,
    ready: Arc<AtomicBool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QueueRequest {
    version: u32,
    operation: QueueOperation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum QueueOperation {
    Enqueue {
        namespace: String,
        target_cursor: WalCursor,
    },
    Claim {
        worker_id: String,
        lease_ms: u64,
    },
    Heartbeat {
        handle: ClaimHandle,
        lease_ms: u64,
    },
    Complete {
        handle: ClaimHandle,
    },
    Fail {
        handle: ClaimHandle,
        retry_delay_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QueueResponse {
    version: u32,
    result: QueueResponseKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum QueueResponseKind {
    Enqueued { outcome: EnqueueOutcome },
    Claimed { job: Option<ClaimedJob> },
    Heartbeat { lease_expires_at_ms: u64 },
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QueueErrorKind {
    InvalidRequest,
    InvalidClaim,
    StaleBroker,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QueueErrorResponse {
    error: QueueErrorBody,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QueueErrorBody {
    kind: QueueErrorKind,
    message: String,
}

struct BrokerError(Error);

impl DiscoveredQueueClient {
    pub fn new(store: Arc<dyn ObjectStore>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(BROKER_REQUEST_TIMEOUT)
            .build()
            .map_err(|error| transport(format!("building queue HTTP client: {error}")))?;
        Ok(Self {
            queue: IndexQueue::new(store),
            client,
            cached: Arc::new(tokio::sync::RwLock::new(None)),
        })
    }

    async fn request(&self, operation: QueueOperation) -> Result<QueueResponseKind> {
        let registration = self.registration().await?;
        match self
            .request_registered(&registration, operation.clone())
            .await
        {
            Err(Error::InvalidQueueBroker(_)) => {
                self.invalidate(&registration).await;
                let replacement = self.registration().await?;
                self.request_registered(&replacement, operation).await
            }
            Err(error @ Error::Io(_)) => {
                // A timeout is ambiguous: the broker may have committed before
                // the response was lost. Do not repeat a claim or completion.
                self.invalidate(&registration).await;
                Err(error)
            }
            result => result,
        }
    }

    async fn registration(&self) -> Result<BrokerRegistration> {
        if let Some(registration) = self.cached.read().await.clone() {
            return Ok(registration);
        }
        let registration = self
            .queue
            .broker_registration()
            .await?
            .ok_or_else(|| Error::InvalidQueueBroker("no broker is registered".into()))?;
        *self.cached.write().await = Some(registration.clone());
        Ok(registration)
    }

    async fn invalidate(&self, failed: &BrokerRegistration) {
        let mut cached = self.cached.write().await;
        if cached.as_ref() == Some(failed) {
            *cached = None;
        }
    }

    async fn request_registered(
        &self,
        registration: &BrokerRegistration,
        operation: QueueOperation,
    ) -> Result<QueueResponseKind> {
        let endpoint = broker_endpoint(&registration.address)?;
        let payload = QueueRequest {
            version: QUEUE_PROTOCOL_VERSION,
            operation,
        };
        let body = serde_json::to_vec(&payload).map_err(|error| Error::Codec(error.to_string()))?;
        let response = self
            .client
            .post(endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| transport(format!("queue broker request failed: {error}")))?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_QUEUE_MESSAGE_BYTES as u64)
        {
            return Err(Error::Corrupt(
                "queue broker response exceeds the protocol size limit".into(),
            ));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| transport(format!("reading queue broker response: {error}")))?;
        if body.len() > MAX_QUEUE_MESSAGE_BYTES {
            return Err(Error::Corrupt(
                "queue broker response exceeds the protocol size limit".into(),
            ));
        }

        if status.is_success() {
            let response: QueueResponse = serde_json::from_slice(&body)
                .map_err(|error| Error::Codec(format!("queue broker response: {error}")))?;
            if response.version != QUEUE_PROTOCOL_VERSION {
                return Err(Error::Corrupt(format!(
                    "unsupported queue broker response version {}",
                    response.version
                )));
            }
            Ok(response.result)
        } else {
            let response: QueueErrorResponse = serde_json::from_slice(&body).map_err(|error| {
                Error::Corrupt(format!(
                    "queue broker returned HTTP {status} with an invalid error body: {error}"
                ))
            })?;
            Err(match response.error.kind {
                QueueErrorKind::InvalidRequest => Error::InvalidWrite(response.error.message),
                QueueErrorKind::InvalidClaim => Error::InvalidQueueClaim(response.error.message),
                QueueErrorKind::StaleBroker => Error::InvalidQueueBroker(response.error.message),
                QueueErrorKind::Internal => Error::Corrupt(format!(
                    "queue broker returned HTTP {status}: {}",
                    response.error.message
                )),
            })
        }
    }
}

#[async_trait]
impl QueueClient for DiscoveredQueueClient {
    async fn enqueue(&self, namespace: &str, target_cursor: WalCursor) -> Result<EnqueueOutcome> {
        match self
            .request(QueueOperation::Enqueue {
                namespace: namespace.to_string(),
                target_cursor,
            })
            .await?
        {
            QueueResponseKind::Enqueued { outcome } => Ok(outcome),
            response => Err(unexpected_response("enqueue", response)),
        }
    }

    async fn claim(&self, worker_id: &str, lease_ms: u64) -> Result<Option<ClaimedJob>> {
        match self
            .request(QueueOperation::Claim {
                worker_id: worker_id.to_string(),
                lease_ms,
            })
            .await?
        {
            QueueResponseKind::Claimed { job } => Ok(job),
            response => Err(unexpected_response("claim", response)),
        }
    }

    async fn heartbeat(&self, handle: &ClaimHandle, lease_ms: u64) -> Result<u64> {
        match self
            .request(QueueOperation::Heartbeat {
                handle: handle.clone(),
                lease_ms,
            })
            .await?
        {
            QueueResponseKind::Heartbeat {
                lease_expires_at_ms,
            } => Ok(lease_expires_at_ms),
            response => Err(unexpected_response("heartbeat", response)),
        }
    }

    async fn complete(&self, handle: &ClaimHandle) -> Result<()> {
        match self
            .request(QueueOperation::Complete {
                handle: handle.clone(),
            })
            .await?
        {
            QueueResponseKind::Completed => Ok(()),
            response => Err(unexpected_response("complete", response)),
        }
    }

    async fn fail(&self, handle: &ClaimHandle, retry_delay_ms: u64) -> Result<()> {
        match self
            .request(QueueOperation::Fail {
                handle: handle.clone(),
                retry_delay_ms,
            })
            .await?
        {
            QueueResponseKind::Failed => Ok(()),
            response => Err(unexpected_response("fail", response)),
        }
    }
}

fn router(
    store: Arc<dyn ObjectStore>,
    broker: IndexQueueBroker,
    registration: BrokerRegistration,
    metrics: Arc<Metrics>,
    ready: Arc<AtomicBool>,
) -> Router {
    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/indexing-queue", post(mutate))
        .layer(DefaultBodyLimit::max(MAX_QUEUE_MESSAGE_BYTES))
        .with_state(BrokerState {
            broker,
            registration,
            store,
            metrics,
            ready,
        })
}

pub async fn serve_with_shutdown(
    store: Arc<dyn ObjectStore>,
    address: SocketAddr,
    advertised_address: &str,
    owner_id: &str,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    serve_with_metrics_and_shutdown(
        store,
        address,
        advertised_address,
        owner_id,
        Metrics::shared(),
        shutdown,
    )
    .await
}

pub async fn serve_with_metrics_and_shutdown(
    store: Arc<dyn ObjectStore>,
    address: SocketAddr,
    advertised_address: &str,
    owner_id: &str,
    metrics: Arc<Metrics>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    validate_broker_address(advertised_address)?;
    let ready = Arc::new(AtomicBool::new(true));
    let (broker, registration) = IndexQueueBroker::register_with_metrics(
        store.clone(),
        advertised_address,
        owner_id,
        BROKER_CHANNEL_CAPACITY,
        BROKER_MAX_BATCH,
        metrics.clone(),
    )
    .await?;
    let app = router(store, broker, registration, metrics, ready.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            ready.store(false, Ordering::SeqCst);
            tokio::time::sleep(READINESS_DRAIN_DELAY).await;
        })
        .await?;
    Ok(())
}

async fn livez(State(state): State<BrokerState>) -> Response {
    if state.broker.is_healthy() {
        readiness_response(StatusCode::OK, "live")
    } else {
        readiness_response(StatusCode::INTERNAL_SERVER_ERROR, "broker_loop_stopped")
    }
}

async fn readyz(State(state): State<BrokerState>) -> Response {
    if !state.ready.load(Ordering::SeqCst) {
        return readiness_response(StatusCode::SERVICE_UNAVAILABLE, "draining");
    }
    if !state.broker.is_healthy() {
        return readiness_response(StatusCode::SERVICE_UNAVAILABLE, "broker_loop_stopped");
    }
    let queue = IndexQueue::new(state.store.clone()).with_metrics(state.metrics.clone());
    match tokio::time::timeout(READY_BACKEND_TIMEOUT, queue.broker_registration()).await {
        Ok(Ok(Some(registration))) if registration == state.registration => {
            readiness_response(StatusCode::OK, "ready")
        }
        Ok(Ok(_)) => readiness_response(StatusCode::SERVICE_UNAVAILABLE, "stale_broker"),
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

async fn metrics_endpoint(State(state): State<BrokerState>) -> Response {
    (
        [("content-type", "text/plain; version=0.0.4")],
        state.metrics.snapshot().to_prometheus(),
    )
        .into_response()
}

async fn mutate(
    State(state): State<BrokerState>,
    request: std::result::Result<Json<QueueRequest>, JsonRejection>,
) -> std::result::Result<Json<QueueResponse>, BrokerError> {
    if !state.ready.load(Ordering::SeqCst) {
        return Err(BrokerError(Error::Io(std::io::Error::other(
            "queue broker is draining",
        ))));
    }
    if !state.broker.is_healthy() {
        return Err(BrokerError(Error::Io(std::io::Error::other(
            "queue broker group-commit loop is unavailable",
        ))));
    }
    let Json(request) =
        request.map_err(|error| BrokerError(Error::InvalidWrite(error.body_text())))?;
    if request.version != QUEUE_PROTOCOL_VERSION {
        return Err(BrokerError(Error::InvalidWrite(format!(
            "unsupported queue broker request version {}",
            request.version
        ))));
    }
    let result = match request.operation {
        QueueOperation::Enqueue {
            namespace,
            target_cursor,
        } => QueueResponseKind::Enqueued {
            outcome: state.broker.enqueue(&namespace, target_cursor).await?,
        },
        QueueOperation::Claim {
            worker_id,
            lease_ms,
        } => QueueResponseKind::Claimed {
            job: state.broker.claim(&worker_id, lease_ms).await?,
        },
        QueueOperation::Heartbeat { handle, lease_ms } => QueueResponseKind::Heartbeat {
            lease_expires_at_ms: state.broker.heartbeat(&handle, lease_ms).await?,
        },
        QueueOperation::Complete { handle } => {
            state.broker.complete(&handle).await?;
            QueueResponseKind::Completed
        }
        QueueOperation::Fail {
            handle,
            retry_delay_ms,
        } => {
            state.broker.fail(&handle, retry_delay_ms).await?;
            QueueResponseKind::Failed
        }
    };
    Ok(Json(QueueResponse {
        version: QUEUE_PROTOCOL_VERSION,
        result,
    }))
}

impl From<Error> for BrokerError {
    fn from(error: Error) -> Self {
        Self(error)
    }
}

impl IntoResponse for BrokerError {
    fn into_response(self) -> Response {
        let (status, kind, message) = match self.0 {
            Error::InvalidWrite(message) => (
                StatusCode::BAD_REQUEST,
                QueueErrorKind::InvalidRequest,
                message,
            ),
            Error::InvalidQueueClaim(message) => {
                (StatusCode::CONFLICT, QueueErrorKind::InvalidClaim, message)
            }
            Error::InvalidQueueBroker(message) => {
                (StatusCode::CONFLICT, QueueErrorKind::StaleBroker, message)
            }
            error => (
                StatusCode::INTERNAL_SERVER_ERROR,
                QueueErrorKind::Internal,
                error.to_string(),
            ),
        };
        (
            status,
            Json(QueueErrorResponse {
                error: QueueErrorBody { kind, message },
            }),
        )
            .into_response()
    }
}

fn unexpected_response(operation: &str, response: QueueResponseKind) -> Error {
    Error::Corrupt(format!(
        "queue broker returned {response:?} for {operation}"
    ))
}

fn transport(message: String) -> Error {
    Error::Io(std::io::Error::other(message))
}

fn broker_endpoint(address: &str) -> Result<url::Url> {
    let mut normalized = address.trim().to_string();
    if normalized.is_empty() {
        return Err(Error::InvalidQueueBroker(
            "registered broker address is empty".into(),
        ));
    }
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    let base: url::Url = normalized.parse().map_err(|error| {
        Error::InvalidQueueBroker(format!(
            "invalid registered broker address {address:?}: {error}"
        ))
    })?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(Error::InvalidQueueBroker(format!(
            "registered broker address must use http or https, got {:?}",
            base.scheme()
        )));
    }
    base.join("v1/indexing-queue").map_err(|error| {
        Error::InvalidQueueBroker(format!(
            "invalid registered broker address {address:?}: {error}"
        ))
    })
}

fn validate_broker_address(address: &str) -> Result<()> {
    broker_endpoint(address).map(|_| ())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::index_queue::{IndexQueue, run_worker_once_with_client};
    use crate::namespace::Namespace;
    use crate::object_store::FsObjectStore;
    use crate::value::{Document, Id};

    async fn start_registered_broker(
        store: Arc<dyn ObjectStore>,
        owner_id: &str,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let advertised_address = format!("http://{address}");
        let metrics = Metrics::shared();
        let (broker, registration) = IndexQueueBroker::register_with_metrics(
            store.clone(),
            &advertised_address,
            owner_id,
            BROKER_CHANNEL_CAPACITY,
            BROKER_MAX_BATCH,
            metrics.clone(),
        )
        .await
        .unwrap();
        let app = router(
            store.clone(),
            broker,
            registration,
            metrics,
            Arc::new(AtomicBool::new(true)),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (address, server)
    }

    async fn start_broker(
        store: Arc<dyn ObjectStore>,
    ) -> (Arc<dyn QueueClient>, tokio::task::JoinHandle<()>) {
        let (_, server) = start_registered_broker(store.clone(), "test-broker").await;
        let client: Arc<dyn QueueClient> = Arc::new(DiscoveredQueueClient::new(store).unwrap());
        (client, server)
    }

    #[test]
    fn broker_address_rejects_empty_and_non_http_urls() {
        assert!(matches!(
            broker_endpoint(""),
            Err(Error::InvalidQueueBroker(_))
        ));
        assert!(matches!(
            broker_endpoint("ftp://broker.internal"),
            Err(Error::InvalidQueueBroker(_))
        ));
    }

    #[tokio::test]
    async fn http_client_covers_claim_heartbeat_fail_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let (client, server) = start_broker(store.clone()).await;

        client.enqueue("alpha", WalCursor::new(0, 1)).await.unwrap();
        let first = client.claim("worker-a", 10_000).await.unwrap().unwrap();
        let original_expiry = first.job.claim.as_ref().unwrap().lease_expires_at_ms;
        assert!(client.heartbeat(&first.handle, 60_000).await.unwrap() > original_expiry);
        client.fail(&first.handle, 0).await.unwrap();

        let second = client.claim("worker-b", 10_000).await.unwrap().unwrap();
        assert_eq!(second.handle.attempt, 2);
        assert!(matches!(
            client.complete(&first.handle).await,
            Err(Error::InvalidQueueClaim(_))
        ));
        client.complete(&second.handle).await.unwrap();
        assert!(IndexQueue::new(store).jobs().await.unwrap().is_empty());

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn broker_ack_is_durable_and_drives_remote_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let (client, server) = start_broker(store.clone()).await;
        let namespace = Namespace::create(store.clone(), "alpha")
            .await
            .unwrap()
            .with_queue_client(client.clone());

        let target = namespace.upsert(Document::new(Id::U64(1))).await.unwrap();
        let jobs = IndexQueue::new(store.clone()).jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs.first().expect("one queued job").target_cursor, target);

        let run = run_worker_once_with_client(store.clone(), client, "remote-worker", 10_000, 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.target_cursor, target);
        assert_eq!(
            namespace.load_manifest().await.unwrap().indexed_cursor,
            Some(target)
        );
        assert!(IndexQueue::new(store).jobs().await.unwrap().is_empty());

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn broker_exposes_queue_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let (address, server) = start_registered_broker(store.clone(), "metrics-broker").await;
        let client = DiscoveredQueueClient::new(store).unwrap();

        client.enqueue("alpha", WalCursor::new(0, 1)).await.unwrap();
        let body = reqwest::get(format!("http://{address}/metrics"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(body.contains("# TYPE sana_index_queue_broker_batches_total counter"));
        assert!(body.contains("\nsana_index_queue_jobs 1\n"));

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn client_rediscovers_replacement_after_stale_broker_response() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let (_, first_server) = start_registered_broker(store.clone(), "first").await;
        let client = DiscoveredQueueClient::new(store.clone()).unwrap();
        client.enqueue("alpha", WalCursor::new(0, 1)).await.unwrap();

        let (_, second_server) = start_registered_broker(store.clone(), "second").await;
        client.enqueue("beta", WalCursor::new(0, 1)).await.unwrap();

        let jobs = IndexQueue::new(store).jobs().await.unwrap();
        assert_eq!(
            jobs.iter()
                .map(|job| job.namespace.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );

        first_server.abort();
        second_server.abort();
        let _ = first_server.await;
        let _ = second_server.await;
    }
}
