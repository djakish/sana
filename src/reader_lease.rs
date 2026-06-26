//! Durable reader leases used by GC to keep old snapshots reachable.
//!
//! The query path writes one exact per-process object; it does not list. GC is
//! allowed to list the reader-lease prefix because it already runs in the
//! maintenance/tooling path.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::manifest::NamespaceManifest;
use crate::namespace::{manifest_body_key_for_pointer, now_ms, wal_key};
use crate::object_store::{ObjectStore, ObjectVersion};
use crate::wal::WalCursor;

pub const READER_LEASE_PREFIX: &str = "jobs/readers/";
pub const READER_LEASE_FORMAT_VERSION: u32 = 1;

const MAX_READER_LEASE_CAS_ATTEMPTS: usize = 64;

#[derive(Clone)]
pub struct ReaderLeaseController {
    inner: Arc<ReaderLeaseInner>,
}

struct ReaderLeaseInner {
    store: Arc<dyn ObjectStore>,
    owner_id: String,
    lease_ms: u64,
    state: Mutex<ReaderLeaseState>,
    persist_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Default)]
struct ReaderLeaseState {
    active: BTreeMap<ActiveReaderKey, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ActiveReaderKey {
    namespace: String,
    generation: u64,
    body_key: String,
    indexed_cursor: Option<WalCursor>,
    commit_cursor: WalCursor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderLeaseSnapshot {
    pub namespace: String,
    pub generation: u64,
    pub body_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_cursor: Option<WalCursor>,
    pub commit_cursor: WalCursor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ReaderLeaseFile {
    format_version: u32,
    revision: u64,
    owner_id: String,
    lease_expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    active: Vec<ReaderLeaseSnapshot>,
}

pub struct ReaderLeaseGuard {
    controller: Option<ReaderLeaseController>,
    key: Option<ActiveReaderKey>,
    stop_renewal: Option<oneshot::Sender<()>>,
    renewal: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl ReaderLeaseController {
    pub fn new(
        store: Arc<dyn ObjectStore>,
        owner_id: impl Into<String>,
        lease_ms: u64,
    ) -> Result<Self> {
        let owner_id = owner_id.into();
        validate_owner_id(&owner_id)?;
        if lease_ms == 0 {
            return Err(Error::InvalidReaderLease(
                "reader lease duration must be positive".into(),
            ));
        }
        Ok(Self {
            inner: Arc::new(ReaderLeaseInner {
                store,
                owner_id,
                lease_ms,
                state: Mutex::new(ReaderLeaseState::default()),
                persist_lock: tokio::sync::Mutex::new(()),
            }),
        })
    }

    pub fn owner_id(&self) -> &str {
        &self.inner.owner_id
    }

    pub async fn acquire_snapshot(
        &self,
        snapshot: ReaderLeaseSnapshot,
    ) -> Result<ReaderLeaseGuard> {
        snapshot.validate()?;
        let key = ActiveReaderKey::from_snapshot(&snapshot);
        self.add_active(key.clone())?;
        if let Err(error) = self.persist_active().await {
            self.remove_active(&key);
            return Err(error);
        }

        let (stop_renewal, renewal_stopped) = oneshot::channel();
        let controller = self.clone();
        let renewal =
            tokio::spawn(async move { controller.renew_until_stopped(renewal_stopped).await });

        Ok(ReaderLeaseGuard {
            controller: Some(self.clone()),
            key: Some(key),
            stop_renewal: Some(stop_renewal),
            renewal: Some(renewal),
        })
    }

    async fn renew_until_stopped(&self, mut stop: oneshot::Receiver<()>) -> Result<()> {
        let heartbeat_every_ms = self.inner.lease_ms.div_ceil(3).max(1);
        loop {
            tokio::select! {
                _ = &mut stop => return Ok(()),
                () = tokio::time::sleep(Duration::from_millis(heartbeat_every_ms)) => {
                    self.persist_active().await?;
                }
            }
        }
    }

    fn add_active(&self, key: ActiveReaderKey) -> Result<()> {
        let mut state = self.lock_state()?;
        *state.active.entry(key).or_insert(0) += 1;
        Ok(())
    }

    fn remove_active(&self, key: &ActiveReaderKey) {
        let Ok(mut state) = self.inner.state.lock() else {
            return;
        };
        let Some(count) = state.active.get_mut(key) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            state.active.remove(key);
        }
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, ReaderLeaseState>> {
        self.inner.state.lock().map_err(|error| {
            Error::Corrupt(format!("reader lease state lock was poisoned: {error}"))
        })
    }

    async fn persist_active(&self) -> Result<()> {
        let _persist = self.inner.persist_lock.lock().await;
        let active = {
            let state = self.lock_state()?;
            state
                .active
                .keys()
                .map(ActiveReaderKey::to_snapshot)
                .collect::<Vec<_>>()
        };
        self.write_active_file(active).await
    }

    async fn write_active_file(&self, active: Vec<ReaderLeaseSnapshot>) -> Result<()> {
        let key = reader_lease_key(&self.inner.owner_id);
        for _ in 0..MAX_READER_LEASE_CAS_ATTEMPTS {
            match self.inner.store.get(&key).await {
                Ok(got) => {
                    let mut file = decode_reader_lease_file(&got.bytes)?;
                    if file.owner_id != self.inner.owner_id {
                        return Err(Error::Corrupt(format!(
                            "reader lease key {key} is owned by {:?}, expected {:?}",
                            file.owner_id, self.inner.owner_id
                        )));
                    }
                    file.revision = file
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| Error::Corrupt("reader lease revision overflow".into()))?;
                    file.lease_expires_at_ms = now_ms()
                        .checked_add(self.inner.lease_ms)
                        .ok_or_else(|| Error::Corrupt("reader lease expiry overflow".into()))?;
                    file.active = active.clone();
                    let encoded = encode_reader_lease_file(&file)?;
                    match self
                        .inner
                        .store
                        .compare_and_set(&key, got.version, Bytes::from(encoded))
                        .await
                    {
                        Ok(_) => return Ok(()),
                        Err(Error::CasMismatch { .. }) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(Error::NotFound(_)) => {
                    let file = ReaderLeaseFile {
                        format_version: READER_LEASE_FORMAT_VERSION,
                        revision: 0,
                        owner_id: self.inner.owner_id.clone(),
                        lease_expires_at_ms: now_ms()
                            .checked_add(self.inner.lease_ms)
                            .ok_or_else(|| Error::Corrupt("reader lease expiry overflow".into()))?,
                        active: active.clone(),
                    };
                    let encoded = encode_reader_lease_file(&file)?;
                    match self
                        .inner
                        .store
                        .put_if_absent(&key, Bytes::from(encoded))
                        .await
                    {
                        Ok(_) => return Ok(()),
                        Err(Error::AlreadyExists(_)) => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(Error::CasMismatch {
            key,
            expected: ObjectVersion("reader-lease-cas-attempts".into()),
            actual: None,
        })
    }
}

impl ReaderLeaseGuard {
    pub async fn release(mut self) -> Result<()> {
        self.stop_renewal();
        if let Some(renewal) = self.renewal.take() {
            let _ = renewal.await;
        }
        let Some(controller) = self.controller.take() else {
            return Ok(());
        };
        let Some(key) = self.key.take() else {
            return Ok(());
        };
        controller.remove_active(&key);
        controller.persist_active().await
    }

    fn stop_renewal(&mut self) {
        if let Some(stop) = self.stop_renewal.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for ReaderLeaseGuard {
    fn drop(&mut self) {
        self.stop_renewal();
        let Some(controller) = self.controller.take() else {
            return;
        };
        let Some(key) = self.key.take() else {
            return;
        };
        controller.remove_active(&key);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = controller.persist_active().await;
            });
        }
    }
}

impl ActiveReaderKey {
    fn from_snapshot(snapshot: &ReaderLeaseSnapshot) -> Self {
        Self {
            namespace: snapshot.namespace.clone(),
            generation: snapshot.generation,
            body_key: snapshot.body_key.clone(),
            indexed_cursor: snapshot.indexed_cursor,
            commit_cursor: snapshot.commit_cursor,
        }
    }

    fn to_snapshot(&self) -> ReaderLeaseSnapshot {
        ReaderLeaseSnapshot {
            namespace: self.namespace.clone(),
            generation: self.generation,
            body_key: self.body_key.clone(),
            indexed_cursor: self.indexed_cursor,
            commit_cursor: self.commit_cursor,
        }
    }
}

impl ReaderLeaseSnapshot {
    fn validate(&self) -> Result<()> {
        if self.namespace.is_empty() {
            return Err(Error::InvalidReaderLease(
                "reader lease namespace cannot be empty".into(),
            ));
        }
        if self.body_key.is_empty() {
            return Err(Error::InvalidReaderLease(
                "reader lease manifest body key cannot be empty".into(),
            ));
        }
        let expected_prefix = format!("namespaces/{}/manifest/g/", self.namespace);
        if !self.body_key.starts_with(&expected_prefix) {
            return Err(Error::InvalidReaderLease(format!(
                "reader lease body key {:?} does not belong to namespace {:?}",
                self.body_key, self.namespace
            )));
        }
        if let Some(indexed) = self.indexed_cursor {
            validate_overlay_range(indexed, self.commit_cursor)?;
        }
        Ok(())
    }
}

pub(crate) fn snapshot_from_manifest(
    namespace: &str,
    pointer: &crate::manifest::ManifestPointer,
    manifest: &NamespaceManifest,
    commit_cursor: WalCursor,
) -> ReaderLeaseSnapshot {
    ReaderLeaseSnapshot {
        namespace: namespace.to_string(),
        generation: manifest.generation,
        body_key: manifest_body_key_for_pointer(namespace, pointer),
        indexed_cursor: manifest.indexed_cursor,
        commit_cursor,
    }
}

pub fn default_reader_owner_id(role: &str) -> String {
    for variable in ["POD_NAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
        {
            return format!("{role}-{value}-{}", std::process::id());
        }
    }
    format!("{role}-{}", std::process::id())
}

pub(crate) async fn active_reader_references(
    store: &Arc<dyn ObjectStore>,
    namespace: &str,
) -> Result<BTreeSet<String>> {
    let now = now_ms();
    let leases = store.list(READER_LEASE_PREFIX).await?;
    let mut live = BTreeSet::new();
    for object in leases {
        let got = store.get(&object.key).await?;
        let file = decode_reader_lease_file(&got.bytes)?;
        if file.lease_expires_at_ms <= now {
            continue;
        }
        for snapshot in file
            .active
            .iter()
            .filter(|snapshot| snapshot.namespace == namespace)
        {
            snapshot.validate()?;
            live.insert(snapshot.body_key.clone());
            let body = store.get(&snapshot.body_key).await?;
            let manifest = NamespaceManifest::decode(&body.bytes)?;
            if manifest.namespace != namespace || manifest.generation != snapshot.generation {
                return Err(Error::Corrupt(format!(
                    "reader lease for {namespace} generation {} points at manifest {} generation {}",
                    snapshot.generation, manifest.namespace, manifest.generation
                )));
            }
            live.extend(manifest.referenced_index_keys());
            live.extend(reader_overlay_wal_keys(snapshot)?);
        }
    }
    Ok(live)
}

fn reader_overlay_wal_keys(snapshot: &ReaderLeaseSnapshot) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    let from = match snapshot.indexed_cursor {
        Some(indexed) => {
            validate_overlay_range(indexed, snapshot.commit_cursor)?;
            if indexed == snapshot.commit_cursor {
                None
            } else {
                Some(indexed.seq.checked_add(1).ok_or_else(|| {
                    Error::Corrupt("reader lease indexed WAL sequence overflow".into())
                })?)
            }
        }
        None => Some(1),
    };
    if let Some(from) = from {
        for seq in from..=snapshot.commit_cursor.seq {
            keys.insert(wal_key(
                &snapshot.namespace,
                WalCursor::new(snapshot.commit_cursor.epoch, seq),
            ));
        }
    }
    Ok(keys)
}

fn validate_overlay_range(indexed: WalCursor, commit: WalCursor) -> Result<()> {
    if indexed > commit {
        return Err(Error::InvalidReaderLease(format!(
            "reader lease indexed cursor {indexed:?} is ahead of commit cursor {commit:?}"
        )));
    }
    if indexed.epoch != commit.epoch {
        return Err(Error::InvalidReaderLease(format!(
            "reader lease crosses unsupported WAL epoch boundary {} -> {}",
            indexed.epoch, commit.epoch
        )));
    }
    Ok(())
}

fn reader_lease_key(owner_id: &str) -> String {
    format!("{}{}.json", READER_LEASE_PREFIX, hex(owner_id.as_bytes()))
}

fn encode_reader_lease_file(file: &ReaderLeaseFile) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(file).map_err(|error| Error::Codec(error.to_string()))
}

fn decode_reader_lease_file(bytes: &[u8]) -> Result<ReaderLeaseFile> {
    let file: ReaderLeaseFile =
        serde_json::from_slice(bytes).map_err(|error| Error::Codec(error.to_string()))?;
    if file.format_version != READER_LEASE_FORMAT_VERSION {
        return Err(Error::Corrupt(format!(
            "unsupported reader lease format version {}",
            file.format_version
        )));
    }
    validate_owner_id(&file.owner_id)?;
    for snapshot in &file.active {
        snapshot.validate()?;
    }
    Ok(file)
}

fn validate_owner_id(owner_id: &str) -> Result<()> {
    if owner_id.is_empty() {
        return Err(Error::InvalidReaderLease(
            "reader lease owner id cannot be empty".into(),
        ));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}
