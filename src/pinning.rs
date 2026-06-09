//! Durable namespace pinning and read-replica leases.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::cache_warm::{CacheWarmOptions, CacheWarmReport};
use crate::error::{Error, Result};
use crate::namespace::{Namespace, manifest_pointer_key, now_ms, validate_namespace_name};
use crate::object_store::{GetResult, ObjectStore};

pub const PINNING_FORMAT_VERSION: u32 = 1;
const MAX_CAS_ATTEMPTS: usize = 64;
const MAX_REPLICAS: u32 = 1_024;

pub fn pinning_key(namespace: &str) -> String {
    format!("namespaces/{namespace}/routing/pinning.json")
}

#[derive(Clone)]
pub struct PinningController {
    store: Arc<dyn ObjectStore>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaStatus {
    Warming,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplicaAssignment {
    pub node_id: String,
    pub assignment_id: u64,
    pub status: ReplicaStatus,
    pub manifest_generation: u64,
    pub lease_expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaClaim {
    pub namespace: String,
    pub slot: u32,
    pub node_id: String,
    pub assignment_id: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PinningMetadata {
    pub replicas: u32,
    pub assigned_replicas: usize,
    pub ready_replicas: usize,
    pub average_utilization: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaRoute {
    pub slot: u32,
    pub node_id: String,
    pub assignment_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct PinningFile {
    format_version: u32,
    revision: u64,
    next_assignment_id: u64,
    replicas: u32,
    assignments: BTreeMap<u32, ReplicaAssignment>,
}

impl PinningFile {
    fn empty() -> Self {
        Self {
            format_version: PINNING_FORMAT_VERSION,
            revision: 0,
            next_assignment_id: 1,
            replicas: 0,
            assignments: BTreeMap::new(),
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
        if self.format_version != PINNING_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported pinning format version {}",
                self.format_version
            )));
        }
        if self.replicas > MAX_REPLICAS {
            return Err(Error::Corrupt(format!(
                "pinning replicas {} exceeds maximum {MAX_REPLICAS}",
                self.replicas
            )));
        }
        let mut assignment_ids = BTreeSet::new();
        let mut node_ids = BTreeSet::new();
        let mut max_assignment_id = 0;
        for (slot, assignment) in &self.assignments {
            if *slot >= self.replicas {
                return Err(Error::Corrupt(format!(
                    "pinning slot {slot} is outside configured replica count {}",
                    self.replicas
                )));
            }
            if assignment.node_id.is_empty()
                || assignment.assignment_id == 0
                || !assignment_ids.insert(assignment.assignment_id)
                || !node_ids.insert(assignment.node_id.as_str())
            {
                return Err(Error::Corrupt(format!(
                    "invalid pinning assignment in slot {slot}"
                )));
            }
            if assignment
                .utilization
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            {
                return Err(Error::Corrupt(format!(
                    "invalid pinning utilization in slot {slot}"
                )));
            }
            max_assignment_id = max_assignment_id.max(assignment.assignment_id);
        }
        if self.next_assignment_id <= max_assignment_id {
            return Err(Error::Corrupt(format!(
                "next pinning assignment id {} is not above live max {}",
                self.next_assignment_id, max_assignment_id
            )));
        }
        Ok(())
    }

    fn remove_expired(&mut self, timestamp_ms: u64) -> bool {
        let old_len = self.assignments.len();
        self.assignments
            .retain(|_, assignment| assignment.lease_expires_at_ms > timestamp_ms);
        self.assignments.len() != old_len
    }
}

enum Mutation<T> {
    Unchanged(T),
    Changed(T),
}

impl PinningController {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    /// Enable or resize pinning. `None` disables pinning and clears assignments.
    pub async fn configure(&self, namespace: &str, replicas: Option<u32>) -> Result<()> {
        self.ensure_namespace(namespace).await?;
        let replicas = match replicas {
            Some(0) => {
                return Err(Error::InvalidWrite(format!(
                    "pinning replicas must be between 1 and {MAX_REPLICAS}"
                )));
            }
            Some(replicas) if replicas > MAX_REPLICAS => {
                return Err(Error::InvalidWrite(format!(
                    "pinning replicas must be between 1 and {MAX_REPLICAS}"
                )));
            }
            Some(replicas) => replicas,
            None => 0,
        };
        self.mutate(namespace, |state| {
            if state.replicas == replicas && (replicas != 0 || state.assignments.is_empty()) {
                return Ok(Mutation::Unchanged(()));
            }
            state.replicas = replicas;
            state.assignments.retain(|slot, _| *slot < replicas);
            if replicas == 0 {
                state.assignments.clear();
            }
            Ok(Mutation::Changed(()))
        })
        .await
    }

    pub async fn claim(
        &self,
        namespace: &str,
        node_id: &str,
        lease_ms: u64,
    ) -> Result<Option<ReplicaClaim>> {
        self.claim_at(namespace, node_id, lease_ms, now_ms()).await
    }

    async fn claim_at(
        &self,
        namespace: &str,
        node_id: &str,
        lease_ms: u64,
        timestamp_ms: u64,
    ) -> Result<Option<ReplicaClaim>> {
        self.ensure_namespace(namespace).await?;
        validate_claim_input(node_id, lease_ms)?;
        self.mutate(namespace, |state| {
            let removed_expired = state.remove_expired(timestamp_ms);
            if state.replicas == 0 {
                return Ok(if removed_expired {
                    Mutation::Changed(None)
                } else {
                    Mutation::Unchanged(None)
                });
            }
            if let Some((&slot, assignment)) = state
                .assignments
                .iter_mut()
                .find(|(_, assignment)| assignment.node_id == node_id)
            {
                let new_expiry = assignment
                    .lease_expires_at_ms
                    .max(timestamp_ms.saturating_add(lease_ms));
                let changed = new_expiry != assignment.lease_expires_at_ms;
                assignment.lease_expires_at_ms = new_expiry;
                let claim = ReplicaClaim {
                    namespace: namespace.to_string(),
                    slot,
                    node_id: node_id.to_string(),
                    assignment_id: assignment.assignment_id,
                };
                return Ok(if changed || removed_expired {
                    Mutation::Changed(Some(claim))
                } else {
                    Mutation::Unchanged(Some(claim))
                });
            }
            let Some(slot) = (0..state.replicas).find(|slot| !state.assignments.contains_key(slot))
            else {
                return Ok(Mutation::Unchanged(None));
            };
            let assignment_id = state.next_assignment_id;
            state.next_assignment_id = state
                .next_assignment_id
                .checked_add(1)
                .ok_or_else(|| Error::Corrupt("pinning assignment id exhausted".into()))?;
            state.assignments.insert(
                slot,
                ReplicaAssignment {
                    node_id: node_id.to_string(),
                    assignment_id,
                    status: ReplicaStatus::Warming,
                    manifest_generation: 0,
                    lease_expires_at_ms: timestamp_ms.saturating_add(lease_ms),
                    utilization: None,
                },
            );
            Ok(Mutation::Changed(Some(ReplicaClaim {
                namespace: namespace.to_string(),
                slot,
                node_id: node_id.to_string(),
                assignment_id,
            })))
        })
        .await
    }

    pub async fn heartbeat(
        &self,
        claim: &ReplicaClaim,
        status: ReplicaStatus,
        manifest_generation: u64,
        utilization: Option<f32>,
        lease_ms: u64,
    ) -> Result<()> {
        self.heartbeat_at(
            claim,
            status,
            manifest_generation,
            utilization,
            lease_ms,
            now_ms(),
        )
        .await
    }

    async fn heartbeat_at(
        &self,
        claim: &ReplicaClaim,
        status: ReplicaStatus,
        manifest_generation: u64,
        utilization: Option<f32>,
        lease_ms: u64,
        timestamp_ms: u64,
    ) -> Result<()> {
        validate_claim_input(&claim.node_id, lease_ms)?;
        validate_utilization(utilization)?;
        self.mutate(&claim.namespace, |state| {
            let assignment = matching_assignment_mut(state, claim)?;
            if assignment.lease_expires_at_ms <= timestamp_ms {
                return Err(stale_claim(claim, "lease has expired"));
            }
            assignment.status = status;
            assignment.manifest_generation = manifest_generation;
            assignment.utilization = utilization;
            assignment.lease_expires_at_ms = assignment
                .lease_expires_at_ms
                .max(timestamp_ms.saturating_add(lease_ms));
            Ok(Mutation::Changed(()))
        })
        .await
    }

    pub async fn release(&self, claim: &ReplicaClaim) -> Result<()> {
        self.mutate(&claim.namespace, |state| {
            matching_assignment(state, claim)?;
            state.assignments.remove(&claim.slot);
            Ok(Mutation::Changed(()))
        })
        .await
    }

    pub async fn metadata(&self, namespace: &str) -> Result<Option<PinningMetadata>> {
        self.metadata_at(namespace, now_ms()).await
    }

    async fn metadata_at(
        &self,
        namespace: &str,
        timestamp_ms: u64,
    ) -> Result<Option<PinningMetadata>> {
        self.ensure_namespace(namespace).await?;
        let Some((_, state)) = self.load(namespace).await? else {
            return Ok(None);
        };
        if state.replicas == 0 {
            return Ok(None);
        }
        let current_generation = Namespace::open(self.store.clone(), namespace)
            .await?
            .load_manifest()
            .await?
            .generation;
        let live: Vec<&ReplicaAssignment> = state
            .assignments
            .values()
            .filter(|assignment| assignment.lease_expires_at_ms > timestamp_ms)
            .collect();
        let ready: Vec<&ReplicaAssignment> = live
            .iter()
            .copied()
            .filter(|assignment| {
                assignment.status == ReplicaStatus::Ready
                    && assignment.manifest_generation == current_generation
            })
            .collect();
        let utilizations: Vec<f32> = ready
            .iter()
            .filter_map(|assignment| assignment.utilization)
            .collect();
        Ok(Some(PinningMetadata {
            replicas: state.replicas,
            assigned_replicas: live.len(),
            ready_replicas: ready.len(),
            average_utilization: (!utilizations.is_empty())
                .then(|| utilizations.iter().sum::<f32>() / utilizations.len() as f32),
        }))
    }

    pub async fn route(
        &self,
        namespace: &str,
        request_key: &[u8],
        manifest_generation: u64,
    ) -> Result<Option<ReplicaRoute>> {
        self.route_at(namespace, request_key, manifest_generation, now_ms())
            .await
    }

    async fn route_at(
        &self,
        namespace: &str,
        request_key: &[u8],
        manifest_generation: u64,
        timestamp_ms: u64,
    ) -> Result<Option<ReplicaRoute>> {
        self.ensure_namespace(namespace).await?;
        let Some((_, state)) = self.load(namespace).await? else {
            return Ok(None);
        };
        let ready: Vec<(u32, &ReplicaAssignment)> = state
            .assignments
            .iter()
            .filter(|(_, assignment)| {
                assignment.lease_expires_at_ms > timestamp_ms
                    && assignment.status == ReplicaStatus::Ready
                    && assignment.manifest_generation == manifest_generation
            })
            .map(|(slot, assignment)| (*slot, assignment))
            .collect();
        if ready.is_empty() {
            return Ok(None);
        }
        let index = route_hash(namespace, request_key) as usize % ready.len();
        let (slot, assignment) = ready[index];
        Ok(Some(ReplicaRoute {
            slot,
            node_id: assignment.node_id.clone(),
            assignment_id: assignment.assignment_id,
        }))
    }

    /// Warm the namespace through its configured cache and mark this replica
    /// ready for the exact generation captured by the warm plan.
    pub async fn warm_replica(
        &self,
        claim: &ReplicaClaim,
        namespace: &Namespace,
        options: CacheWarmOptions,
        lease_ms: u64,
    ) -> Result<CacheWarmReport> {
        if claim.namespace != namespace.name() {
            return Err(Error::InvalidPinningClaim(format!(
                "claim namespace {:?} does not match {:?}",
                claim.namespace,
                namespace.name()
            )));
        }
        validate_claim_input(&claim.node_id, lease_ms)?;
        let initial_generation = namespace.load_manifest().await?.generation;
        let heartbeat_every_ms = lease_ms.div_ceil(3).max(1);
        let warm = namespace.hint_cache_warm(options);
        tokio::pin!(warm);
        let report = loop {
            tokio::select! {
                result = &mut warm => break result?,
                () = tokio::time::sleep(Duration::from_millis(heartbeat_every_ms)) => {
                    self.heartbeat(
                        claim,
                        ReplicaStatus::Warming,
                        initial_generation,
                        None,
                        lease_ms,
                    ).await?;
                }
            }
        };
        self.heartbeat(
            claim,
            ReplicaStatus::Ready,
            report.plan.manifest_generation,
            Some(0.0),
            lease_ms,
        )
        .await?;
        Ok(report)
    }

    async fn ensure_namespace(&self, namespace: &str) -> Result<()> {
        validate_namespace_name(namespace)?;
        match self.store.get(&manifest_pointer_key(namespace)).await {
            Ok(_) => Ok(()),
            Err(Error::NotFound(_)) => Err(Error::NotFound(format!("namespace {namespace}"))),
            Err(error) => Err(error),
        }
    }

    async fn mutate<T>(
        &self,
        namespace: &str,
        mut operation: impl FnMut(&mut PinningFile) -> Result<Mutation<T>>,
    ) -> Result<T> {
        validate_namespace_name(namespace)?;
        for attempt in 0..MAX_CAS_ATTEMPTS {
            let (snapshot, mut state) = self.load_or_create(namespace).await?;
            let value = match operation(&mut state)? {
                Mutation::Unchanged(value) => return Ok(value),
                Mutation::Changed(value) => value,
            };
            state.revision = state
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Corrupt("pinning revision exhausted".into()))?;
            state.validate()?;
            match self
                .store
                .compare_and_set(
                    &pinning_key(namespace),
                    snapshot.version,
                    Bytes::from(state.encode()?),
                )
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

    async fn load(&self, namespace: &str) -> Result<Option<(GetResult, PinningFile)>> {
        match self.store.get(&pinning_key(namespace)).await {
            Ok(snapshot) => {
                let state = PinningFile::decode(&snapshot.bytes)?;
                Ok(Some((snapshot, state)))
            }
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn load_or_create(&self, namespace: &str) -> Result<(GetResult, PinningFile)> {
        loop {
            if let Some(state) = self.load(namespace).await? {
                return Ok(state);
            }
            match self
                .store
                .put_if_absent(
                    &pinning_key(namespace),
                    Bytes::from(PinningFile::empty().encode()?),
                )
                .await
            {
                Ok(_) | Err(Error::AlreadyExists(_)) => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

fn matching_assignment<'a>(
    state: &'a PinningFile,
    claim: &ReplicaClaim,
) -> Result<&'a ReplicaAssignment> {
    let Some(assignment) = state.assignments.get(&claim.slot) else {
        return Err(stale_claim(claim, "slot is no longer assigned"));
    };
    if assignment.node_id != claim.node_id || assignment.assignment_id != claim.assignment_id {
        return Err(stale_claim(claim, "slot is owned by another assignment"));
    }
    Ok(assignment)
}

fn matching_assignment_mut<'a>(
    state: &'a mut PinningFile,
    claim: &ReplicaClaim,
) -> Result<&'a mut ReplicaAssignment> {
    matching_assignment(state, claim)?;
    Ok(state
        .assignments
        .get_mut(&claim.slot)
        .expect("matching assignment exists"))
}

fn stale_claim(claim: &ReplicaClaim, reason: &str) -> Error {
    Error::InvalidPinningClaim(format!(
        "namespace {:?} slot {} assignment {} for node {:?}: {reason}",
        claim.namespace, claim.slot, claim.assignment_id, claim.node_id
    ))
}

fn validate_claim_input(node_id: &str, lease_ms: u64) -> Result<()> {
    if node_id.is_empty() {
        return Err(Error::InvalidPinningClaim("node id cannot be empty".into()));
    }
    if lease_ms == 0 {
        return Err(Error::InvalidPinningClaim(
            "lease duration must be positive".into(),
        ));
    }
    Ok(())
}

fn validate_utilization(utilization: Option<f32>) -> Result<()> {
    if utilization.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
        return Err(Error::InvalidPinningClaim(
            "utilization must be finite and between 0 and 1".into(),
        ));
    }
    Ok(())
}

fn route_hash(namespace: &str, request_key: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in namespace
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0xff))
        .chain(request_key)
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer;
    use crate::object_store::{CachingObjectStore, FsObjectStore};
    use crate::value::{Document, Id, VectorValue};

    async fn namespace(store: Arc<dyn ObjectStore>, name: &str) -> Namespace {
        Namespace::create(store, name).await.unwrap()
    }

    #[tokio::test]
    async fn ready_replicas_route_only_matching_manifest_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        namespace(store.clone(), "alpha").await;
        let controller = PinningController::new(store);
        controller.configure("alpha", Some(2)).await.unwrap();
        let first = controller
            .claim_at("alpha", "node-a", 100, 10)
            .await
            .unwrap()
            .unwrap();
        let second = controller
            .claim_at("alpha", "node-b", 100, 10)
            .await
            .unwrap()
            .unwrap();
        assert!(
            controller
                .claim_at("alpha", "node-c", 100, 10)
                .await
                .unwrap()
                .is_none()
        );

        controller
            .heartbeat_at(&first, ReplicaStatus::Ready, 0, Some(0.5), 100, 20)
            .await
            .unwrap();
        controller
            .heartbeat_at(&second, ReplicaStatus::Ready, 0, Some(0.7), 100, 20)
            .await
            .unwrap();
        let metadata = controller.metadata_at("alpha", 30).await.unwrap().unwrap();
        assert_eq!(metadata.replicas, 2);
        assert_eq!(metadata.assigned_replicas, 2);
        assert_eq!(metadata.ready_replicas, 2);
        assert!((metadata.average_utilization.unwrap() - 0.6).abs() < 1e-6);

        let route = controller
            .route_at("alpha", b"request-1", 0, 30)
            .await
            .unwrap()
            .unwrap();
        assert!(route.node_id == "node-a" || route.node_id == "node-b");
        assert_eq!(
            controller
                .route_at("alpha", b"request-1", 0, 30)
                .await
                .unwrap()
                .unwrap(),
            route
        );
        assert!(
            controller
                .route_at("alpha", b"request-1", 1, 30)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn expired_lease_is_reassigned_and_stale_node_is_fenced() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        namespace(store.clone(), "alpha").await;
        let controller = PinningController::new(store);
        controller.configure("alpha", Some(1)).await.unwrap();
        let old = controller
            .claim_at("alpha", "old", 10, 100)
            .await
            .unwrap()
            .unwrap();
        let replacement = controller
            .claim_at("alpha", "new", 10, 110)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replacement.slot, old.slot);
        assert_ne!(replacement.assignment_id, old.assignment_id);
        assert!(matches!(
            controller
                .heartbeat_at(&old, ReplicaStatus::Ready, 0, None, 10, 111)
                .await,
            Err(Error::InvalidPinningClaim(_))
        ));
        controller.release(&replacement).await.unwrap();
        assert_eq!(
            controller
                .metadata_at("alpha", 112)
                .await
                .unwrap()
                .unwrap()
                .assigned_replicas,
            0
        );
    }

    #[tokio::test]
    async fn scale_down_and_unpin_remove_excess_assignments() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        namespace(store.clone(), "alpha").await;
        let controller = PinningController::new(store);
        controller.configure("alpha", Some(3)).await.unwrap();
        for node in ["a", "b", "c"] {
            controller
                .claim_at("alpha", node, 100, 10)
                .await
                .unwrap()
                .unwrap();
        }

        controller.configure("alpha", Some(1)).await.unwrap();
        let metadata = controller.metadata_at("alpha", 20).await.unwrap().unwrap();
        assert_eq!(metadata.replicas, 1);
        assert_eq!(metadata.assigned_replicas, 1);

        controller.configure("alpha", None).await.unwrap();
        assert!(controller.metadata_at("alpha", 20).await.unwrap().is_none());
        assert!(
            controller
                .route_at("alpha", b"request", 0, 20)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_claimers_fill_each_slot_once() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        namespace(store.clone(), "alpha").await;
        let controller = PinningController::new(store);
        controller.configure("alpha", Some(4)).await.unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..16 {
            let controller = controller.clone();
            tasks.spawn(async move {
                controller
                    .claim_at("alpha", &format!("node-{index}"), 100, 10)
                    .await
            });
        }

        let mut claims = Vec::new();
        while let Some(result) = tasks.join_next().await {
            if let Some(claim) = result.unwrap().unwrap() {
                claims.push(claim);
            }
        }
        assert_eq!(claims.len(), 4);
        assert_eq!(
            claims
                .iter()
                .map(|claim| claim.slot)
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
        assert_eq!(
            claims
                .iter()
                .map(|claim| claim.assignment_id)
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
    }

    #[tokio::test]
    async fn warm_replica_marks_exact_generation_ready() {
        let dir = tempfile::tempdir().unwrap();
        let backing: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let cache = Arc::new(CachingObjectStore::new(backing, 16 * 1024 * 1024));
        let store: Arc<dyn ObjectStore> = cache.clone();
        let namespace = namespace(store.clone(), "alpha").await;
        for (id, vector) in [(1, vec![1.0, 0.0]), (2, vec![2.0, 0.0])] {
            let mut document = Document::new(Id::U64(id));
            document
                .vectors
                .insert("embedding".into(), VectorValue::F32(vector));
            namespace.upsert(document).await.unwrap();
        }
        indexer::flush(&namespace).await.unwrap();
        cache.clear().await;

        let controller = PinningController::new(store);
        controller.configure("alpha", Some(1)).await.unwrap();
        let claim = controller
            .claim("alpha", "query-1", 1_000)
            .await
            .unwrap()
            .unwrap();
        let report = controller
            .warm_replica(
                &claim,
                &namespace,
                CacheWarmOptions {
                    max_bytes: 16 * 1024 * 1024,
                    max_concurrency: 4,
                },
                1_000,
            )
            .await
            .unwrap();

        let route = controller
            .route("alpha", b"request", report.plan.manifest_generation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.node_id, "query-1");
        assert!(cache.stats().await.resident_bytes > 0);
    }

    #[tokio::test]
    async fn namespace_gc_preserves_pinning_control_state() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let namespace = namespace(store.clone(), "alpha").await;
        let controller = PinningController::new(store);
        controller.configure("alpha", Some(1)).await.unwrap();

        indexer::gc(&namespace, true).await.unwrap();
        let metadata = controller.metadata("alpha").await.unwrap().unwrap();
        assert_eq!(metadata.replicas, 1);
    }

    #[tokio::test]
    async fn pinning_rejects_invalid_namespace_before_storage_access() {
        let dir = tempfile::tempdir().unwrap();
        let object_store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let error = PinningController::new(object_store.clone())
            .configure("../invalid", Some(1))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidWrite(_)));
        assert!(object_store.list("").await.unwrap().is_empty());
    }
}
