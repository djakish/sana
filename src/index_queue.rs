//! Durable indexing notifications in one object-store JSON file.
//!
//! The WAL and manifest remain authoritative. Queue jobs only prompt workers
//! to catch a namespace up to a committed WAL cursor. All state transitions
//! use compare-and-set, claims have expiring leases, and the claim attempt
//! fences a timed-out worker from completing work after another worker takes
//! over.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::backpressure::unindexed_wal_bytes;
use crate::error::{Error, Result};
use crate::indexer;
use crate::metrics::{IndexLagSample, Metrics, QueueStateSample, incr};
use crate::namespace::{Namespace, now_ms, validate_namespace_name};
use crate::object_store::{GetResult, ObjectStore};
use crate::wal::WalCursor;

pub const INDEX_QUEUE_KEY: &str = "jobs/indexing_queue.json";
pub const INDEX_QUEUE_FORMAT_VERSION: u32 = 1;

const MAX_CAS_ATTEMPTS: usize = 64;
const BROKER_COMMIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct IndexQueue {
    store: Arc<dyn ObjectStore>,
    metrics: Option<Arc<Metrics>>,
}

#[derive(Clone)]
pub struct IndexQueueBroker {
    sender: mpsc::Sender<BrokerRequest>,
    healthy: Arc<AtomicBool>,
    metrics: Option<Arc<Metrics>>,
}

/// Queue mutation boundary shared by direct object-store access, the in-process
/// group-commit broker, and the future network broker client.
#[async_trait]
pub trait QueueClient: Send + Sync {
    async fn enqueue(&self, namespace: &str, target_cursor: WalCursor) -> Result<EnqueueOutcome>;

    async fn claim(&self, worker_id: &str, lease_ms: u64) -> Result<Option<ClaimedJob>>;

    async fn heartbeat(&self, handle: &ClaimHandle, lease_ms: u64) -> Result<u64>;

    async fn complete(&self, handle: &ClaimHandle) -> Result<()>;

    async fn fail(&self, handle: &ClaimHandle, retry_delay_ms: u64) -> Result<()>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerRegistration {
    pub address: String,
    pub owner_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexJob {
    pub id: u64,
    pub namespace: String,
    pub target_cursor: WalCursor,
    pub created_at_ms: u64,
    pub attempts: u32,
    pub available_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<JobClaim>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobClaim {
    pub worker_id: String,
    pub attempt: u32,
    pub lease_expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimHandle {
    pub job_id: u64,
    pub worker_id: String,
    pub attempt: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedJob {
    pub job: IndexJob,
    pub handle: ClaimHandle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnqueueOutcome {
    Added { job_id: u64 },
    Coalesced { job_id: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRun {
    pub job_id: u64,
    pub namespace: String,
    pub target_cursor: WalCursor,
    pub did_flush: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub scanned_namespaces: usize,
    pub lagging_namespaces: usize,
    pub notifications_added: usize,
    pub notifications_coalesced: usize,
    /// Exact per-namespace indexing lag observed by this scan, for every
    /// scanned namespace (zero-lag entries included so gauges reset).
    pub lag: BTreeMap<String, IndexLagSample>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct QueueFile {
    format_version: u32,
    next_job_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    broker: Option<BrokerRegistration>,
    jobs: Vec<IndexJob>,
}

impl QueueFile {
    fn empty() -> Self {
        Self {
            format_version: INDEX_QUEUE_FORMAT_VERSION,
            next_job_id: 1,
            broker: None,
            jobs: Vec::new(),
        }
    }

    fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|e| Error::Codec(e.to_string()))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let queue: Self = serde_json::from_slice(bytes).map_err(|e| Error::Codec(e.to_string()))?;
        queue.validate()?;
        Ok(queue)
    }

    fn validate(&self) -> Result<()> {
        if self.format_version != INDEX_QUEUE_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported indexing queue format version {}",
                self.format_version
            )));
        }

        let mut ids = BTreeSet::new();
        let mut max_id = 0;
        for job in &self.jobs {
            if job.id == 0 || !ids.insert(job.id) {
                return Err(Error::Corrupt(format!(
                    "invalid or duplicate indexing queue job id {}",
                    job.id
                )));
            }
            if validate_namespace_name(&job.namespace).is_err() {
                return Err(Error::Corrupt(format!(
                    "indexing queue job {} has an invalid namespace",
                    job.id
                )));
            }
            if let Some(claim) = &job.claim
                && claim.attempt != job.attempts
            {
                return Err(Error::Corrupt(format!(
                    "indexing queue job {} claim attempt {} does not match attempts {}",
                    job.id, claim.attempt, job.attempts
                )));
            }
            max_id = max_id.max(job.id);
        }
        if self.next_job_id <= max_id {
            return Err(Error::Corrupt(format!(
                "indexing queue next job id {} is not above live max id {}",
                self.next_job_id, max_id
            )));
        }
        if let Some(broker) = &self.broker
            && (broker.address.is_empty() || broker.owner_id.is_empty() || broker.generation == 0)
        {
            return Err(Error::Corrupt(
                "indexing queue has an invalid broker registration".into(),
            ));
        }
        Ok(())
    }

    fn state_sample(&self, timestamp_ms: u64) -> QueueStateSample {
        let active_namespaces: BTreeSet<&str> = self
            .jobs
            .iter()
            .filter_map(|job| {
                job.claim
                    .as_ref()
                    .filter(|claim| claim.lease_expires_at_ms > timestamp_ms)
                    .map(|_| job.namespace.as_str())
            })
            .collect();
        let jobs = u64::try_from(self.jobs.len()).unwrap_or(u64::MAX);
        let claimed_jobs = u64::try_from(
            self.jobs
                .iter()
                .filter(|job| {
                    job.claim
                        .as_ref()
                        .is_some_and(|claim| claim.lease_expires_at_ms > timestamp_ms)
                })
                .count(),
        )
        .unwrap_or(u64::MAX);
        let available_jobs = u64::try_from(
            self.jobs
                .iter()
                .filter(|job| {
                    job.available_at_ms <= timestamp_ms
                        && !active_namespaces.contains(job.namespace.as_str())
                })
                .count(),
        )
        .unwrap_or(u64::MAX);
        let oldest_job_age_seconds = self
            .jobs
            .iter()
            .map(|job| timestamp_ms.saturating_sub(job.created_at_ms) / 1_000)
            .max()
            .unwrap_or(0);
        QueueStateSample {
            jobs,
            available_jobs,
            claimed_jobs,
            oldest_job_age_seconds,
        }
    }

    fn register_broker(
        &mut self,
        address: &str,
        owner_id: &str,
    ) -> Result<Mutation<BrokerRegistration>> {
        if address.is_empty() {
            return Err(Error::InvalidQueueBroker(
                "broker address cannot be empty".into(),
            ));
        }
        if owner_id.is_empty() {
            return Err(Error::InvalidQueueBroker(
                "broker owner id cannot be empty".into(),
            ));
        }
        let generation = match &self.broker {
            Some(broker) => broker
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::Corrupt("queue broker generation exhausted".into()))?,
            None => 1,
        };
        let registration = BrokerRegistration {
            address: address.to_string(),
            owner_id: owner_id.to_string(),
            generation,
        };
        self.broker = Some(registration.clone());
        Ok(Mutation::Changed(registration))
    }
}

enum Mutation<T> {
    Unchanged(T),
    Changed(T),
}

impl<T> Mutation<T> {
    fn into_parts(self) -> (T, bool) {
        match self {
            Self::Unchanged(value) => (value, false),
            Self::Changed(value) => (value, true),
        }
    }
}

#[derive(Clone)]
enum BrokerOperation {
    Enqueue {
        namespace: String,
        target_cursor: WalCursor,
        timestamp_ms: u64,
    },
    Claim {
        worker_id: String,
        lease_ms: u64,
        timestamp_ms: u64,
    },
    Heartbeat {
        handle: ClaimHandle,
        lease_ms: u64,
        timestamp_ms: u64,
    },
    Complete {
        handle: ClaimHandle,
    },
    Fail {
        handle: ClaimHandle,
        retry_delay_ms: u64,
        timestamp_ms: u64,
    },
}

enum BrokerReply {
    Enqueued(EnqueueOutcome),
    Claimed(Option<ClaimedJob>),
    Heartbeat(u64),
    Completed,
    Failed,
}

struct BrokerRequest {
    operation: BrokerOperation,
    response: oneshot::Sender<Result<BrokerReply>>,
}

impl QueueFile {
    fn enqueue(
        &mut self,
        namespace: &str,
        target_cursor: WalCursor,
        timestamp_ms: u64,
    ) -> Result<Mutation<EnqueueOutcome>> {
        validate_namespace_name(namespace)?;

        if let Some(job) = self
            .jobs
            .iter_mut()
            .find(|job| job.namespace == namespace && job.claim.is_none())
        {
            let job_id = job.id;
            if target_cursor > job.target_cursor {
                job.target_cursor = target_cursor;
                return Ok(Mutation::Changed(EnqueueOutcome::Coalesced { job_id }));
            }
            return Ok(Mutation::Unchanged(EnqueueOutcome::Coalesced { job_id }));
        }

        if let Some(job) = self.jobs.iter().find(|job| {
            job.namespace == namespace && job.claim.is_some() && job.target_cursor >= target_cursor
        }) {
            return Ok(Mutation::Unchanged(EnqueueOutcome::Coalesced {
                job_id: job.id,
            }));
        }

        let job_id = self.next_job_id;
        self.next_job_id = self
            .next_job_id
            .checked_add(1)
            .ok_or_else(|| Error::Corrupt("indexing queue job id exhausted".into()))?;
        self.jobs.push(IndexJob {
            id: job_id,
            namespace: namespace.to_string(),
            target_cursor,
            created_at_ms: timestamp_ms,
            attempts: 0,
            available_at_ms: timestamp_ms,
            claim: None,
        });
        Ok(Mutation::Changed(EnqueueOutcome::Added { job_id }))
    }

    fn claim(
        &mut self,
        worker_id: &str,
        lease_ms: u64,
        timestamp_ms: u64,
    ) -> Result<Mutation<Option<ClaimedJob>>> {
        if worker_id.is_empty() {
            return Err(Error::InvalidQueueClaim("worker id cannot be empty".into()));
        }
        if lease_ms == 0 {
            return Err(Error::InvalidQueueClaim(
                "lease duration must be positive".into(),
            ));
        }

        let active_namespaces: BTreeSet<&str> = self
            .jobs
            .iter()
            .filter_map(|job| {
                job.claim
                    .as_ref()
                    .filter(|claim| claim.lease_expires_at_ms > timestamp_ms)
                    .map(|_| job.namespace.as_str())
            })
            .collect();
        let Some(index) = self.jobs.iter().position(|job| {
            job.available_at_ms <= timestamp_ms
                && !active_namespaces.contains(job.namespace.as_str())
        }) else {
            return Ok(Mutation::Unchanged(None));
        };

        let job = self
            .jobs
            .get_mut(index)
            .ok_or_else(|| Error::Corrupt("claim job index out of bounds".into()))?;
        job.attempts = job
            .attempts
            .checked_add(1)
            .ok_or_else(|| Error::Corrupt(format!("job {} attempts exhausted", job.id)))?;
        let claim = JobClaim {
            worker_id: worker_id.to_string(),
            attempt: job.attempts,
            lease_expires_at_ms: timestamp_ms.saturating_add(lease_ms),
        };
        let handle = ClaimHandle {
            job_id: job.id,
            worker_id: claim.worker_id.clone(),
            attempt: claim.attempt,
        };
        job.claim = Some(claim);
        Ok(Mutation::Changed(Some(ClaimedJob {
            job: job.clone(),
            handle,
        })))
    }

    fn heartbeat(
        &mut self,
        handle: &ClaimHandle,
        lease_ms: u64,
        timestamp_ms: u64,
    ) -> Result<Mutation<u64>> {
        if lease_ms == 0 {
            return Err(Error::InvalidQueueClaim(
                "lease duration must be positive".into(),
            ));
        }

        let job = matching_job_mut(self, handle)?;
        let claim = job.claim.as_mut().expect("matching claim exists");
        if claim.lease_expires_at_ms <= timestamp_ms {
            return Err(stale_claim(handle, "lease has expired"));
        }
        let lease_expires_at_ms = claim
            .lease_expires_at_ms
            .max(timestamp_ms.saturating_add(lease_ms));
        if lease_expires_at_ms == claim.lease_expires_at_ms {
            return Ok(Mutation::Unchanged(lease_expires_at_ms));
        }
        claim.lease_expires_at_ms = lease_expires_at_ms;
        Ok(Mutation::Changed(lease_expires_at_ms))
    }

    fn complete(&mut self, handle: &ClaimHandle) -> Result<Mutation<()>> {
        let index = matching_job_index(self, handle)?;
        self.jobs.remove(index);
        Ok(Mutation::Changed(()))
    }

    fn fail(
        &mut self,
        handle: &ClaimHandle,
        retry_delay_ms: u64,
        timestamp_ms: u64,
    ) -> Result<Mutation<()>> {
        let job = matching_job_mut(self, handle)?;
        job.claim = None;
        job.available_at_ms = timestamp_ms.saturating_add(retry_delay_ms);
        Ok(Mutation::Changed(()))
    }
}

impl IndexQueue {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            store,
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    fn with_optional_metrics(mut self, metrics: Option<Arc<Metrics>>) -> Self {
        self.metrics = metrics;
        self
    }

    fn record_state(&self, queue: &QueueFile, timestamp_ms: u64) {
        if let Some(metrics) = &self.metrics {
            metrics.queue.record_state(queue.state_sample(timestamp_ms));
        }
    }

    fn record_claim_wait(&self, timestamp_ms: u64, claimed: &ClaimedJob) {
        if let Some(metrics) = &self.metrics {
            metrics.queue.record_claim_wait(Duration::from_millis(
                timestamp_ms.saturating_sub(claimed.job.created_at_ms),
            ));
        }
    }

    /// Add a notification, or advance an existing unclaimed notification for
    /// the namespace to the newer WAL target.
    pub async fn enqueue(
        &self,
        namespace: &str,
        target_cursor: WalCursor,
    ) -> Result<EnqueueOutcome> {
        self.enqueue_at(namespace, target_cursor, now_ms()).await
    }

    async fn enqueue_at(
        &self,
        namespace: &str,
        target_cursor: WalCursor,
        timestamp_ms: u64,
    ) -> Result<EnqueueOutcome> {
        validate_namespace_name(namespace)?;
        self.mutate_at(timestamp_ms, |queue| {
            queue.enqueue(namespace, target_cursor, timestamp_ms)
        })
        .await
    }

    /// Claim the oldest available job whose namespace has no active worker.
    pub async fn claim(&self, worker_id: &str, lease_ms: u64) -> Result<Option<ClaimedJob>> {
        self.claim_at(worker_id, lease_ms, now_ms()).await
    }

    async fn claim_at(
        &self,
        worker_id: &str,
        lease_ms: u64,
        timestamp_ms: u64,
    ) -> Result<Option<ClaimedJob>> {
        let claimed = self
            .mutate_at(timestamp_ms, |queue| {
                queue.claim(worker_id, lease_ms, timestamp_ms)
            })
            .await?;
        if let Some(claimed) = &claimed {
            self.record_claim_wait(timestamp_ms, claimed);
        }
        Ok(claimed)
    }

    /// Extend a live claim. A stale or superseded handle is rejected.
    pub async fn heartbeat(&self, handle: &ClaimHandle, lease_ms: u64) -> Result<u64> {
        self.heartbeat_at(handle, lease_ms, now_ms()).await
    }

    async fn heartbeat_at(
        &self,
        handle: &ClaimHandle,
        lease_ms: u64,
        timestamp_ms: u64,
    ) -> Result<u64> {
        self.mutate_at(timestamp_ms, |queue| {
            queue.heartbeat(handle, lease_ms, timestamp_ms)
        })
        .await
    }

    /// Remove a successfully processed job.
    pub async fn complete(&self, handle: &ClaimHandle) -> Result<()> {
        self.mutate(|queue| queue.complete(handle)).await
    }

    /// Return a failed job to the queue after a bounded retry delay.
    pub async fn fail(&self, handle: &ClaimHandle, retry_delay_ms: u64) -> Result<()> {
        self.fail_at(handle, retry_delay_ms, now_ms()).await
    }

    async fn fail_at(
        &self,
        handle: &ClaimHandle,
        retry_delay_ms: u64,
        timestamp_ms: u64,
    ) -> Result<()> {
        self.mutate_at(timestamp_ms, |queue| {
            queue.fail(handle, retry_delay_ms, timestamp_ms)
        })
        .await
    }

    /// Read the current queue in FIFO order.
    pub async fn jobs(&self) -> Result<Vec<IndexJob>> {
        let (_, queue) = self.load_or_create().await?;
        self.record_state(&queue, now_ms());
        Ok(queue.jobs)
    }

    pub async fn broker_registration(&self) -> Result<Option<BrokerRegistration>> {
        let (_, queue) = self.load_or_create().await?;
        self.record_state(&queue, now_ms());
        Ok(queue.broker)
    }

    pub async fn register_broker(
        &self,
        address: &str,
        owner_id: &str,
    ) -> Result<BrokerRegistration> {
        self.mutate(|queue| queue.register_broker(address, owner_id))
            .await
    }

    async fn mutate<T>(
        &self,
        operation: impl FnMut(&mut QueueFile) -> Result<Mutation<T>>,
    ) -> Result<T> {
        self.mutate_at(now_ms(), operation).await
    }

    async fn mutate_at<T>(
        &self,
        state_timestamp_ms: u64,
        mut operation: impl FnMut(&mut QueueFile) -> Result<Mutation<T>>,
    ) -> Result<T> {
        for attempt in 0..MAX_CAS_ATTEMPTS {
            let (snapshot, mut queue) = self.load_or_create().await?;
            let value = match operation(&mut queue)? {
                Mutation::Unchanged(value) => {
                    self.record_state(&queue, state_timestamp_ms);
                    return Ok(value);
                }
                Mutation::Changed(value) => value,
            };
            queue.validate()?;
            let bytes = Bytes::from(queue.encode()?);
            if let Some(metrics) = &self.metrics {
                incr(&metrics.queue.cas_attempts);
            }
            match self
                .store
                .compare_and_set(INDEX_QUEUE_KEY, snapshot.version, bytes)
                .await
            {
                Ok(_) => {
                    if let Some(metrics) = &self.metrics {
                        incr(&metrics.queue.cas_successes);
                    }
                    self.record_state(&queue, state_timestamp_ms);
                    return Ok(value);
                }
                Err(Error::CasMismatch { .. }) if attempt + 1 < MAX_CAS_ATTEMPTS => {
                    if let Some(metrics) = &self.metrics {
                        incr(&metrics.queue.cas_retries);
                    }
                    tokio::task::yield_now().await;
                }
                Err(error @ Error::CasMismatch { .. }) => {
                    if let Some(metrics) = &self.metrics {
                        incr(&metrics.queue.cas_retries);
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded CAS loop returns on its final attempt")
    }

    async fn mutate_batch_owned(
        &self,
        operations: &[BrokerOperation],
        owner: Option<&BrokerRegistration>,
    ) -> Result<Vec<Result<BrokerReply>>> {
        let state_timestamp_ms = operations
            .iter()
            .filter_map(BrokerOperation::timestamp_ms)
            .max()
            .unwrap_or_else(now_ms);
        for attempt in 0..MAX_CAS_ATTEMPTS {
            let (snapshot, mut queue) = self.load_or_create().await?;
            if let Some(owner) = owner
                && queue.broker.as_ref() != Some(owner)
            {
                return Err(Error::InvalidQueueBroker(format!(
                    "broker {} generation {} no longer owns the queue",
                    owner.owner_id, owner.generation
                )));
            }
            let mut changed = false;
            let replies = operations
                .iter()
                .map(|operation| match operation.apply(&mut queue) {
                    Ok(mutation) => {
                        let (reply, operation_changed) = mutation.into_parts();
                        changed |= operation_changed;
                        Ok(reply)
                    }
                    Err(error) => Err(error),
                })
                .collect();
            if !changed {
                self.record_state(&queue, state_timestamp_ms);
                return Ok(replies);
            }

            queue.validate()?;
            let bytes = Bytes::from(queue.encode()?);
            if let Some(metrics) = &self.metrics {
                incr(&metrics.queue.cas_attempts);
            }
            match self
                .store
                .compare_and_set(INDEX_QUEUE_KEY, snapshot.version, bytes)
                .await
            {
                Ok(_) => {
                    if let Some(metrics) = &self.metrics {
                        incr(&metrics.queue.cas_successes);
                    }
                    self.record_state(&queue, state_timestamp_ms);
                    return Ok(replies);
                }
                Err(Error::CasMismatch { .. }) if attempt + 1 < MAX_CAS_ATTEMPTS => {
                    if let Some(metrics) = &self.metrics {
                        incr(&metrics.queue.cas_retries);
                    }
                    tokio::task::yield_now().await;
                }
                Err(error @ Error::CasMismatch { .. }) => {
                    if let Some(metrics) = &self.metrics {
                        incr(&metrics.queue.cas_retries);
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded CAS loop returns on its final attempt")
    }

    async fn load_or_create(&self) -> Result<(GetResult, QueueFile)> {
        loop {
            match self.store.get(INDEX_QUEUE_KEY).await {
                Ok(snapshot) => {
                    let queue = QueueFile::decode(&snapshot.bytes)?;
                    return Ok((snapshot, queue));
                }
                Err(Error::NotFound(_)) => {
                    let bytes = Bytes::from(QueueFile::empty().encode()?);
                    match self.store.put_if_absent(INDEX_QUEUE_KEY, bytes).await {
                        Ok(_) | Err(Error::AlreadyExists(_)) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[async_trait]
impl QueueClient for IndexQueue {
    async fn enqueue(&self, namespace: &str, target_cursor: WalCursor) -> Result<EnqueueOutcome> {
        IndexQueue::enqueue(self, namespace, target_cursor).await
    }

    async fn claim(&self, worker_id: &str, lease_ms: u64) -> Result<Option<ClaimedJob>> {
        IndexQueue::claim(self, worker_id, lease_ms).await
    }

    async fn heartbeat(&self, handle: &ClaimHandle, lease_ms: u64) -> Result<u64> {
        IndexQueue::heartbeat(self, handle, lease_ms).await
    }

    async fn complete(&self, handle: &ClaimHandle) -> Result<()> {
        IndexQueue::complete(self, handle).await
    }

    async fn fail(&self, handle: &ClaimHandle, retry_delay_ms: u64) -> Result<()> {
        IndexQueue::fail(self, handle, retry_delay_ms).await
    }
}

impl BrokerOperation {
    fn timestamp_ms(&self) -> Option<u64> {
        match self {
            Self::Enqueue { timestamp_ms, .. }
            | Self::Claim { timestamp_ms, .. }
            | Self::Heartbeat { timestamp_ms, .. }
            | Self::Fail { timestamp_ms, .. } => Some(*timestamp_ms),
            Self::Complete { .. } => None,
        }
    }

    fn apply(&self, queue: &mut QueueFile) -> Result<Mutation<BrokerReply>> {
        match self {
            Self::Enqueue {
                namespace,
                target_cursor,
                timestamp_ms,
            } => Ok(map_mutation(
                queue.enqueue(namespace, *target_cursor, *timestamp_ms)?,
                BrokerReply::Enqueued,
            )),
            Self::Claim {
                worker_id,
                lease_ms,
                timestamp_ms,
            } => Ok(map_mutation(
                queue.claim(worker_id, *lease_ms, *timestamp_ms)?,
                BrokerReply::Claimed,
            )),
            Self::Heartbeat {
                handle,
                lease_ms,
                timestamp_ms,
            } => Ok(map_mutation(
                queue.heartbeat(handle, *lease_ms, *timestamp_ms)?,
                BrokerReply::Heartbeat,
            )),
            Self::Complete { handle } => Ok(map_mutation(queue.complete(handle)?, |()| {
                BrokerReply::Completed
            })),
            Self::Fail {
                handle,
                retry_delay_ms,
                timestamp_ms,
            } => Ok(map_mutation(
                queue.fail(handle, *retry_delay_ms, *timestamp_ms)?,
                |()| BrokerReply::Failed,
            )),
        }
    }
}

fn map_mutation<T, U>(mutation: Mutation<T>, map: impl FnOnce(T) -> U) -> Mutation<U> {
    match mutation {
        Mutation::Unchanged(value) => Mutation::Unchanged(map(value)),
        Mutation::Changed(value) => Mutation::Changed(map(value)),
    }
}

impl IndexQueueBroker {
    /// Start a stateless group-commit broker. Requests that accumulated while a
    /// prior object-store write was in flight are applied with one CAS.
    pub fn start(store: Arc<dyn ObjectStore>, channel_capacity: usize, max_batch: usize) -> Self {
        Self::start_with_owner(store, None, channel_capacity, max_batch, None)
    }

    pub fn start_with_metrics(
        store: Arc<dyn ObjectStore>,
        channel_capacity: usize,
        max_batch: usize,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self::start_with_owner(store, None, channel_capacity, max_batch, Some(metrics))
    }

    pub async fn register(
        store: Arc<dyn ObjectStore>,
        address: &str,
        owner_id: &str,
        channel_capacity: usize,
        max_batch: usize,
    ) -> Result<(Self, BrokerRegistration)> {
        Self::register_with_optional_metrics(
            store,
            address,
            owner_id,
            channel_capacity,
            max_batch,
            None,
        )
        .await
    }

    pub async fn register_with_metrics(
        store: Arc<dyn ObjectStore>,
        address: &str,
        owner_id: &str,
        channel_capacity: usize,
        max_batch: usize,
        metrics: Arc<Metrics>,
    ) -> Result<(Self, BrokerRegistration)> {
        Self::register_with_optional_metrics(
            store,
            address,
            owner_id,
            channel_capacity,
            max_batch,
            Some(metrics),
        )
        .await
    }

    async fn register_with_optional_metrics(
        store: Arc<dyn ObjectStore>,
        address: &str,
        owner_id: &str,
        channel_capacity: usize,
        max_batch: usize,
        metrics: Option<Arc<Metrics>>,
    ) -> Result<(Self, BrokerRegistration)> {
        let queue = IndexQueue::new(store.clone()).with_optional_metrics(metrics.clone());
        let registration = queue.register_broker(address, owner_id).await?;
        let broker = Self::start_with_owner(
            store,
            Some(registration.clone()),
            channel_capacity,
            max_batch,
            metrics,
        );
        Ok((broker, registration))
    }

    fn start_with_owner(
        store: Arc<dyn ObjectStore>,
        owner: Option<BrokerRegistration>,
        channel_capacity: usize,
        max_batch: usize,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        Self::start_with_owner_and_timeout(
            store,
            owner,
            channel_capacity,
            max_batch,
            metrics,
            BROKER_COMMIT_TIMEOUT,
        )
    }

    fn start_with_owner_and_timeout(
        store: Arc<dyn ObjectStore>,
        owner: Option<BrokerRegistration>,
        channel_capacity: usize,
        max_batch: usize,
        metrics: Option<Arc<Metrics>>,
        commit_timeout: Duration,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(channel_capacity.max(1));
        let healthy = Arc::new(AtomicBool::new(true));
        tokio::spawn(run_broker(
            IndexQueue::new(store).with_optional_metrics(metrics.clone()),
            owner,
            receiver,
            max_batch.max(1),
            healthy.clone(),
            metrics.clone(),
            commit_timeout,
        ));
        Self {
            sender,
            healthy,
            metrics,
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn start_with_commit_timeout(
        store: Arc<dyn ObjectStore>,
        channel_capacity: usize,
        max_batch: usize,
        commit_timeout: Duration,
    ) -> Self {
        Self::start_with_owner_and_timeout(
            store,
            None,
            channel_capacity,
            max_batch,
            None,
            commit_timeout,
        )
    }

    pub async fn enqueue(
        &self,
        namespace: &str,
        target_cursor: WalCursor,
    ) -> Result<EnqueueOutcome> {
        validate_namespace_name(namespace)?;
        match self
            .request(BrokerOperation::Enqueue {
                namespace: namespace.to_string(),
                target_cursor,
                timestamp_ms: now_ms(),
            })
            .await?
        {
            BrokerReply::Enqueued(outcome) => Ok(outcome),
            _ => Err(unexpected_broker_reply("enqueue")),
        }
    }

    pub async fn claim(&self, worker_id: &str, lease_ms: u64) -> Result<Option<ClaimedJob>> {
        let result = match self
            .request(BrokerOperation::Claim {
                worker_id: worker_id.to_string(),
                lease_ms,
                timestamp_ms: now_ms(),
            })
            .await?
        {
            BrokerReply::Claimed(job) => Ok(job),
            _ => Err(unexpected_broker_reply("claim")),
        }?;
        if let Some(claimed) = &result
            && let Some(metrics) = &self.metrics
        {
            metrics.queue.record_claim_wait(Duration::from_millis(
                now_ms().saturating_sub(claimed.job.created_at_ms),
            ));
        }
        Ok(result)
    }

    pub async fn heartbeat(&self, handle: &ClaimHandle, lease_ms: u64) -> Result<u64> {
        match self
            .request(BrokerOperation::Heartbeat {
                handle: handle.clone(),
                lease_ms,
                timestamp_ms: now_ms(),
            })
            .await?
        {
            BrokerReply::Heartbeat(expires_at_ms) => Ok(expires_at_ms),
            _ => Err(unexpected_broker_reply("heartbeat")),
        }
    }

    pub async fn complete(&self, handle: &ClaimHandle) -> Result<()> {
        match self
            .request(BrokerOperation::Complete {
                handle: handle.clone(),
            })
            .await?
        {
            BrokerReply::Completed => Ok(()),
            _ => Err(unexpected_broker_reply("complete")),
        }
    }

    pub async fn fail(&self, handle: &ClaimHandle, retry_delay_ms: u64) -> Result<()> {
        match self
            .request(BrokerOperation::Fail {
                handle: handle.clone(),
                retry_delay_ms,
                timestamp_ms: now_ms(),
            })
            .await?
        {
            BrokerReply::Failed => Ok(()),
            _ => Err(unexpected_broker_reply("fail")),
        }
    }

    async fn request(&self, operation: BrokerOperation) -> Result<BrokerReply> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(BrokerRequest {
                operation,
                response,
            })
            .await
            .map_err(|error| Error::Corrupt(format!("indexing queue broker stopped: {error}")))?;
        receiver.await.map_err(|error| {
            Error::Corrupt(format!("indexing queue broker dropped a response: {error}"))
        })?
    }
}

#[async_trait]
impl QueueClient for IndexQueueBroker {
    async fn enqueue(&self, namespace: &str, target_cursor: WalCursor) -> Result<EnqueueOutcome> {
        IndexQueueBroker::enqueue(self, namespace, target_cursor).await
    }

    async fn claim(&self, worker_id: &str, lease_ms: u64) -> Result<Option<ClaimedJob>> {
        IndexQueueBroker::claim(self, worker_id, lease_ms).await
    }

    async fn heartbeat(&self, handle: &ClaimHandle, lease_ms: u64) -> Result<u64> {
        IndexQueueBroker::heartbeat(self, handle, lease_ms).await
    }

    async fn complete(&self, handle: &ClaimHandle) -> Result<()> {
        IndexQueueBroker::complete(self, handle).await
    }

    async fn fail(&self, handle: &ClaimHandle, retry_delay_ms: u64) -> Result<()> {
        IndexQueueBroker::fail(self, handle, retry_delay_ms).await
    }
}

async fn run_broker(
    queue: IndexQueue,
    owner: Option<BrokerRegistration>,
    mut receiver: mpsc::Receiver<BrokerRequest>,
    max_batch: usize,
    healthy: Arc<AtomicBool>,
    metrics: Option<Arc<Metrics>>,
    commit_timeout: Duration,
) {
    while let Some(first) = receiver.recv().await {
        let mut requests = Vec::with_capacity(max_batch);
        requests.push(first);
        while requests.len() < max_batch {
            match receiver.try_recv() {
                Ok(request) => requests.push(request),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
        if let Some(metrics) = &metrics {
            metrics
                .queue
                .record_broker_batch(u64::try_from(requests.len()).unwrap_or(u64::MAX));
        }

        let operations: Vec<BrokerOperation> = requests
            .iter()
            .map(|request| request.operation.clone())
            .collect();
        match tokio::time::timeout(
            commit_timeout,
            queue.mutate_batch_owned(&operations, owner.as_ref()),
        )
        .await
        {
            Ok(Ok(replies)) => {
                for (request, reply) in requests.into_iter().zip(replies) {
                    let _ = request.response.send(reply);
                }
            }
            Ok(Err(error)) => {
                for request in requests {
                    let _ = request.response.send(Err(copy_batch_error(&error)));
                }
                if matches!(error, Error::InvalidQueueBroker(_)) {
                    break;
                }
            }
            Err(_) => {
                for request in requests {
                    let _ = request.response.send(Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "indexing queue broker group commit timed out",
                    ))));
                }
                break;
            }
        }
    }
    healthy.store(false, Ordering::SeqCst);
}

fn unexpected_broker_reply(operation: &str) -> Error {
    Error::Corrupt(format!(
        "indexing queue broker returned the wrong reply for {operation}"
    ))
}

fn copy_batch_error(error: &Error) -> Error {
    match error {
        Error::InvalidQueueBroker(message) => Error::InvalidQueueBroker(message.clone()),
        Error::InvalidQueueClaim(message) => Error::InvalidQueueClaim(message.clone()),
        Error::InvalidWrite(message) => Error::InvalidWrite(message.clone()),
        error => Error::Io(std::io::Error::other(format!(
            "indexing queue broker batch failed: {error}"
        ))),
    }
}

/// Scan authoritative WAL/manifest state and restore any missed indexing
/// notifications through a temporary group-commit broker.
/// Every namespace with a manifest pointer under the store's `namespaces/`
/// prefix. Listing is not a hot path; this backs reconciliation and
/// maintenance scans.
pub async fn list_namespace_names(store: &Arc<dyn ObjectStore>) -> Result<BTreeSet<String>> {
    const POINTER_SUFFIX: &str = "/manifest/current";
    Ok(store
        .list("namespaces/")
        .await?
        .into_iter()
        .filter_map(|object| {
            object
                .key
                .strip_prefix("namespaces/")
                .and_then(|key| key.strip_suffix(POINTER_SUFFIX))
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .collect())
}

pub async fn reconcile_unindexed(store: Arc<dyn ObjectStore>) -> Result<ReconcileReport> {
    let broker: Arc<dyn QueueClient> = Arc::new(IndexQueueBroker::start(store.clone(), 1_024, 256));
    reconcile_unindexed_with_client(store, broker).await
}

/// Reconcile lagging namespaces through an existing broker.
pub async fn reconcile_unindexed_with_broker(
    store: Arc<dyn ObjectStore>,
    broker: &IndexQueueBroker,
) -> Result<ReconcileReport> {
    reconcile_unindexed_with_client(store, Arc::new(broker.clone())).await
}

/// Reconcile lagging namespaces through the configured queue client.
pub async fn reconcile_unindexed_with_client(
    store: Arc<dyn ObjectStore>,
    queue: Arc<dyn QueueClient>,
) -> Result<ReconcileReport> {
    let namespace_names = list_namespace_names(&store).await?;
    let mut report = ReconcileReport {
        scanned_namespaces: namespace_names.len(),
        ..ReconcileReport::default()
    };

    let mut reads = tokio::task::JoinSet::new();
    for namespace_name in namespace_names {
        let store = store.clone();
        reads.spawn(async move {
            let namespace = Namespace::open(store, &namespace_name).await?;
            let manifest = namespace.load_manifest().await?;
            // Manifest first, then the monotonic commit cursor: a concurrent
            // flush may advance the manifest between reads, but it cannot be
            // ahead of a commit cursor captured afterward.
            let (commit, committed_wal_bytes) = namespace.wal_commit_stats().await?;
            let bytes = unindexed_wal_bytes(&manifest, committed_wal_bytes)?;
            Ok::<_, Error>((namespace_name, commit, manifest.indexed_cursor, bytes))
        });
    }

    let mut lagging = Vec::new();
    while let Some(result) = reads.join_next().await {
        let (namespace, commit, indexed, unindexed_bytes) =
            result.map_err(|error| Error::Corrupt(format!("reconcile join error: {error}")))??;
        if indexed.is_some_and(|cursor| cursor > commit) {
            return Err(Error::Corrupt(format!(
                "namespace {namespace:?} indexed cursor {indexed:?} is ahead of commit {commit:?}"
            )));
        }
        let indexed = indexed.unwrap_or_else(|| WalCursor::new(commit.epoch, 0));
        report.lag.insert(
            namespace.clone(),
            IndexLagSample {
                unindexed_bytes,
                unindexed_batches: commit.seq.saturating_sub(indexed.seq),
            },
        );
        if indexed < commit {
            lagging.push((namespace, commit));
        }
    }
    report.lagging_namespaces = lagging.len();

    let mut enqueues = tokio::task::JoinSet::new();
    for (namespace, commit) in lagging {
        let queue = queue.clone();
        enqueues.spawn(async move { queue.enqueue(&namespace, commit).await });
    }
    while let Some(result) = enqueues.join_next().await {
        let outcome =
            result.map_err(|error| Error::Corrupt(format!("reconcile join error: {error}")))??;
        match outcome {
            EnqueueOutcome::Added { .. } => report.notifications_added += 1,
            EnqueueOutcome::Coalesced { .. } => report.notifications_coalesced += 1,
        }
    }

    Ok(report)
}

/// Claim and execute at most one indexing notification.
///
/// The worker heartbeats while the indexer runs. Successful work is completed
/// only after the manifest has reached the job's target WAL cursor. Failed work
/// is made available after `retry_delay_ms`.
pub async fn run_worker_once(
    store: Arc<dyn ObjectStore>,
    worker_id: &str,
    lease_ms: u64,
    retry_delay_ms: u64,
) -> Result<Option<WorkerRun>> {
    let queue: Arc<dyn QueueClient> = Arc::new(IndexQueue::new(store.clone()));
    run_worker_once_with_client(store, queue, worker_id, lease_ms, retry_delay_ms).await
}

pub async fn run_worker_once_with_client(
    store: Arc<dyn ObjectStore>,
    queue: Arc<dyn QueueClient>,
    worker_id: &str,
    lease_ms: u64,
    retry_delay_ms: u64,
) -> Result<Option<WorkerRun>> {
    let Some(claimed) = queue.claim(worker_id, lease_ms).await? else {
        return Ok(None);
    };

    let heartbeat_every_ms = lease_ms.div_ceil(3).max(1);
    let work = execute_index_job(
        store,
        queue.clone(),
        &claimed.job,
        claimed.handle.clone(),
        lease_ms,
    );
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            result = &mut work => break result,
            () = tokio::time::sleep(Duration::from_millis(heartbeat_every_ms)) => {
                queue.heartbeat(&claimed.handle, lease_ms).await?;
            }
        }
    };

    match result {
        Ok(did_flush) => {
            queue.complete(&claimed.handle).await?;
            Ok(Some(WorkerRun {
                job_id: claimed.job.id,
                namespace: claimed.job.namespace.clone(),
                target_cursor: claimed.job.target_cursor,
                did_flush,
            }))
        }
        Err(work_error) => {
            if let Err(queue_error) = queue.fail(&claimed.handle, retry_delay_ms).await {
                return Err(Error::Corrupt(format!(
                    "index job {} failed ({work_error}) and could not be returned to the queue: \
                     {queue_error}",
                    claimed.job.id
                )));
            }
            Err(work_error)
        }
    }
}

struct IndexJobPublishFence {
    queue: Arc<dyn QueueClient>,
    handle: ClaimHandle,
    lease_ms: u64,
}

#[async_trait]
impl indexer::ManifestPublishFence for IndexJobPublishFence {
    async fn verify(&self) -> Result<()> {
        self.queue.heartbeat(&self.handle, self.lease_ms).await?;
        Ok(())
    }
}

async fn execute_index_job(
    store: Arc<dyn ObjectStore>,
    queue: Arc<dyn QueueClient>,
    job: &IndexJob,
    handle: ClaimHandle,
    lease_ms: u64,
) -> Result<bool> {
    let namespace = Namespace::open(store, &job.namespace).await?;
    let fence = IndexJobPublishFence {
        queue,
        handle,
        lease_ms,
    };
    let did_flush = indexer::flush_with_fence(&namespace, &fence).await?;
    let manifest = namespace.load_manifest().await?;
    if manifest.indexed_cursor < Some(job.target_cursor) {
        return Err(Error::Corrupt(format!(
            "index job {} for namespace {:?} targeted WAL {:?}, but manifest only reached {:?}",
            job.id, job.namespace, job.target_cursor, manifest.indexed_cursor
        )));
    }
    Ok(did_flush)
}

fn matching_job_index(queue: &QueueFile, handle: &ClaimHandle) -> Result<usize> {
    let Some(index) = queue.jobs.iter().position(|job| job.id == handle.job_id) else {
        return Err(stale_claim(handle, "job no longer exists"));
    };
    let job = queue
        .jobs
        .get(index)
        .ok_or_else(|| stale_claim(handle, "job index out of bounds"))?;
    let matches = job.claim.as_ref().is_some_and(|claim| {
        claim.worker_id == handle.worker_id && claim.attempt == handle.attempt
    });
    if !matches {
        return Err(stale_claim(handle, "job is owned by another claim"));
    }
    Ok(index)
}

fn matching_job_mut<'a>(
    queue: &'a mut QueueFile,
    handle: &ClaimHandle,
) -> Result<&'a mut IndexJob> {
    let index = matching_job_index(queue, handle)?;
    queue
        .jobs
        .get_mut(index)
        .ok_or_else(|| stale_claim(handle, "job index out of bounds"))
}

fn stale_claim(handle: &ClaimHandle, reason: &str) -> Error {
    Error::InvalidQueueClaim(format!(
        "job {} attempt {} for worker {:?}: {reason}",
        handle.job_id, handle.attempt, handle.worker_id
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

    use std::ops::Range;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::{Notify, Semaphore};

    use super::*;
    use crate::object_store::{FsObjectStore, GetResult, ObjectMeta, ObjectVersion};
    use crate::value::{Document, Id};

    #[derive(Default)]
    struct RecordingEnqueueClient {
        notifications: tokio::sync::Mutex<Vec<(String, WalCursor)>>,
    }

    #[async_trait]
    impl QueueClient for RecordingEnqueueClient {
        async fn enqueue(
            &self,
            namespace: &str,
            target_cursor: WalCursor,
        ) -> Result<EnqueueOutcome> {
            self.notifications
                .lock()
                .await
                .push((namespace.to_string(), target_cursor));
            Ok(EnqueueOutcome::Added { job_id: 1 })
        }

        async fn claim(&self, _worker_id: &str, _lease_ms: u64) -> Result<Option<ClaimedJob>> {
            Err(Error::Corrupt("unexpected claim in enqueue test".into()))
        }

        async fn heartbeat(&self, _handle: &ClaimHandle, _lease_ms: u64) -> Result<u64> {
            Err(Error::Corrupt(
                "unexpected heartbeat in enqueue test".into(),
            ))
        }

        async fn complete(&self, _handle: &ClaimHandle) -> Result<()> {
            Err(Error::Corrupt("unexpected complete in enqueue test".into()))
        }

        async fn fail(&self, _handle: &ClaimHandle, _retry_delay_ms: u64) -> Result<()> {
            Err(Error::Corrupt("unexpected fail in enqueue test".into()))
        }
    }

    struct RejectingHeartbeatClient {
        target_cursor: WalCursor,
    }

    #[async_trait]
    impl QueueClient for RejectingHeartbeatClient {
        async fn enqueue(
            &self,
            _namespace: &str,
            _target_cursor: WalCursor,
        ) -> Result<EnqueueOutcome> {
            Err(Error::Corrupt(
                "unexpected enqueue in stale publish test".into(),
            ))
        }

        async fn claim(&self, worker_id: &str, lease_ms: u64) -> Result<Option<ClaimedJob>> {
            let attempt = 1;
            Ok(Some(ClaimedJob {
                job: IndexJob {
                    id: 1,
                    namespace: "alpha".into(),
                    target_cursor: self.target_cursor,
                    created_at_ms: 1,
                    attempts: attempt,
                    available_at_ms: 1,
                    claim: Some(JobClaim {
                        worker_id: worker_id.to_string(),
                        attempt,
                        lease_expires_at_ms: now_ms().saturating_add(lease_ms),
                    }),
                },
                handle: ClaimHandle {
                    job_id: 1,
                    worker_id: worker_id.to_string(),
                    attempt,
                },
            }))
        }

        async fn heartbeat(&self, handle: &ClaimHandle, _lease_ms: u64) -> Result<u64> {
            Err(Error::InvalidQueueClaim(format!(
                "job {} attempt {} for worker {:?}: stale test claim",
                handle.job_id, handle.attempt, handle.worker_id
            )))
        }

        async fn complete(&self, _handle: &ClaimHandle) -> Result<()> {
            Err(Error::Corrupt(
                "unexpected complete in stale publish test".into(),
            ))
        }

        async fn fail(&self, _handle: &ClaimHandle, _retry_delay_ms: u64) -> Result<()> {
            Ok(())
        }
    }

    struct CountingStore {
        inner: Arc<dyn ObjectStore>,
        queue_cas_count: AtomicUsize,
        fail_queue: bool,
        queue_gate: Option<Arc<Semaphore>>,
        queue_get_count: AtomicUsize,
        queue_get_entered: Notify,
    }

    impl CountingStore {
        fn new(inner: Arc<dyn ObjectStore>) -> Self {
            Self {
                inner,
                queue_cas_count: AtomicUsize::new(0),
                fail_queue: false,
                queue_gate: None,
                queue_get_count: AtomicUsize::new(0),
                queue_get_entered: Notify::new(),
            }
        }

        fn failing_queue(inner: Arc<dyn ObjectStore>) -> Self {
            Self {
                inner,
                queue_cas_count: AtomicUsize::new(0),
                fail_queue: true,
                queue_gate: None,
                queue_get_count: AtomicUsize::new(0),
                queue_get_entered: Notify::new(),
            }
        }

        fn blocking_queue(inner: Arc<dyn ObjectStore>, gate: Arc<Semaphore>) -> Self {
            Self {
                inner,
                queue_cas_count: AtomicUsize::new(0),
                fail_queue: false,
                queue_gate: Some(gate),
                queue_get_count: AtomicUsize::new(0),
                queue_get_entered: Notify::new(),
            }
        }

        async fn wait_for_queue_gets(&self, expected: usize) {
            loop {
                let notified = self.queue_get_entered.notified();
                if self.queue_get_count.load(Ordering::Acquire) >= expected {
                    return;
                }
                notified.await;
            }
        }

        fn reject_queue(&self, key: &str) -> Result<()> {
            if self.fail_queue && key == INDEX_QUEUE_KEY {
                return Err(Error::Io(std::io::Error::other(
                    "injected indexing queue outage",
                )));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl ObjectStore for CountingStore {
        async fn get(&self, key: &str) -> Result<GetResult> {
            self.reject_queue(key)?;
            if key == INDEX_QUEUE_KEY
                && let Some(gate) = &self.queue_gate
            {
                let get_number = self.queue_get_count.fetch_add(1, Ordering::Release) + 1;
                self.queue_get_entered.notify_waiters();
                if get_number <= 2 {
                    gate.acquire()
                        .await
                        .expect("queue gate remains open")
                        .forget();
                }
            }
            self.inner.get(key).await
        }

        async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
            self.reject_queue(key)?;
            self.inner.get_range(key, range).await
        }

        async fn put(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
            self.reject_queue(key)?;
            self.inner.put(key, bytes).await
        }

        async fn put_if_absent(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
            self.reject_queue(key)?;
            self.inner.put_if_absent(key, bytes).await
        }

        async fn compare_and_set(
            &self,
            key: &str,
            expected: ObjectVersion,
            bytes: Bytes,
        ) -> Result<ObjectVersion> {
            self.reject_queue(key)?;
            if key == INDEX_QUEUE_KEY {
                self.queue_cas_count.fetch_add(1, Ordering::Relaxed);
            }
            self.inner.compare_and_set(key, expected, bytes).await
        }

        async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
            self.inner.list(prefix).await
        }

        async fn delete(&self, key: &str) -> Result<()> {
            self.reject_queue(key)?;
            self.inner.delete(key).await
        }
    }

    fn queue(dir: &tempfile::TempDir) -> IndexQueue {
        IndexQueue::new(Arc::new(FsObjectStore::new(dir.path())))
    }

    #[tokio::test]
    async fn enqueue_coalesces_only_unclaimed_namespace_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue(&dir);

        assert_eq!(
            queue
                .enqueue_at("alpha", WalCursor::new(0, 1), 10)
                .await
                .unwrap(),
            EnqueueOutcome::Added { job_id: 1 }
        );
        assert_eq!(
            queue
                .enqueue_at("alpha", WalCursor::new(0, 3), 20)
                .await
                .unwrap(),
            EnqueueOutcome::Coalesced { job_id: 1 }
        );
        let first = queue.claim_at("worker-a", 100, 30).await.unwrap().unwrap();
        assert_eq!(first.job.target_cursor, WalCursor::new(0, 3));

        assert_eq!(
            queue
                .enqueue_at("alpha", WalCursor::new(0, 4), 40)
                .await
                .unwrap(),
            EnqueueOutcome::Added { job_id: 2 }
        );
        let jobs = queue.jobs().await.unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].claim.is_some());
        assert!(jobs[1].claim.is_none());
    }

    #[tokio::test]
    async fn enqueue_rejects_invalid_namespace_before_creating_queue() {
        let dir = tempfile::tempdir().unwrap();
        let object_store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let error = IndexQueue::new(object_store.clone())
            .enqueue("../invalid", WalCursor::new(0, 1))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidWrite(_)));
        assert!(matches!(
            object_store.get(INDEX_QUEUE_KEY).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn late_notification_coalesces_when_active_target_already_covers_it() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue(&dir);
        queue
            .enqueue_at("alpha", WalCursor::new(0, 2), 10)
            .await
            .unwrap();
        let active = queue.claim_at("worker", 100, 20).await.unwrap().unwrap();

        assert_eq!(
            queue
                .enqueue_at("alpha", WalCursor::new(0, 1), 30)
                .await
                .unwrap(),
            EnqueueOutcome::Coalesced {
                job_id: active.job.id
            }
        );
        assert_eq!(queue.jobs().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn expired_claim_is_recovered_and_old_worker_is_fenced() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue(&dir);
        queue
            .enqueue_at("alpha", WalCursor::new(0, 1), 1)
            .await
            .unwrap();

        let old = queue.claim_at("old", 10, 100).await.unwrap().unwrap();
        assert!(queue.claim_at("new", 10, 109).await.unwrap().is_none());
        let new = queue.claim_at("new", 10, 110).await.unwrap().unwrap();
        assert_eq!(new.handle.attempt, 2);

        assert!(matches!(
            queue.complete(&old.handle).await,
            Err(Error::InvalidQueueClaim(_))
        ));
        assert!(matches!(
            queue.heartbeat_at(&old.handle, 10, 111).await,
            Err(Error::InvalidQueueClaim(_))
        ));
        queue.complete(&new.handle).await.unwrap();
        assert!(queue.jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_job_waits_until_retry_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue(&dir);
        queue
            .enqueue_at("alpha", WalCursor::new(0, 1), 1)
            .await
            .unwrap();
        let claim = queue.claim_at("worker", 100, 10).await.unwrap().unwrap();

        queue.fail_at(&claim.handle, 50, 20).await.unwrap();
        assert!(queue.claim_at("worker", 100, 69).await.unwrap().is_none());
        let retried = queue.claim_at("worker", 100, 70).await.unwrap().unwrap();
        assert_eq!(retried.handle.attempt, 2);
    }

    #[tokio::test]
    async fn active_namespace_does_not_block_other_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue(&dir);
        queue
            .enqueue_at("alpha", WalCursor::new(0, 1), 1)
            .await
            .unwrap();
        let alpha = queue.claim_at("worker-a", 100, 2).await.unwrap().unwrap();
        queue
            .enqueue_at("alpha", WalCursor::new(0, 2), 3)
            .await
            .unwrap();
        queue
            .enqueue_at("beta", WalCursor::new(0, 1), 4)
            .await
            .unwrap();

        let beta = queue.claim_at("worker-b", 100, 5).await.unwrap().unwrap();
        assert_eq!(beta.job.namespace, "beta");
        queue.complete(&alpha.handle).await.unwrap();
        let next_alpha = queue.claim_at("worker-c", 100, 6).await.unwrap().unwrap();
        assert_eq!(next_alpha.job.namespace, "alpha");
        assert_eq!(next_alpha.job.id, 2);
    }

    #[tokio::test]
    async fn heartbeat_extends_but_never_shortens_lease() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue(&dir);
        queue
            .enqueue_at("alpha", WalCursor::new(0, 1), 1)
            .await
            .unwrap();
        let claim = queue.claim_at("worker", 100, 10).await.unwrap().unwrap();

        assert_eq!(
            queue.heartbeat_at(&claim.handle, 20, 20).await.unwrap(),
            110
        );
        assert_eq!(
            queue.heartbeat_at(&claim.handle, 100, 30).await.unwrap(),
            130
        );
    }

    #[tokio::test]
    async fn concurrent_pushers_preserve_highest_target() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue(&dir);
        let mut tasks = tokio::task::JoinSet::new();
        for seq in 1..=32 {
            let queue = queue.clone();
            tasks
                .spawn(async move { queue.enqueue_at("alpha", WalCursor::new(0, seq), seq).await });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }

        let jobs = queue.jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].target_cursor, WalCursor::new(0, 32));
    }

    #[tokio::test]
    async fn queue_client_trait_covers_the_full_job_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let client: Arc<dyn QueueClient> = Arc::new(IndexQueueBroker::start(store.clone(), 8, 8));

        client.enqueue("alpha", WalCursor::new(0, 1)).await.unwrap();
        let first = client.claim("worker-a", 10_000).await.unwrap().unwrap();
        let original_expiry = first
            .job
            .claim
            .as_ref()
            .expect("claimed job has lease metadata")
            .lease_expires_at_ms;
        let extended = client.heartbeat(&first.handle, 60_000).await.unwrap();
        assert!(extended > original_expiry);
        client.fail(&first.handle, 0).await.unwrap();

        let second = client.claim("worker-b", 10_000).await.unwrap().unwrap();
        assert_eq!(second.handle.attempt, 2);
        client.complete(&second.handle).await.unwrap();
        assert!(IndexQueue::new(store).jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn direct_queue_records_state_cas_and_claim_wait_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let metrics = Metrics::shared();
        let queue = queue(&dir).with_metrics(metrics.clone());

        queue
            .enqueue_at("alpha", WalCursor::new(0, 1), 1_000)
            .await
            .unwrap();
        queue
            .claim_at("worker", 10_000, 3_500)
            .await
            .unwrap()
            .unwrap();

        let snapshot = metrics.snapshot().queue;
        assert_eq!(snapshot.cas_attempts, 2);
        assert_eq!(snapshot.cas_successes, 2);
        assert_eq!(snapshot.cas_retries, 0);
        assert_eq!(snapshot.jobs, 1);
        assert_eq!(snapshot.available_jobs, 0);
        assert_eq!(snapshot.claimed_jobs, 1);
        assert_eq!(snapshot.oldest_job_age_seconds, 2);
        assert_eq!(snapshot.claim_wait.count(), 1);
    }

    #[tokio::test]
    async fn namespace_write_uses_the_configured_queue_client() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let client = Arc::new(RecordingEnqueueClient::default());
        let namespace = Namespace::create(store.clone(), "alpha")
            .await
            .unwrap()
            .with_queue_client(client.clone());

        let cursor = namespace.upsert(Document::new(Id::U64(1))).await.unwrap();

        assert_eq!(
            *client.notifications.lock().await,
            vec![("alpha".to_string(), cursor)]
        );
        assert!(matches!(
            store.get(INDEX_QUEUE_KEY).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn worker_flushes_to_target_and_completes_job() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let namespace = Namespace::create(store.clone(), "alpha").await.unwrap();
        let target = namespace.upsert(Document::new(Id::U64(1))).await.unwrap();

        let run = run_worker_once(store.clone(), "worker", 1_000, 10)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.namespace, "alpha");
        assert_eq!(run.target_cursor, target);
        assert!(run.did_flush);
        assert_eq!(
            namespace.load_manifest().await.unwrap().indexed_cursor,
            Some(target)
        );
        assert!(queue(&dir).jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_worker_claim_cannot_publish_flushed_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let namespace = Namespace::create(store.clone(), "alpha").await.unwrap();
        let target = namespace.upsert(Document::new(Id::U64(1))).await.unwrap();
        let queue: Arc<dyn QueueClient> = Arc::new(RejectingHeartbeatClient {
            target_cursor: target,
        });

        let error = run_worker_once_with_client(store, queue, "worker", 60_000, 10)
            .await
            .unwrap_err();

        assert!(matches!(error, Error::InvalidQueueClaim(_)));
        assert_eq!(
            namespace.load_manifest().await.unwrap().indexed_cursor,
            None
        );
    }

    #[tokio::test]
    async fn completed_work_is_safe_to_repeat_after_worker_crash() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let namespace = Namespace::create(store.clone(), "alpha").await.unwrap();
        namespace.upsert(Document::new(Id::U64(1))).await.unwrap();
        let queue = IndexQueue::new(store.clone());
        let enqueued_at = queue.jobs().await.unwrap()[0].available_at_ms;

        let lease_ms = 10_000;
        let queue_client: Arc<dyn QueueClient> = Arc::new(queue.clone());
        let old = queue
            .claim_at("old-worker", lease_ms, enqueued_at)
            .await
            .unwrap()
            .unwrap();
        assert!(
            execute_index_job(
                store.clone(),
                queue_client.clone(),
                &old.job,
                old.handle.clone(),
                lease_ms,
            )
            .await
            .unwrap()
        );
        let expired_at = queue.jobs().await.unwrap()[0]
            .claim
            .as_ref()
            .unwrap()
            .lease_expires_at_ms;

        let replacement = queue
            .claim_at("new-worker", lease_ms, expired_at)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !execute_index_job(
                store,
                queue_client,
                &replacement.job,
                replacement.handle.clone(),
                lease_ms,
            )
            .await
            .unwrap()
        );
        queue.complete(&replacement.handle).await.unwrap();
        assert!(queue.jobs().await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broker_group_commits_buffered_pushes_with_one_cas() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let store = Arc::new(CountingStore::new(inner));
        let metrics = Metrics::shared();
        let broker = IndexQueueBroker::start_with_metrics(store.clone(), 64, 64, metrics.clone());
        let mut responses = Vec::new();

        for seq in 1..=32 {
            let (response, receiver) = oneshot::channel();
            broker
                .sender
                .try_send(BrokerRequest {
                    operation: BrokerOperation::Enqueue {
                        namespace: "alpha".into(),
                        target_cursor: WalCursor::new(0, seq),
                        timestamp_ms: seq,
                    },
                    response,
                })
                .unwrap();
            responses.push(receiver);
        }
        for response in responses {
            assert!(matches!(
                response.await.unwrap().unwrap(),
                BrokerReply::Enqueued(_)
            ));
        }

        assert_eq!(store.queue_cas_count.load(Ordering::Relaxed), 1);
        let jobs = IndexQueue::new(store).jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].target_cursor, WalCursor::new(0, 32));
        let snapshot = metrics.snapshot().queue;
        assert_eq!(snapshot.broker_batches, 1);
        assert_eq!(snapshot.broker_batch_requests, 32);
        assert_eq!(snapshot.broker_batch_size.count(), 1);
        assert_eq!(snapshot.cas_attempts, 1);
        assert_eq!(snapshot.cas_successes, 1);
        assert_eq!(snapshot.jobs, 1);
    }

    #[tokio::test]
    async fn broker_marks_itself_unhealthy_after_group_commit_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let gate = Arc::new(Semaphore::new(0));
        let store: Arc<dyn ObjectStore> =
            Arc::new(CountingStore::blocking_queue(inner, gate.clone()));
        let broker =
            IndexQueueBroker::start_with_commit_timeout(store, 8, 8, Duration::from_millis(250));

        assert!(matches!(
            broker.enqueue("alpha", WalCursor::new(0, 1)).await,
            Err(Error::Io(_))
        ));
        assert!(!broker.is_healthy());
        gate.add_permits(2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_broker_request_does_not_poison_valid_batch_member() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let broker = IndexQueueBroker::start(store.clone(), 8, 8);
        let (invalid_tx, invalid_rx) = oneshot::channel();
        let (valid_tx, valid_rx) = oneshot::channel();

        broker
            .sender
            .try_send(BrokerRequest {
                operation: BrokerOperation::Claim {
                    worker_id: "worker".into(),
                    lease_ms: 0,
                    timestamp_ms: 10,
                },
                response: invalid_tx,
            })
            .unwrap();
        broker
            .sender
            .try_send(BrokerRequest {
                operation: BrokerOperation::Enqueue {
                    namespace: "alpha".into(),
                    target_cursor: WalCursor::new(0, 1),
                    timestamp_ms: 10,
                },
                response: valid_tx,
            })
            .unwrap();

        assert!(matches!(
            invalid_rx.await.unwrap(),
            Err(Error::InvalidQueueClaim(_))
        ));
        assert!(matches!(
            valid_rx.await.unwrap().unwrap(),
            BrokerReply::Enqueued(EnqueueOutcome::Added { job_id: 1 })
        ));
        assert_eq!(IndexQueue::new(store).jobs().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn replacement_broker_resumes_from_durable_queue_file() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let first = IndexQueueBroker::start(store.clone(), 8, 8);
        first.enqueue("alpha", WalCursor::new(0, 1)).await.unwrap();
        drop(first);

        let replacement = IndexQueueBroker::start(store.clone(), 8, 8);
        replacement
            .enqueue("beta", WalCursor::new(0, 1))
            .await
            .unwrap();

        let jobs = IndexQueue::new(store).jobs().await.unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].namespace, "alpha");
        assert_eq!(jobs[1].namespace, "beta");
    }

    #[tokio::test]
    async fn overlapping_brokers_preserve_all_jobs_through_cas_retries() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let first = IndexQueueBroker::start(store.clone(), 32, 16);
        let second = IndexQueueBroker::start(store.clone(), 32, 16);
        let mut tasks = tokio::task::JoinSet::new();

        for index in 0..32 {
            let broker = if index % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            tasks.spawn(async move {
                broker
                    .enqueue(&format!("namespace-{index}"), WalCursor::new(0, 1))
                    .await
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }

        let jobs = IndexQueue::new(store).jobs().await.unwrap();
        assert_eq!(jobs.len(), 32);
        let namespaces: BTreeSet<&str> = jobs.iter().map(|job| job.namespace.as_str()).collect();
        assert_eq!(namespaces.len(), 32);
    }

    #[tokio::test]
    async fn replacement_broker_fences_the_previous_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let (first, first_registration) =
            IndexQueueBroker::register(store.clone(), "http://first:8090", "first", 8, 8)
                .await
                .unwrap();
        first.enqueue("alpha", WalCursor::new(0, 1)).await.unwrap();

        let (second, second_registration) =
            IndexQueueBroker::register(store.clone(), "http://second:8090", "second", 8, 8)
                .await
                .unwrap();
        assert!(second_registration.generation > first_registration.generation);
        assert!(matches!(
            first.enqueue("stale", WalCursor::new(0, 1)).await,
            Err(Error::InvalidQueueBroker(_))
        ));

        second.enqueue("beta", WalCursor::new(0, 1)).await.unwrap();
        let jobs = IndexQueue::new(store).jobs().await.unwrap();
        assert_eq!(
            jobs.iter()
                .map(|job| job.namespace.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }

    #[tokio::test]
    async fn reconciliation_restores_only_lagging_namespace_notifications() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let alpha = Namespace::create(store.clone(), "alpha").await.unwrap();
        let beta = Namespace::create(store.clone(), "beta").await.unwrap();
        let alpha_target = alpha.upsert(Document::new(Id::U64(1))).await.unwrap();
        beta.upsert(Document::new(Id::U64(2))).await.unwrap();
        assert!(indexer::flush(&beta).await.unwrap());

        store.delete(INDEX_QUEUE_KEY).await.unwrap();
        let report = reconcile_unindexed(store.clone()).await.unwrap();
        assert_eq!(report.scanned_namespaces, 2);
        assert_eq!(report.lagging_namespaces, 1);
        assert_eq!(report.notifications_added, 1);
        assert_eq!(report.notifications_coalesced, 0);
        let alpha_lag = report.lag["alpha"];
        assert_eq!(alpha_lag.unindexed_batches, 1);
        assert!(alpha_lag.unindexed_bytes > 0);
        assert_eq!(report.lag["beta"], IndexLagSample::default());
        let jobs = IndexQueue::new(store.clone()).jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].namespace, "alpha");
        assert_eq!(jobs[0].target_cursor, alpha_target);

        let repeated = reconcile_unindexed(store).await.unwrap();
        assert_eq!(repeated.lagging_namespaces, 1);
        assert_eq!(repeated.notifications_added, 0);
        assert_eq!(repeated.notifications_coalesced, 1);
    }

    #[tokio::test]
    async fn queue_outage_does_not_fail_a_durable_write() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let failing: Arc<dyn ObjectStore> = Arc::new(CountingStore::failing_queue(inner.clone()));
        let namespace = Namespace::create(failing, "alpha").await.unwrap();

        let target = namespace.upsert(Document::new(Id::U64(1))).await.unwrap();
        assert_eq!(namespace.commit_cursor().await.unwrap(), target);
        assert!(namespace.lookup(&Id::U64(1)).await.unwrap().is_some());
        assert!(matches!(
            inner.get(INDEX_QUEUE_KEY).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn advisory_queue_io_does_not_hold_the_namespace_append_lock() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let gate = Arc::new(Semaphore::new(0));
        let blocking = Arc::new(CountingStore::blocking_queue(inner, gate.clone()));
        let store: Arc<dyn ObjectStore> = blocking.clone();
        let namespace = Arc::new(Namespace::create(store, "alpha").await.unwrap());

        let first_namespace = namespace.clone();
        let first =
            tokio::spawn(async move { first_namespace.upsert(Document::new(Id::U64(1))).await });
        blocking.wait_for_queue_gets(1).await;

        let second_namespace = namespace.clone();
        let second =
            tokio::spawn(async move { second_namespace.upsert(Document::new(Id::U64(2))).await });
        tokio::time::timeout(Duration::from_secs(1), blocking.wait_for_queue_gets(2))
            .await
            .expect("second append should commit before first queue notification finishes");

        gate.add_permits(2);
        assert_eq!(first.await.unwrap().unwrap(), WalCursor::new(0, 1));
        assert_eq!(second.await.unwrap().unwrap(), WalCursor::new(0, 2));
        assert_eq!(
            namespace.commit_cursor().await.unwrap(),
            WalCursor::new(0, 2)
        );
    }
}
