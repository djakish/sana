//! Durable indexing notifications in one object-store JSON file.
//!
//! The WAL and manifest remain authoritative. Queue jobs only prompt workers
//! to catch a namespace up to a committed WAL cursor. All state transitions
//! use compare-and-set, claims have expiring leases, and the claim attempt
//! fences a timed-out worker from completing work after another worker takes
//! over.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::indexer;
use crate::namespace::Namespace;
use crate::namespace::now_ms;
use crate::object_store::{GetResult, ObjectStore};
use crate::wal::WalCursor;

pub const INDEX_QUEUE_KEY: &str = "jobs/indexing_queue.json";
pub const INDEX_QUEUE_FORMAT_VERSION: u32 = 1;

const MAX_CAS_ATTEMPTS: usize = 64;

#[derive(Clone)]
pub struct IndexQueue {
    store: Arc<dyn ObjectStore>,
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
            if job.namespace.is_empty() {
                return Err(Error::Corrupt(format!(
                    "indexing queue job {} has an empty namespace",
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
        if namespace.is_empty() {
            return Err(Error::InvalidWrite(
                "indexing queue namespace cannot be empty".into(),
            ));
        }

        self.mutate(|queue| {
            if let Some(job) = queue
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

            let job_id = queue.next_job_id;
            queue.next_job_id = queue
                .next_job_id
                .checked_add(1)
                .ok_or_else(|| Error::Corrupt("indexing queue job id exhausted".into()))?;
            queue.jobs.push(IndexJob {
                id: job_id,
                namespace: namespace.to_string(),
                target_cursor,
                created_at_ms: timestamp_ms,
                attempts: 0,
                available_at_ms: timestamp_ms,
                claim: None,
            });
            Ok(Mutation::Changed(EnqueueOutcome::Added { job_id }))
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
        if worker_id.is_empty() {
            return Err(Error::InvalidQueueClaim("worker id cannot be empty".into()));
        }
        if lease_ms == 0 {
            return Err(Error::InvalidQueueClaim(
                "lease duration must be positive".into(),
            ));
        }

        self.mutate(|queue| {
            let active_namespaces: BTreeSet<&str> = queue
                .jobs
                .iter()
                .filter_map(|job| {
                    job.claim
                        .as_ref()
                        .filter(|claim| claim.lease_expires_at_ms > timestamp_ms)
                        .map(|_| job.namespace.as_str())
                })
                .collect();
            let Some(index) = queue.jobs.iter().position(|job| {
                job.available_at_ms <= timestamp_ms
                    && !active_namespaces.contains(job.namespace.as_str())
            }) else {
                return Ok(Mutation::Unchanged(None));
            };

            let job = &mut queue.jobs[index];
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
        })
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
        if lease_ms == 0 {
            return Err(Error::InvalidQueueClaim(
                "lease duration must be positive".into(),
            ));
        }

        self.mutate(|queue| {
            let job = matching_job_mut(queue, handle)?;
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
        })
        .await
    }

    /// Remove a successfully processed job.
    pub async fn complete(&self, handle: &ClaimHandle) -> Result<()> {
        self.mutate(|queue| {
            let index = matching_job_index(queue, handle)?;
            queue.jobs.remove(index);
            Ok(Mutation::Changed(()))
        })
        .await
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
        self.mutate(|queue| {
            let job = matching_job_mut(queue, handle)?;
            job.claim = None;
            job.available_at_ms = timestamp_ms.saturating_add(retry_delay_ms);
            Ok(Mutation::Changed(()))
        })
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
    let job = &queue.jobs[index];
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
    Ok(&mut queue.jobs[index])
}

fn stale_claim(handle: &ClaimHandle, reason: &str) -> Error {
    Error::InvalidQueueClaim(format!(
        "job {} attempt {} for worker {:?}: {reason}",
        handle.job_id, handle.attempt, handle.worker_id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::FsObjectStore;
    use crate::value::{Document, Id};

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
}
