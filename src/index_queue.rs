//! Durable indexing notifications in one object-store JSON file.
//!
//! The WAL and manifest remain authoritative. Queue jobs only prompt workers
//! to catch a namespace up to a committed WAL cursor. All state transitions
//! use compare-and-set, claims have expiring leases, and the claim attempt
//! fences a timed-out worker from completing work after another worker takes
//! over.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::backpressure::unindexed_wal_bytes;
use crate::error::{Error, Result};
use crate::indexer;
use crate::metrics::IndexLagSample;
use crate::namespace::{Namespace, now_ms, validate_namespace_name};
use crate::object_store::{GetResult, ObjectStore};
use crate::wal::WalCursor;

pub const INDEX_QUEUE_KEY: &str = "jobs/indexing_queue.json";
pub const INDEX_QUEUE_FORMAT_VERSION: u32 = 1;

const MAX_CAS_ATTEMPTS: usize = 64;

#[derive(Clone)]
pub struct IndexQueue {
    store: Arc<dyn ObjectStore>,
}

#[derive(Clone)]
pub struct IndexQueueBroker {
    sender: mpsc::Sender<BrokerRequest>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimHandle {
    pub job_id: u64,
    pub worker_id: String,
    pub attempt: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedJob {
    pub job: IndexJob,
    pub handle: ClaimHandle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    jobs: Vec<IndexJob>,
}

impl QueueFile {
    fn empty() -> Self {
        Self {
            format_version: INDEX_QUEUE_FORMAT_VERSION,
            next_job_id: 1,
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
        Ok(())
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
        Self { store }
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
        self.mutate(|queue| queue.enqueue(namespace, target_cursor, timestamp_ms))
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
        self.mutate(|queue| queue.claim(worker_id, lease_ms, timestamp_ms))
            .await
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
        self.mutate(|queue| queue.heartbeat(handle, lease_ms, timestamp_ms))
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
        self.mutate(|queue| queue.fail(handle, retry_delay_ms, timestamp_ms))
            .await
    }

    /// Read the current queue in FIFO order.
    pub async fn jobs(&self) -> Result<Vec<IndexJob>> {
        Ok(self.load_or_create().await?.1.jobs)
    }

    async fn mutate<T>(
        &self,
        mut operation: impl FnMut(&mut QueueFile) -> Result<Mutation<T>>,
    ) -> Result<T> {
        for attempt in 0..MAX_CAS_ATTEMPTS {
            let (snapshot, mut queue) = self.load_or_create().await?;
            let value = match operation(&mut queue)? {
                Mutation::Unchanged(value) => return Ok(value),
                Mutation::Changed(value) => value,
            };
            queue.validate()?;
            let bytes = Bytes::from(queue.encode()?);
            match self
                .store
                .compare_and_set(INDEX_QUEUE_KEY, snapshot.version, bytes)
                .await
            {
                Ok(_) => return Ok(value),
                Err(Error::CasMismatch { .. }) if attempt + 1 < MAX_CAS_ATTEMPTS => {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded CAS loop returns on its final attempt")
    }

    async fn mutate_batch(
        &self,
        operations: &[BrokerOperation],
    ) -> Result<Vec<Result<BrokerReply>>> {
        for attempt in 0..MAX_CAS_ATTEMPTS {
            let (snapshot, mut queue) = self.load_or_create().await?;
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
                return Ok(replies);
            }

            queue.validate()?;
            let bytes = Bytes::from(queue.encode()?);
            match self
                .store
                .compare_and_set(INDEX_QUEUE_KEY, snapshot.version, bytes)
                .await
            {
                Ok(_) => return Ok(replies),
                Err(Error::CasMismatch { .. }) if attempt + 1 < MAX_CAS_ATTEMPTS => {
                    tokio::task::yield_now().await;
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

impl BrokerOperation {
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
        let (sender, receiver) = mpsc::channel(channel_capacity.max(1));
        tokio::spawn(run_broker(
            IndexQueue::new(store),
            receiver,
            max_batch.max(1),
        ));
        Self { sender }
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
        match self
            .request(BrokerOperation::Claim {
                worker_id: worker_id.to_string(),
                lease_ms,
                timestamp_ms: now_ms(),
            })
            .await?
        {
            BrokerReply::Claimed(job) => Ok(job),
            _ => Err(unexpected_broker_reply("claim")),
        }
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

async fn run_broker(
    queue: IndexQueue,
    mut receiver: mpsc::Receiver<BrokerRequest>,
    max_batch: usize,
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

        let operations: Vec<BrokerOperation> = requests
            .iter()
            .map(|request| request.operation.clone())
            .collect();
        match queue.mutate_batch(&operations).await {
            Ok(replies) => {
                for (request, reply) in requests.into_iter().zip(replies) {
                    let _ = request.response.send(reply);
                }
            }
            Err(error) => {
                let message = error.to_string();
                for request in requests {
                    let _ = request.response.send(Err(Error::Corrupt(format!(
                        "indexing queue broker batch failed: {message}"
                    ))));
                }
            }
        }
    }
}

fn unexpected_broker_reply(operation: &str) -> Error {
    Error::Corrupt(format!(
        "indexing queue broker returned the wrong reply for {operation}"
    ))
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
    let broker = IndexQueueBroker::start(store.clone(), 1_024, 256);
    reconcile_unindexed_with_broker(store, &broker).await
}

/// Reconcile lagging namespaces through an existing broker.
pub async fn reconcile_unindexed_with_broker(
    store: Arc<dyn ObjectStore>,
    broker: &IndexQueueBroker,
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
        let broker = broker.clone();
        enqueues.spawn(async move { broker.enqueue(&namespace, commit).await });
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
    let queue = IndexQueue::new(store.clone());
    let Some(claimed) = queue.claim(worker_id, lease_ms).await? else {
        return Ok(None);
    };

    let heartbeat_every_ms = lease_ms.div_ceil(3).max(1);
    let work = execute_index_job(store, &claimed.job);
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

async fn execute_index_job(store: Arc<dyn ObjectStore>, job: &IndexJob) -> Result<bool> {
    let namespace = Namespace::open(store, &job.namespace).await?;
    let did_flush = indexer::flush(&namespace).await?;
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
    async fn completed_work_is_safe_to_repeat_after_worker_crash() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let namespace = Namespace::create(store.clone(), "alpha").await.unwrap();
        namespace.upsert(Document::new(Id::U64(1))).await.unwrap();
        let queue = IndexQueue::new(store.clone());
        let enqueued_at = queue.jobs().await.unwrap()[0].available_at_ms;

        let old = queue
            .claim_at("old-worker", 10, enqueued_at)
            .await
            .unwrap()
            .unwrap();
        assert!(execute_index_job(store.clone(), &old.job).await.unwrap());
        let expired_at = old.job.claim.as_ref().unwrap().lease_expires_at_ms;

        let replacement = queue
            .claim_at("new-worker", 10, expired_at)
            .await
            .unwrap()
            .unwrap();
        assert!(!execute_index_job(store, &replacement.job).await.unwrap());
        queue.complete(&replacement.handle).await.unwrap();
        assert!(queue.jobs().await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broker_group_commits_buffered_pushes_with_one_cas() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let store = Arc::new(CountingStore::new(inner));
        let broker = IndexQueueBroker::start(store.clone(), 64, 64);
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
