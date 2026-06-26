//! Policy-driven background maintenance, so a single `sana serve` keeps its
//! namespaces tidy without operator cron jobs.
//!
//! One pass scans every namespace and, for fully indexed ones, runs the
//! existing maintenance primitives in priority order: full compaction when
//! run counts or vector append chains grow past the policy thresholds,
//! otherwise manifest-published vector split/merge/reassign work. Online GC is
//! disabled by default: deleting immutable objects safely in a multi-process
//! deployment still requires publisher safety points and durable GC candidates,
//! not a local timer.
//! Operators can still opt into the legacy two-pass GC while the CLI dry-run
//! remains available. Per-namespace failures are reported, not fatal — one
//! wedged namespace must not stall the fleet.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::index_queue::list_namespace_names;
use crate::indexer;
use crate::namespace::{Namespace, now_ms};
use crate::object_store::{GetResult, ObjectStore};

pub const MAINTENANCE_LEASE_KEY: &str = "jobs/maintenance_leader.json";
pub const MAINTENANCE_LEASE_FORMAT_VERSION: u32 = 1;

const MAX_LEASE_CAS_ATTEMPTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenancePolicy {
    /// Compact a namespace when its doc or attribute run count reaches this.
    pub compact_at_runs: usize,
    /// Compact when any vector column's append-delta chain reaches this.
    pub compact_at_vector_appends: usize,
    /// Execute manifest-published vector maintenance tasks.
    pub vector_maintenance: bool,
    /// Reclaim orphaned objects with legacy two-pass deferred deletion.
    ///
    /// This is intentionally off by default. It is safe only in controlled
    /// single-process/quiescent deployments; production GC needs a durable safe
    /// point over readers and publishers.
    pub gc: bool,
}

impl Default for MaintenancePolicy {
    fn default() -> Self {
        Self {
            compact_at_runs: 8,
            compact_at_vector_appends: 4,
            vector_maintenance: true,
            gc: false,
        }
    }
}

/// Cross-pass memory for opt-in deferred GC: the orphans each namespace showed
/// on the previous pass. An object is deleted only when two consecutive scans
/// agree.
#[derive(Debug, Default)]
pub struct MaintenanceState {
    gc_candidates: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone)]
pub struct MaintenanceLeaseController {
    store: Arc<dyn ObjectStore>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceLease {
    pub owner_id: String,
    pub fencing_token: u64,
    pub lease_expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MaintenanceLeaseFile {
    format_version: u32,
    revision: u64,
    next_fencing_token: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<MaintenanceLease>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// True when a leased maintenance pass found another live leader and did no
    /// work.
    pub skipped_leader_lease: bool,
    pub scanned_namespaces: usize,
    pub compacted: Vec<String>,
    pub vector_maintained: Vec<String>,
    pub gc_deleted_objects: usize,
    /// Fresh orphans observed this pass; they become deletable next pass.
    pub gc_pending_objects: usize,
    /// Per-namespace failures as `"{namespace}: {error}"`, isolated so the
    /// rest of the pass still runs.
    pub errors: Vec<String>,
}

enum Mutation<T> {
    Unchanged(T),
    Changed(T),
}

impl MaintenanceLeaseFile {
    fn empty() -> Self {
        Self {
            format_version: MAINTENANCE_LEASE_FORMAT_VERSION,
            revision: 0,
            next_fencing_token: 1,
            owner: None,
        }
    }

    fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|error| Error::Codec(error.to_string()))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let state: Self =
            serde_json::from_slice(bytes).map_err(|error| Error::Codec(error.to_string()))?;
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<()> {
        if self.format_version != MAINTENANCE_LEASE_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported maintenance lease format version {}",
                self.format_version
            )));
        }
        if self.next_fencing_token == 0 {
            return Err(Error::Corrupt(
                "maintenance lease next fencing token cannot be zero".into(),
            ));
        }
        if let Some(owner) = &self.owner {
            if owner.owner_id.is_empty() {
                return Err(Error::Corrupt(
                    "maintenance lease owner id cannot be empty".into(),
                ));
            }
            if owner.fencing_token == 0 {
                return Err(Error::Corrupt(
                    "maintenance lease owner has a zero fencing token".into(),
                ));
            }
            if self.next_fencing_token <= owner.fencing_token {
                return Err(Error::Corrupt(format!(
                    "maintenance lease next fencing token {} is not above live token {}",
                    self.next_fencing_token, owner.fencing_token
                )));
            }
        }
        Ok(())
    }
}

impl MaintenanceLeaseController {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    pub async fn claim(&self, owner_id: &str, lease_ms: u64) -> Result<Option<MaintenanceLease>> {
        self.claim_at(owner_id, lease_ms, now_ms()).await
    }

    async fn claim_at(
        &self,
        owner_id: &str,
        lease_ms: u64,
        timestamp_ms: u64,
    ) -> Result<Option<MaintenanceLease>> {
        validate_lease_input(owner_id, lease_ms)?;
        self.mutate(|state| {
            if let Some(owner) = &mut state.owner
                && owner.lease_expires_at_ms > timestamp_ms
            {
                if owner.owner_id != owner_id {
                    return Ok(Mutation::Unchanged(None));
                }
                let lease_expires_at_ms = owner
                    .lease_expires_at_ms
                    .max(timestamp_ms.saturating_add(lease_ms));
                if lease_expires_at_ms == owner.lease_expires_at_ms {
                    return Ok(Mutation::Unchanged(Some(owner.clone())));
                }
                owner.lease_expires_at_ms = lease_expires_at_ms;
                return Ok(Mutation::Changed(Some(owner.clone())));
            }

            let fencing_token = state.next_fencing_token;
            state.next_fencing_token = state
                .next_fencing_token
                .checked_add(1)
                .ok_or_else(|| Error::Corrupt("maintenance fencing token exhausted".into()))?;
            let lease = MaintenanceLease {
                owner_id: owner_id.to_string(),
                fencing_token,
                lease_expires_at_ms: timestamp_ms.saturating_add(lease_ms),
            };
            state.owner = Some(lease.clone());
            Ok(Mutation::Changed(Some(lease)))
        })
        .await
    }

    pub async fn heartbeat(
        &self,
        lease: &MaintenanceLease,
        lease_ms: u64,
    ) -> Result<MaintenanceLease> {
        self.heartbeat_at(lease, lease_ms, now_ms()).await
    }

    async fn heartbeat_at(
        &self,
        lease: &MaintenanceLease,
        lease_ms: u64,
        timestamp_ms: u64,
    ) -> Result<MaintenanceLease> {
        validate_lease_input(&lease.owner_id, lease_ms)?;
        self.mutate(|state| {
            let owner = matching_owner_mut(state, lease)?;
            if owner.lease_expires_at_ms <= timestamp_ms {
                return Err(stale_maintenance_lease(lease, "lease has expired"));
            }
            let lease_expires_at_ms = owner
                .lease_expires_at_ms
                .max(timestamp_ms.saturating_add(lease_ms));
            if lease_expires_at_ms == owner.lease_expires_at_ms {
                return Ok(Mutation::Unchanged(owner.clone()));
            }
            owner.lease_expires_at_ms = lease_expires_at_ms;
            Ok(Mutation::Changed(owner.clone()))
        })
        .await
    }

    pub async fn release(&self, lease: &MaintenanceLease) -> Result<()> {
        self.release_at(lease, now_ms()).await
    }

    async fn release_at(&self, lease: &MaintenanceLease, timestamp_ms: u64) -> Result<()> {
        self.mutate(|state| {
            let owner = matching_owner(state, lease)?;
            if owner.lease_expires_at_ms <= timestamp_ms {
                return Err(stale_maintenance_lease(lease, "lease has expired"));
            }
            state.owner = None;
            Ok(Mutation::Changed(()))
        })
        .await
    }

    async fn mutate<T>(
        &self,
        mut operation: impl FnMut(&mut MaintenanceLeaseFile) -> Result<Mutation<T>>,
    ) -> Result<T> {
        for attempt in 0..MAX_LEASE_CAS_ATTEMPTS {
            let (snapshot, mut state) = self.load_or_create().await?;
            let value = match operation(&mut state)? {
                Mutation::Unchanged(value) => return Ok(value),
                Mutation::Changed(value) => value,
            };
            state.revision = state
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Corrupt("maintenance lease revision exhausted".into()))?;
            state.validate()?;
            match self
                .store
                .compare_and_set(
                    MAINTENANCE_LEASE_KEY,
                    snapshot.version,
                    Bytes::from(state.encode()?),
                )
                .await
            {
                Ok(_) => return Ok(value),
                Err(Error::CasMismatch { .. }) if attempt + 1 < MAX_LEASE_CAS_ATTEMPTS => {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded CAS loop returns on its final attempt")
    }

    async fn load_or_create(&self) -> Result<(GetResult, MaintenanceLeaseFile)> {
        loop {
            match self.store.get(MAINTENANCE_LEASE_KEY).await {
                Ok(snapshot) => {
                    let state = MaintenanceLeaseFile::decode(&snapshot.bytes)?;
                    return Ok((snapshot, state));
                }
                Err(Error::NotFound(_)) => {
                    let bytes = Bytes::from(MaintenanceLeaseFile::empty().encode()?);
                    match self.store.put_if_absent(MAINTENANCE_LEASE_KEY, bytes).await {
                        Ok(_) | Err(Error::AlreadyExists(_)) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub fn default_maintenance_owner_id(role: &str) -> String {
    for variable in ["POD_NAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
        {
            return format!("{role}-{value}-{}", std::process::id());
        }
    }
    format!("{role}-{}", std::process::id())
}

/// Run one maintenance pass over every namespace in the store.
pub async fn run_once(
    store: Arc<dyn ObjectStore>,
    policy: &MaintenancePolicy,
    state: &mut MaintenanceState,
) -> Result<MaintenanceReport> {
    run_once_with_fence(store, policy, state, None).await
}

async fn run_once_with_fence(
    store: Arc<dyn ObjectStore>,
    policy: &MaintenancePolicy,
    state: &mut MaintenanceState,
    fence: Option<&dyn indexer::ManifestPublishFence>,
) -> Result<MaintenanceReport> {
    let names = list_namespace_names(&store).await?;
    let mut report = MaintenanceReport {
        scanned_namespaces: names.len(),
        ..MaintenanceReport::default()
    };

    for name in &names {
        let prior = state.gc_candidates.remove(name).unwrap_or_default();
        match maintain_namespace(&store, name, policy, prior, &mut report, fence).await {
            Ok(pending) => {
                if !pending.is_empty() {
                    state.gc_candidates.insert(name.clone(), pending);
                }
            }
            Err(error) => report.errors.push(format!("{name}: {error}")),
        }
    }
    // Drop candidates for namespaces that disappeared since the last pass.
    state.gc_candidates.retain(|name, _| names.contains(name));
    Ok(report)
}

/// Run one maintenance pass only if this process owns the store-global
/// maintenance lease. If another live owner exists, returns a skipped report.
pub async fn run_once_leased(
    store: Arc<dyn ObjectStore>,
    policy: &MaintenancePolicy,
    state: &mut MaintenanceState,
    owner_id: &str,
    lease_ms: u64,
) -> Result<MaintenanceReport> {
    let controller = MaintenanceLeaseController::new(store.clone());
    let Some(lease) = controller.claim(owner_id, lease_ms).await? else {
        return Ok(MaintenanceReport {
            skipped_leader_lease: true,
            ..MaintenanceReport::default()
        });
    };
    let (stop_renewal, renewal_stopped) = oneshot::channel();
    let renewal = tokio::spawn(renew_maintenance_lease(
        controller.clone(),
        lease.clone(),
        lease_ms,
        renewal_stopped,
    ));
    let fence = MaintenancePublishFence {
        controller: controller.clone(),
        lease: lease.clone(),
        lease_ms,
    };
    let report = run_once_with_fence(store, policy, state, Some(&fence)).await;
    let _ = stop_renewal.send(());
    let renewal = renewal.await.map_err(|error| {
        Error::Corrupt(format!("maintenance lease renewal task failed: {error}"))
    })?;
    let final_heartbeat = controller.heartbeat(&lease, lease_ms).await;
    match (report, renewal, final_heartbeat) {
        (Ok(report), Ok(()), Ok(_)) => Ok(report),
        (Ok(_), Err(error), _) | (Ok(_), _, Err(error)) => Err(error),
        (Err(error), _, _) => Err(error),
    }
}

async fn renew_maintenance_lease(
    controller: MaintenanceLeaseController,
    lease: MaintenanceLease,
    lease_ms: u64,
    mut stop: oneshot::Receiver<()>,
) -> Result<()> {
    let heartbeat_every_ms = lease_ms.div_ceil(3).max(1);
    loop {
        tokio::select! {
            _ = &mut stop => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(heartbeat_every_ms)) => {
                controller.heartbeat(&lease, lease_ms).await?;
            }
        }
    }
}

struct MaintenancePublishFence {
    controller: MaintenanceLeaseController,
    lease: MaintenanceLease,
    lease_ms: u64,
}

#[async_trait]
impl indexer::ManifestPublishFence for MaintenancePublishFence {
    async fn verify(&self) -> Result<()> {
        self.controller
            .heartbeat(&self.lease, self.lease_ms)
            .await?;
        Ok(())
    }
}

/// Maintain one namespace; returns the orphan keys to remember for next pass.
async fn maintain_namespace(
    store: &Arc<dyn ObjectStore>,
    name: &str,
    policy: &MaintenancePolicy,
    prior_candidates: BTreeSet<String>,
    report: &mut MaintenanceReport,
    fence: Option<&dyn indexer::ManifestPublishFence>,
) -> Result<BTreeSet<String>> {
    let ns = Namespace::open(store.clone(), name).await?;
    let manifest = ns.load_manifest().await?;
    let commit = ns.commit_cursor().await?;
    let fully_indexed = manifest.indexed_cursor == Some(commit);

    // Index-shape work only on fully indexed namespaces: the flush worker owns
    // catching up, and compaction/maintenance on a lagging namespace would just
    // be repeated. Compaction subsumes vector maintenance (it rebuilds the
    // base and clears append chains), so at most one of the two runs per pass.
    if fully_indexed {
        let needs_compaction = manifest.doc_ssts.len() >= policy.compact_at_runs
            || manifest.attr_ssts.len() >= policy.compact_at_runs
            || manifest
                .vector_indexes
                .values()
                .any(|meta| meta.append_indexes.len() >= policy.compact_at_vector_appends);
        let has_vector_tasks = manifest.vector_indexes.values().any(|meta| {
            meta.maintenance_plan
                .as_ref()
                .is_some_and(|plan| !plan.tasks.is_empty())
        });

        if needs_compaction {
            let did_compact = match fence {
                Some(fence) => indexer::compact_with_fence(&ns, fence).await?,
                None => indexer::compact(&ns).await?,
            };
            if did_compact {
                report.compacted.push(name.to_string());
            }
        } else if policy.vector_maintenance && has_vector_tasks {
            let did_maintain_vectors = match fence {
                Some(fence) => indexer::maintain_vectors_with_fence(&ns, fence).await?,
                None => indexer::maintain_vectors(&ns).await?,
            };
            if did_maintain_vectors {
                report.vector_maintained.push(name.to_string());
            }
        }
    }

    if !policy.gc {
        return Ok(BTreeSet::new());
    }

    // Deferred GC: scan now, but delete only what the *previous* pass already
    // reported orphaned. The scan after a compaction above sees the new
    // manifest, so the just-superseded runs enter the candidate set and are
    // reclaimed one interval later.
    let scan = indexer::gc(&ns, false).await?;
    let orphans: BTreeSet<String> = scan.orphan_keys.into_iter().collect();
    let delete_candidates: BTreeSet<String> =
        orphans.intersection(&prior_candidates).cloned().collect();
    if !delete_candidates.is_empty() {
        let deleted =
            indexer::delete_gc_candidates_with_fence(&ns, delete_candidates, fence).await?;
        report.gc_deleted_objects += deleted.orphan_keys.len();
    }
    let pending: BTreeSet<String> = orphans.difference(&prior_candidates).cloned().collect();
    report.gc_pending_objects += pending.len();
    Ok(pending)
}

fn matching_owner<'a>(
    state: &'a MaintenanceLeaseFile,
    lease: &MaintenanceLease,
) -> Result<&'a MaintenanceLease> {
    let Some(owner) = state.owner.as_ref() else {
        return Err(stale_maintenance_lease(lease, "no owner is registered"));
    };
    if owner.owner_id != lease.owner_id || owner.fencing_token != lease.fencing_token {
        return Err(stale_maintenance_lease(
            lease,
            "lease is owned by another process",
        ));
    }
    Ok(owner)
}

fn matching_owner_mut<'a>(
    state: &'a mut MaintenanceLeaseFile,
    lease: &MaintenanceLease,
) -> Result<&'a mut MaintenanceLease> {
    matching_owner(state, lease)?;
    Ok(state
        .owner
        .as_mut()
        .expect("matching maintenance lease owner exists"))
}

fn stale_maintenance_lease(lease: &MaintenanceLease, reason: &str) -> Error {
    Error::InvalidMaintenanceLease(format!(
        "maintenance lease token {} for owner {:?}: {reason}",
        lease.fencing_token, lease.owner_id
    ))
}

fn validate_lease_input(owner_id: &str, lease_ms: u64) -> Result<()> {
    validate_owner_id(owner_id)?;
    if lease_ms == 0 {
        return Err(Error::InvalidMaintenanceLease(
            "lease duration must be positive".into(),
        ));
    }
    Ok(())
}

fn validate_owner_id(owner_id: &str) -> Result<()> {
    if owner_id.is_empty() {
        return Err(Error::InvalidMaintenanceLease(
            "owner id cannot be empty".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::FsObjectStore;

    fn store(dir: &tempfile::TempDir) -> Arc<dyn ObjectStore> {
        Arc::new(FsObjectStore::new(dir.path()))
    }

    #[tokio::test]
    async fn maintenance_lease_is_exclusive_until_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let controller = MaintenanceLeaseController::new(store(&dir));

        let first = controller.claim_at("a", 100, 10).await.unwrap().unwrap();
        assert_eq!(first.fencing_token, 1);
        assert_eq!(first.lease_expires_at_ms, 110);
        assert!(controller.claim_at("b", 100, 20).await.unwrap().is_none());

        let renewed = controller.claim_at("a", 100, 30).await.unwrap().unwrap();
        assert_eq!(renewed.fencing_token, first.fencing_token);
        assert_eq!(renewed.lease_expires_at_ms, 130);

        let replacement = controller.claim_at("b", 100, 130).await.unwrap().unwrap();
        assert_eq!(replacement.fencing_token, 2);
        assert_eq!(replacement.owner_id, "b");
    }

    #[tokio::test]
    async fn stale_maintenance_owner_cannot_heartbeat_or_release() {
        let dir = tempfile::tempdir().unwrap();
        let controller = MaintenanceLeaseController::new(store(&dir));

        let old = controller.claim_at("old", 10, 100).await.unwrap().unwrap();
        assert!(matches!(
            controller.release_at(&old, 110).await,
            Err(Error::InvalidMaintenanceLease(_))
        ));
        let replacement = controller.claim_at("new", 10, 110).await.unwrap().unwrap();

        assert!(matches!(
            controller.heartbeat_at(&old, 10, 111).await,
            Err(Error::InvalidMaintenanceLease(_))
        ));
        assert!(matches!(
            controller.release_at(&old, 111).await,
            Err(Error::InvalidMaintenanceLease(_))
        ));
        controller
            .heartbeat_at(&replacement, 10, 111)
            .await
            .unwrap();
        controller.release_at(&replacement, 112).await.unwrap();
    }

    #[tokio::test]
    async fn leased_maintenance_skips_when_another_leader_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let controller = MaintenanceLeaseController::new(store.clone());
        controller.claim("leader", 10_000).await.unwrap().unwrap();

        let mut state = MaintenanceState::default();
        let report = run_once_leased(
            store,
            &MaintenancePolicy::default(),
            &mut state,
            "other",
            10_000,
        )
        .await
        .unwrap();

        assert!(report.skipped_leader_lease);
        assert_eq!(report.scanned_namespaces, 0);
    }
}
