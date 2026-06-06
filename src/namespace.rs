//! Namespace lifecycle over the object store: create, append WAL batches,
//! CAS-advance the commit cursor, and serve reads.
//!
//! The write path advances a lightweight `wal_commit/current` cursor (not the
//! full manifest) on every commit, keeping write durability separate from
//! indexing freshness (architecture Principle 2). The manifest changes only
//! when indexing publishes SST files (see `indexer`).
//!
//! Reads merge two layers: document SSTs named by the manifest (newest-first,
//! tombstone hides older) as a base, then the recent-WAL overlay after the
//! manifest's `indexed_cursor` applied on top. With no SSTs yet the base is
//! empty and reads are pure WAL replay (Stage 1 behavior).
//!
//! Concurrency model: an in-process append lock avoids needless local races,
//! while the durable commit state reserves one staged WAL at a time with CAS.
//! A writer that encounters a pending reservation finishes it before reserving
//! another sequence, so crashes and concurrent namespace handles cannot lose or
//! overwrite an accepted batch. The filesystem backend's CAS remains
//! single-process only (D4); S3/GCS conditional writes provide cross-node CAS.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::doc::{DocRecord, decode_id, encode_id};
use crate::error::{Error, Result};
use crate::index_queue::{EnqueueOutcome, IndexQueue};
use crate::manifest::{ManifestPointer, NamespaceManifest};
use crate::object_store::{ObjectStore, ObjectVersion, version_of};
use crate::query::{MultiQuery, MultiQueryResult, Query, QueryResult, RecallRequest, RecallResult};
use crate::sst::SstReader;
use crate::value::{Document, Id, Value};
use crate::wal::{WalBatch, WalCursor, WalOp};

const WAL_COMMIT_FORMAT_VERSION: u32 = 1;
const IDEMPOTENCY_FORMAT_VERSION: u32 = 1;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;

pub(crate) fn manifest_pointer_key(ns: &str) -> String {
    format!("namespaces/{ns}/manifest/current")
}

pub(crate) fn manifest_body_key(ns: &str, generation: u64) -> String {
    format!("namespaces/{ns}/manifest/g/{generation}.json")
}

pub(crate) fn manifest_content_body_key(
    ns: &str,
    generation: u64,
    version: &ObjectVersion,
) -> String {
    format!("namespaces/{ns}/manifest/g/{generation}-{}.json", version.0)
}

pub(crate) fn manifest_body_key_for_pointer(ns: &str, pointer: &ManifestPointer) -> String {
    pointer
        .body_key
        .clone()
        .unwrap_or_else(|| manifest_body_key(ns, pointer.generation))
}

pub(crate) fn wal_commit_key(ns: &str) -> String {
    format!("namespaces/{ns}/wal_commit/current")
}

pub(crate) fn wal_key(ns: &str, cursor: WalCursor) -> String {
    format!("namespaces/{ns}/wal/{}/{}.wal", cursor.epoch, cursor.seq)
}

pub(crate) fn idempotency_prefix(ns: &str) -> String {
    format!("namespaces/{ns}/idempotency/")
}

fn idempotency_key_path(ns: &str, key: &str) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(key.len().saturating_mul(2));
    for byte in key.as_bytes() {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("{}{encoded}.json", idempotency_prefix(ns))
}

fn wal_staging_key(ns: &str, cursor: WalCursor, version: &ObjectVersion) -> String {
    format!(
        "namespaces/{ns}/wal_staging/{}/{}-{}.wal",
        cursor.epoch, cursor.seq, version.0
    )
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RequestFingerprint {
    version: ObjectVersion,
    size_bytes: u64,
    crc32: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PendingWalCommit {
    cursor: WalCursor,
    staging_key: String,
    staging_version: ObjectVersion,
    request_fingerprint: RequestFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WalCommitState {
    format_version: u32,
    committed: WalCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PendingWalCommit>,
}

impl WalCommitState {
    fn new(committed: WalCursor) -> Self {
        Self {
            format_version: WAL_COMMIT_FORMAT_VERSION,
            committed,
            pending: None,
        }
    }

    fn validate(&self, namespace: &str) -> Result<()> {
        if self.format_version != WAL_COMMIT_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported WAL commit format version {}",
                self.format_version
            )));
        }
        if let Some(pending) = &self.pending {
            let next_seq = self.committed.seq.checked_add(1).ok_or_else(|| {
                Error::Corrupt("WAL sequence exhausted with pending write".into())
            })?;
            if pending.cursor != WalCursor::new(self.committed.epoch, next_seq) {
                return Err(Error::Corrupt(
                    "pending WAL cursor does not follow committed cursor".into(),
                ));
            }
            let staging_prefix = format!(
                "namespaces/{namespace}/wal_staging/{}/{}-",
                pending.cursor.epoch, pending.cursor.seq
            );
            let staging_suffix = format!("-{}.wal", pending.staging_version.0);
            if !pending.staging_key.starts_with(&staging_prefix)
                || !pending.staging_key.ends_with(&staging_suffix)
            {
                return Err(Error::Corrupt(
                    "pending WAL staging key does not match its cursor".into(),
                ));
            }
            if let Some(key) = &pending.idempotency_key
                && (key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES)
            {
                return Err(Error::Corrupt(
                    "pending WAL has an invalid idempotency key".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredWalCommit {
    State(WalCommitState),
    Legacy(WalCursor),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct IdempotencyRecord {
    format_version: u32,
    key: String,
    request_fingerprint: RequestFingerprint,
    cursor: WalCursor,
}

fn encode_commit_state(state: &WalCommitState) -> Result<Vec<u8>> {
    serde_json::to_vec(state).map_err(|error| Error::Codec(error.to_string()))
}

fn decode_commit_state(namespace: &str, bytes: &[u8]) -> Result<WalCommitState> {
    let stored: StoredWalCommit =
        serde_json::from_slice(bytes).map_err(|error| Error::Codec(error.to_string()))?;
    let state = match stored {
        StoredWalCommit::State(state) => state,
        StoredWalCommit::Legacy(cursor) => WalCommitState::new(cursor),
    };
    state.validate(namespace)?;
    Ok(state)
}

fn validate_idempotency_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        return Err(Error::InvalidWrite(format!(
            "idempotency key must contain 1..={MAX_IDEMPOTENCY_KEY_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn request_fingerprint(operations: &[WalOp]) -> Result<RequestFingerprint> {
    let encoded =
        postcard::to_allocvec(operations).map_err(|error| Error::Codec(error.to_string()))?;
    Ok(RequestFingerprint {
        version: version_of(&encoded),
        size_bytes: u64::try_from(encoded.len())
            .map_err(|_| Error::InvalidWrite("write batch exceeds u64 size".into()))?,
        crc32: crc32fast::hash(&encoded),
    })
}

pub(crate) async fn put_immutable_if_absent(
    store: &Arc<dyn ObjectStore>,
    key: &str,
    bytes: Bytes,
) -> Result<()> {
    match store.put_if_absent(key, bytes.clone()).await {
        Ok(_) => Ok(()),
        Err(Error::AlreadyExists(_)) => {
            let existing = store.get(key).await?;
            if existing.bytes == bytes {
                Ok(())
            } else {
                Err(Error::Corrupt(format!(
                    "immutable object key has conflicting bytes at {key}"
                )))
            }
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn op_id(op: &WalOp) -> &Id {
    match op {
        WalOp::Upsert { id, .. } | WalOp::Patch { id, .. } | WalOp::Delete { id } => id,
    }
}

pub struct Namespace {
    store: Arc<dyn ObjectStore>,
    name: String,
    append_lock: tokio::sync::Mutex<()>,
}

/// Pointer object version (to CAS against), the current pointer, and the body.
pub(crate) struct ManifestSnapshot {
    pub pointer_version: ObjectVersion,
    pub pointer: ManifestPointer,
    pub manifest: NamespaceManifest,
    pub body_size_bytes: u64,
}

impl std::fmt::Debug for Namespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Namespace")
            .field("name", &self.name)
            .finish()
    }
}

impl Namespace {
    /// Create a brand-new namespace. Errors if it already exists.
    pub async fn create(store: Arc<dyn ObjectStore>, name: &str) -> Result<Self> {
        Self::create_from_manifest(store, name, NamespaceManifest::new(name, now_ms())).await
    }

    pub(crate) async fn create_from_manifest(
        store: Arc<dyn ObjectStore>,
        name: &str,
        manifest: NamespaceManifest,
    ) -> Result<Self> {
        if manifest.namespace != name || manifest.generation != 0 {
            return Err(Error::Corrupt(format!(
                "initial manifest for namespace {name:?} must use matching name and generation 0"
            )));
        }
        let pointer_key = manifest_pointer_key(name);
        match store.get(&pointer_key).await {
            Ok(_) => return Err(Error::AlreadyExists(format!("namespace {name}"))),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }

        let encoded_manifest = manifest.encode()?;
        let body_version = version_of(&encoded_manifest);
        let body_key = manifest_content_body_key(name, 0, &body_version);
        put_immutable_if_absent(&store, &body_key, Bytes::from(encoded_manifest)).await?;

        let cursor_key = wal_commit_key(name);
        let encoded_cursor = encode_commit_state(&WalCommitState::new(WalCursor::new(0, 0)))?;
        put_immutable_if_absent(&store, &cursor_key, Bytes::from(encoded_cursor)).await?;

        // The pointer is the existence sentinel; create it last and atomically.
        match store
            .put_if_absent(
                &pointer_key,
                Bytes::from(ManifestPointer::for_body(0, body_key).encode()?),
            )
            .await
        {
            Ok(_) => {}
            Err(Error::AlreadyExists(_)) => {
                return Err(Error::AlreadyExists(format!("namespace {name}")));
            }
            Err(error) => return Err(error),
        }

        Ok(Self::handle(store, name))
    }

    /// Open an existing namespace. Errors with `NotFound` if it does not exist.
    pub async fn open(store: Arc<dyn ObjectStore>, name: &str) -> Result<Self> {
        match store.get(&manifest_pointer_key(name)).await {
            Ok(_) => Ok(Self::handle(store, name)),
            Err(Error::NotFound(_)) => Err(Error::NotFound(format!("namespace {name}"))),
            Err(e) => Err(e),
        }
    }

    pub async fn create_or_open(store: Arc<dyn ObjectStore>, name: &str) -> Result<Self> {
        match Self::create(store.clone(), name).await {
            Ok(ns) => Ok(ns),
            Err(Error::AlreadyExists(_)) => Self::open(store, name).await,
            Err(e) => Err(e),
        }
    }

    fn handle(store: Arc<dyn ObjectStore>, name: &str) -> Self {
        Self {
            store,
            name: name.to_string(),
            append_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    pub(crate) async fn load_sst(&self, key: &str) -> Result<SstReader> {
        SstReader::open(self.store.get(key).await?.bytes)
    }

    /// Collect WAL operations after `from` (exclusive) up to and including `to`,
    /// in commit order. This is the recent-write overlay merged on top of SSTs.
    ///
    /// The WAL keys are all known up front, so the GETs are issued concurrently
    /// and the results re-ordered by sequence (the overlay is expected to be
    /// small — bounded by how far indexing lags writes).
    pub(crate) async fn read_overlay_ops(
        &self,
        from: Option<WalCursor>,
        to: WalCursor,
    ) -> Result<Vec<WalOp>> {
        let start = from.map(|c| c.seq + 1).unwrap_or(1);
        if start > to.seq {
            return Ok(Vec::new());
        }

        let mut set = tokio::task::JoinSet::new();
        for (idx, seq) in (start..=to.seq).enumerate() {
            let store = self.store.clone();
            let key = wal_key(&self.name, WalCursor::new(to.epoch, seq));
            set.spawn(async move {
                let got = store.get(&key).await?;
                Ok::<(usize, Vec<WalOp>), Error>((idx, WalBatch::decode(&got.bytes)?.operations))
            });
        }

        let mut slots: Vec<Option<Vec<WalOp>>> = (start..=to.seq).map(|_| None).collect();
        while let Some(res) = set.join_next().await {
            let (idx, batch_ops) =
                res.map_err(|e| Error::Corrupt(format!("overlay join error: {e}")))??;
            slots[idx] = Some(batch_ops);
        }

        let mut ops = Vec::new();
        for slot in slots {
            ops.extend(slot.expect("every overlay slot is filled exactly once"));
        }
        Ok(ops)
    }

    /// Merge all document SSTs into the newest-wins record map (one record per
    /// id, present or tombstone). Loads each SST object exactly once. Shared by
    /// full reads, flush base resolution, and compaction.
    pub(crate) async fn sst_records(
        &self,
        manifest: &NamespaceManifest,
    ) -> Result<BTreeMap<Id, DocRecord>> {
        let mut seen: BTreeMap<Id, DocRecord> = BTreeMap::new();
        for meta in &manifest.doc_ssts {
            let reader = self.load_sst(&meta.key).await?;
            for (key, value) in reader.entries()? {
                seen.entry(decode_id(&key)?)
                    .or_insert(DocRecord::decode(&value)?);
            }
        }
        Ok(seen)
    }

    /// Resolve the newest SST record for an id (point lookup, newest-first),
    /// skipping files whose `[min_id, max_id]` cannot contain it.
    ///
    /// Each probed SST is read with [`sst::ranged_get`], which fetches only the
    /// footer, index, and one block (using the manifest's `size_bytes`), rather
    /// than loading the whole object. The batch path ([`resolve_ids`]) keeps the
    /// whole-object load because it point-gets many ids against each file.
    ///
    /// [`sst::ranged_get`]: crate::sst::ranged_get
    /// [`resolve_ids`]: Self::resolve_ids
    pub(crate) async fn sst_point_get(
        &self,
        manifest: &NamespaceManifest,
        id: &Id,
    ) -> Result<Option<DocRecord>> {
        let key = encode_id(id);
        for meta in &manifest.doc_ssts {
            if let (Some(min), Some(max)) = (&meta.min_id, &meta.max_id)
                && (id < min || id > max)
            {
                continue;
            }
            if let Some(value) =
                crate::sst::ranged_get(self.store.as_ref(), &meta.key, meta.size_bytes, &key)
                    .await?
            {
                return Ok(Some(DocRecord::decode(&value)?));
            }
        }
        Ok(None)
    }

    /// Resolve a set of ids against a known manifest snapshot plus an
    /// already-read WAL overlay, in a single pass: each doc SST object is loaded
    /// once (not once per id), point-get newest-first, then the overlay is
    /// applied for the requested ids. Returns only present documents.
    ///
    /// This is the batched counterpart to [`lookup`]: a filtered query that
    /// matched N candidate ids would otherwise call `lookup` N times, and each
    /// `lookup` re-reads the manifest, the commit cursor, every doc SST, and the
    /// overlay — O(N) round trips for data the caller already holds.
    ///
    /// [`lookup`]: Self::lookup
    pub(crate) async fn resolve_ids(
        &self,
        manifest: &NamespaceManifest,
        overlay: &[WalOp],
        ids: &BTreeSet<Id>,
    ) -> Result<BTreeMap<Id, Document>> {
        let mut readers = Vec::with_capacity(manifest.doc_ssts.len());
        for meta in &manifest.doc_ssts {
            readers.push((meta, self.load_sst(&meta.key).await?));
        }

        let mut docs: BTreeMap<Id, Document> = BTreeMap::new();
        for id in ids {
            let key = encode_id(id);
            for (meta, reader) in &readers {
                if let (Some(min), Some(max)) = (&meta.min_id, &meta.max_id)
                    && (id < min || id > max)
                {
                    continue;
                }
                if let Some(value) = reader.get(&key)? {
                    // Newest-first wins; a tombstone resolves to "absent".
                    if let DocRecord::Present(doc) = DocRecord::decode(&value)? {
                        docs.insert(id.clone(), doc);
                    }
                    break;
                }
            }
        }

        for op in overlay {
            if ids.contains(op_id(op)) {
                apply_op(&mut docs, op.clone());
            }
        }
        Ok(docs)
    }

    /// Load the current manifest body via the pointer.
    pub async fn load_manifest(&self) -> Result<NamespaceManifest> {
        Ok(self.load_manifest_snapshot().await?.manifest)
    }

    pub(crate) async fn load_manifest_snapshot(&self) -> Result<ManifestSnapshot> {
        let ptr = self.store.get(&manifest_pointer_key(&self.name)).await?;
        let pointer = ManifestPointer::decode(&ptr.bytes)?;
        let body = self
            .store
            .get(&manifest_body_key_for_pointer(&self.name, &pointer))
            .await?;
        Ok(ManifestSnapshot {
            pointer_version: ptr.version,
            pointer,
            manifest: NamespaceManifest::decode(&body.bytes)?,
            body_size_bytes: body.bytes.len() as u64,
        })
    }

    pub(crate) async fn publish_manifest(
        &self,
        expected: ObjectVersion,
        manifest: &NamespaceManifest,
    ) -> Result<()> {
        let encoded = manifest.encode()?;
        let body_version = version_of(&encoded);
        let body_key = manifest_content_body_key(&self.name, manifest.generation, &body_version);
        match self
            .store
            .put_if_absent(&body_key, Bytes::from(encoded.clone()))
            .await
        {
            Ok(_) => {}
            Err(Error::AlreadyExists(_)) => {
                let got = self.store.get(&body_key).await?;
                if got.bytes.as_ref() != encoded.as_slice() {
                    return Err(Error::Corrupt(format!(
                        "manifest body key collision at {body_key}"
                    )));
                }
            }
            Err(e) => return Err(e),
        }

        self.store
            .compare_and_set(
                &manifest_pointer_key(&self.name),
                expected,
                Bytes::from(ManifestPointer::for_body(manifest.generation, body_key).encode()?),
            )
            .await?;
        Ok(())
    }

    /// The highest WAL position durably committed.
    pub async fn commit_cursor(&self) -> Result<WalCursor> {
        Ok(self.load_commit_state().await?.1.committed)
    }

    async fn load_commit_state(&self) -> Result<(crate::object_store::GetResult, WalCommitState)> {
        let got = self.store.get(&wal_commit_key(&self.name)).await?;
        let state = decode_commit_state(&self.name, &got.bytes)?;
        Ok((got, state))
    }

    pub(crate) async fn wal_gc_state(&self) -> Result<(WalCursor, Option<String>)> {
        let (_, state) = self.load_commit_state().await?;
        Ok((
            state.committed,
            state.pending.map(|pending| pending.staging_key),
        ))
    }

    /// Append one atomic batch and advance the commit cursor. Returns the
    /// committed position. An idempotency key permanently maps to the first
    /// committed request fingerprint and cursor; an exact retry returns that
    /// cursor, while a different payload is rejected.
    pub async fn append(
        &self,
        operations: Vec<WalOp>,
        idempotency_key: Option<String>,
    ) -> Result<WalCursor> {
        if operations.is_empty() {
            return Err(Error::InvalidWrite("write batch cannot be empty".into()));
        }
        if let Some(key) = &idempotency_key {
            validate_idempotency_key(key)?;
        }
        let fingerprint = request_fingerprint(&operations)?;

        let append_guard = self.append_lock.lock().await;
        let committed = self
            .append_locked(operations, idempotency_key, fingerprint)
            .await?;
        drop(append_guard);

        // Indexing jobs are advisory. A queue outage must not turn a durable
        // WAL commit into a reported write failure; reconciliation can enqueue
        // this cursor again later.
        let _ = IndexQueue::new(self.store.clone())
            .enqueue(&self.name, committed)
            .await;

        Ok(committed)
    }

    async fn append_locked(
        &self,
        operations: Vec<WalOp>,
        idempotency_key: Option<String>,
        fingerprint: RequestFingerprint,
    ) -> Result<WalCursor> {
        loop {
            let (current, state) = self.load_commit_state().await?;
            if let Some(pending) = state.pending.clone() {
                self.finish_pending(pending).await?;
                continue;
            }

            if let Some(key) = &idempotency_key
                && let Some(cursor) = self
                    .lookup_idempotency(key, &fingerprint, state.committed)
                    .await?
            {
                return Ok(cursor);
            }

            // Do not reserve a request that is already invalid against the
            // current schema. A concurrent writer must reserve the commit state
            // before publishing its schema change, so a successful reservation
            // fences schema evolution until this request commits or is aborted.
            self.validate_ops_for_current_schema(&operations).await?;

            let next_seq = state
                .committed
                .seq
                .checked_add(1)
                .ok_or_else(|| Error::InvalidWrite("WAL sequence exhausted".into()))?;
            let next = WalCursor::new(state.committed.epoch, next_seq);
            let batch = WalBatch {
                namespace: self.name.clone(),
                sequence: next.seq,
                created_at_ms: now_ms(),
                idempotency_key: idempotency_key.clone(),
                operations: operations.clone(),
            };
            let encoded = batch.encode()?;
            let staging_version = version_of(&encoded);
            let staging_key = wal_staging_key(&self.name, next, &staging_version);
            put_immutable_if_absent(&self.store, &staging_key, Bytes::from(encoded)).await?;

            let pending = PendingWalCommit {
                cursor: next,
                staging_key,
                staging_version,
                request_fingerprint: fingerprint.clone(),
                idempotency_key: idempotency_key.clone(),
            };
            let mut reserved = state;
            reserved.pending = Some(pending.clone());
            match self
                .store
                .compare_and_set(
                    &wal_commit_key(&self.name),
                    current.version,
                    Bytes::from(encode_commit_state(&reserved)?),
                )
                .await
            {
                Ok(_) => return self.finish_pending(pending).await,
                Err(Error::CasMismatch { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    async fn validate_ops_for_current_schema(&self, operations: &[WalOp]) -> Result<()> {
        let mut schema = self.load_manifest().await?.schema;
        schema.infer_and_validate_ops(operations)?;
        Ok(())
    }

    async fn finish_pending(&self, target: PendingWalCommit) -> Result<WalCursor> {
        loop {
            let (current, state) = self.load_commit_state().await?;
            if state.committed >= target.cursor {
                return Ok(target.cursor);
            }
            let Some(pending) = state.pending.clone() else {
                return Err(Error::Corrupt(
                    "pending WAL reservation disappeared before commit".into(),
                ));
            };
            if pending != target {
                return Err(Error::Corrupt(
                    "WAL commit state changed to a different pending reservation".into(),
                ));
            }

            let staged = self.store.get(&pending.staging_key).await?;
            if staged.version != pending.staging_version {
                return Err(Error::Corrupt(format!(
                    "pending WAL staging version mismatch at {}",
                    pending.staging_key
                )));
            }
            let batch = WalBatch::decode(&staged.bytes)?;
            if batch.namespace != self.name
                || batch.sequence != pending.cursor.seq
                || batch.idempotency_key != pending.idempotency_key
                || request_fingerprint(&batch.operations)? != pending.request_fingerprint
            {
                return Err(Error::Corrupt(
                    "pending WAL staging object does not match its reservation".into(),
                ));
            }

            if let Err(error) = self.evolve_schema_for_ops(&batch.operations).await {
                if matches!(error, Error::InvalidSchema(_)) {
                    let mut aborted = state;
                    aborted.pending = None;
                    let _ = self
                        .store
                        .compare_and_set(
                            &wal_commit_key(&self.name),
                            current.version,
                            Bytes::from(encode_commit_state(&aborted)?),
                        )
                        .await;
                }
                return Err(error);
            }

            self.store
                .put(&wal_key(&self.name, pending.cursor), staged.bytes.clone())
                .await?;
            if let Some(key) = &pending.idempotency_key {
                self.write_idempotency_record(key, &pending).await?;
            }

            let mut committed = state;
            committed.committed = pending.cursor;
            committed.pending = None;
            match self
                .store
                .compare_and_set(
                    &wal_commit_key(&self.name),
                    current.version,
                    Bytes::from(encode_commit_state(&committed)?),
                )
                .await
            {
                Ok(_) => return Ok(pending.cursor),
                Err(Error::CasMismatch { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    async fn lookup_idempotency(
        &self,
        key: &str,
        fingerprint: &RequestFingerprint,
        committed: WalCursor,
    ) -> Result<Option<WalCursor>> {
        let object_key = idempotency_key_path(&self.name, key);
        let got = match self.store.get(&object_key).await {
            Ok(got) => got,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        let record: IdempotencyRecord =
            serde_json::from_slice(&got.bytes).map_err(|error| Error::Codec(error.to_string()))?;
        if record.format_version != IDEMPOTENCY_FORMAT_VERSION || record.key != key {
            return Err(Error::Corrupt(format!(
                "invalid idempotency record at {object_key}"
            )));
        }
        if &record.request_fingerprint != fingerprint {
            return Err(Error::IdempotencyConflict(key.to_string()));
        }
        if record.cursor > committed {
            return Err(Error::Corrupt(format!(
                "idempotency record {object_key} points past the committed WAL"
            )));
        }
        Ok(Some(record.cursor))
    }

    async fn write_idempotency_record(&self, key: &str, pending: &PendingWalCommit) -> Result<()> {
        let object_key = idempotency_key_path(&self.name, key);
        let record = IdempotencyRecord {
            format_version: IDEMPOTENCY_FORMAT_VERSION,
            key: key.to_string(),
            request_fingerprint: pending.request_fingerprint.clone(),
            cursor: pending.cursor,
        };
        let encoded =
            serde_json::to_vec(&record).map_err(|error| Error::Codec(error.to_string()))?;
        match self
            .store
            .put_if_absent(&object_key, Bytes::from(encoded))
            .await
        {
            Ok(_) => Ok(()),
            Err(Error::AlreadyExists(_)) => {
                let existing = self.store.get(&object_key).await?;
                let existing: IdempotencyRecord = serde_json::from_slice(&existing.bytes)
                    .map_err(|error| Error::Codec(error.to_string()))?;
                if existing == record {
                    Ok(())
                } else {
                    Err(Error::Corrupt(format!(
                        "conflicting idempotency record at {object_key}"
                    )))
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Reconcile this namespace's current durable WAL cursor into the indexing
    /// queue. This is safe to call repeatedly because pending jobs coalesce.
    pub async fn enqueue_indexing(&self) -> Result<EnqueueOutcome> {
        let target = self.commit_cursor().await?;
        IndexQueue::new(self.store.clone())
            .enqueue(&self.name, target)
            .await
    }

    async fn evolve_schema_for_ops(&self, operations: &[WalOp]) -> Result<()> {
        loop {
            let snapshot = self.load_manifest_snapshot().await?;
            let mut schema = snapshot.manifest.schema.clone();
            if !schema.infer_and_validate_ops(operations)? {
                return Ok(());
            }

            let mut manifest = snapshot.manifest;
            manifest.generation = snapshot.pointer.generation + 1;
            manifest.schema = schema;
            manifest.updated_at_ms = now_ms();

            match self
                .publish_manifest(snapshot.pointer_version, &manifest)
                .await
            {
                Ok(()) => return Ok(()),
                Err(Error::CasMismatch { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn upsert(&self, document: Document) -> Result<WalCursor> {
        let id = document.id.clone();
        self.append(vec![WalOp::Upsert { id, document }], None)
            .await
    }

    pub async fn delete(&self, id: Id) -> Result<WalCursor> {
        self.append(vec![WalOp::Delete { id }], None).await
    }

    /// Materialize the full document snapshot: SST base (newest-first wins,
    /// tombstones dropped) with the recent-WAL overlay applied on top.
    pub async fn replay(&self) -> Result<BTreeMap<Id, Document>> {
        let manifest = self.load_manifest().await?;
        let commit = self.commit_cursor().await?;
        self.replay_at(&manifest, commit).await
    }

    pub(crate) async fn replay_at(
        &self,
        manifest: &NamespaceManifest,
        commit: WalCursor,
    ) -> Result<BTreeMap<Id, Document>> {
        let mut docs: BTreeMap<Id, Document> = self
            .sst_records(manifest)
            .await?
            .into_iter()
            .filter_map(|(id, rec)| match rec {
                DocRecord::Present(d) => Some((id, d)),
                DocRecord::Deleted => None,
            })
            .collect();

        for op in self
            .read_overlay_ops(manifest.indexed_cursor, commit)
            .await?
        {
            apply_op(&mut docs, op);
        }
        Ok(docs)
    }

    /// Strong primary-key lookup: SST base for the id with the overlay applied.
    /// Unlike `replay`, this uses the point-get path (`sst_point_get`): it stops
    /// at the first SST containing the id and prunes by `[min_id, max_id]`,
    /// rather than merging every SST.
    pub async fn lookup(&self, id: &Id) -> Result<Option<Document>> {
        let manifest = self.load_manifest().await?;
        let commit = self.commit_cursor().await?;

        let mut docs: BTreeMap<Id, Document> = BTreeMap::new();
        if let Some(DocRecord::Present(doc)) = self.sst_point_get(&manifest, id).await? {
            docs.insert(id.clone(), doc);
        }
        for op in self
            .read_overlay_ops(manifest.indexed_cursor, commit)
            .await?
        {
            if op_id(&op) == id {
                apply_op(&mut docs, op);
            }
        }
        Ok(docs.remove(id))
    }

    /// Execute a strong snapshot query. Stage 3 currently scans materialized
    /// documents; attribute/vector index acceleration can replace the candidate
    /// generation inside `query` without changing this entry point.
    pub async fn query(&self, query: Query) -> Result<QueryResult> {
        crate::query::execute(self, query).await
    }

    /// Execute several independent query plans against one captured manifest
    /// and WAL commit snapshot. Hybrid rank fusion stays a caller concern; this
    /// gives text/vector/attribute subqueries a consistent read timestamp.
    pub async fn multi_query(&self, query: MultiQuery) -> Result<MultiQueryResult> {
        crate::query::execute_multi(self, query).await
    }

    /// Evaluate ANN recall by comparing approximate vector search with exact
    /// search over sampled vectors from the strong snapshot.
    pub async fn recall(&self, request: RecallRequest) -> Result<RecallResult> {
        crate::query::recall(self, request).await
    }
}

pub(crate) fn apply_op(docs: &mut BTreeMap<Id, Document>, op: WalOp) {
    match op {
        WalOp::Upsert { id, document } => {
            docs.insert(id, document);
        }
        WalOp::Delete { id } => {
            docs.remove(&id);
        }
        WalOp::Patch {
            id,
            attributes,
            vectors,
        } => {
            // Patch creates-or-updates. A null attribute value clears the field.
            let doc = docs.entry(id.clone()).or_insert_with(|| Document::new(id));
            for (k, v) in attributes {
                if matches!(v, Value::Null) {
                    doc.attributes.remove(&k);
                } else {
                    doc.attributes.insert(k, v);
                }
            }
            for (k, v) in vectors {
                doc.vectors.insert(k, v);
            }
        }
    }
}
