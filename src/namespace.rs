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

use crate::backpressure::{enforce_limit, unindexed_wal_bytes};
use crate::doc::{DocRecord, decode_id, encode_id};
use crate::error::{Error, Result};
use crate::index_queue::{EnqueueOutcome, IndexQueue};
use crate::manifest::{ManifestPointer, NamespaceManifest};
use crate::object_store::{
    GetResult, ObjectStore, ObjectVersion, legacy_version_of, version_matches, version_of,
};
use crate::query::{
    MultiQuery, MultiQueryResult, Query, QueryOptions, QueryResult, RecallRequest, RecallResult,
};
use crate::sst::SstReader;
use crate::value::{Document, Id, Value};
use crate::wal::{WalBatch, WalCursor, WalOp};
use crate::write::{
    ConditionalWriteOp, ConditionalWriteResult, DeleteByFilterRequest, PatchByFilterRequest,
    WriteOptions, WriteOutcome,
};

const WAL_COMMIT_FORMAT_VERSION: u32 = 1;
const IDEMPOTENCY_FORMAT_VERSION: u32 = 1;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const MAX_NAMESPACE_NAME_BYTES: usize = 128;

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

fn conditional_outcome_key(
    ns: &str,
    cursor: WalCursor,
    idempotency_key: Option<&str>,
    version: &ObjectVersion,
) -> String {
    match idempotency_key {
        Some(key) => {
            let record_key = idempotency_key_path(ns, key);
            format!(
                "{}.outcome-{}.json",
                record_key.trim_end_matches(".json"),
                version.0
            )
        }
        None => format!(
            "namespaces/{ns}/wal_staging/{}/{}-outcome-{}.json",
            cursor.epoch, cursor.seq, version.0
        ),
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_version: Option<ObjectVersion>,
    size_bytes: u64,
    crc32: u32,
}

impl RequestFingerprint {
    fn matches(&self, other: &Self) -> bool {
        self.size_bytes == other.size_bytes
            && self.crc32 == other.crc32
            && (self.version == other.version
                || self.legacy_version.as_ref() == Some(&other.version)
                || other.legacy_version.as_ref() == Some(&self.version))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ImmutableObjectRef {
    key: String,
    version: ObjectVersion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PendingWalCommit {
    cursor: WalCursor,
    staging_key: String,
    staging_version: ObjectVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wal_size_bytes: Option<u64>,
    request_fingerprint: RequestFingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wal_fingerprint: Option<RequestFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<String>,
    #[serde(default)]
    idempotency_kind: IdempotencyKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conditional_outcome: Option<ImmutableObjectRef>,
}

struct PreparedWrite {
    operations: Vec<WalOp>,
    idempotency_key: Option<String>,
    request_fingerprint: RequestFingerprint,
    idempotency_kind: IdempotencyKind,
    conditional_outcome: Option<WriteOutcome>,
    max_unindexed_wal_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IdempotencyKind {
    #[default]
    Append,
    Conditional,
    PatchByFilter,
    DeleteByFilter,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WalCommitState {
    format_version: u32,
    committed: WalCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committed_wal_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PendingWalCommit>,
}

impl WalCommitState {
    fn new(committed: WalCursor) -> Self {
        Self {
            format_version: WAL_COMMIT_FORMAT_VERSION,
            committed,
            committed_wal_bytes: Some(0),
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
            if matches!(pending.idempotency_kind, IdempotencyKind::Append)
                && pending.conditional_outcome.is_some()
            {
                return Err(Error::Corrupt(
                    "ordinary pending WAL contains a conditional outcome".into(),
                ));
            }
            if !matches!(pending.idempotency_kind, IdempotencyKind::Append)
                && pending.conditional_outcome.is_none()
            {
                return Err(Error::Corrupt(
                    "conditional pending WAL is missing its outcome".into(),
                ));
            }
            if let Some(outcome) = &pending.conditional_outcome {
                let suffix = format!("-{}.json", outcome.version.0);
                let valid_prefix = match &pending.idempotency_key {
                    Some(_) => idempotency_prefix(namespace),
                    None => format!(
                        "namespaces/{namespace}/wal_staging/{}/{}-",
                        pending.cursor.epoch, pending.cursor.seq
                    ),
                };
                if !outcome.key.starts_with(&valid_prefix) || !outcome.key.ends_with(&suffix) {
                    return Err(Error::Corrupt(
                        "conditional outcome key does not match its reservation".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredWalCommit {
    State(Box<WalCommitState>),
    Legacy(WalCursor),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct IdempotencyRecord {
    format_version: u32,
    key: String,
    request_fingerprint: RequestFingerprint,
    cursor: WalCursor,
    #[serde(default)]
    kind: IdempotencyKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conditional_outcome: Option<ImmutableObjectRef>,
}

fn encode_commit_state(state: &WalCommitState) -> Result<Vec<u8>> {
    serde_json::to_vec(state).map_err(|error| Error::Codec(error.to_string()))
}

fn decode_commit_state(namespace: &str, bytes: &[u8]) -> Result<WalCommitState> {
    let stored: StoredWalCommit =
        serde_json::from_slice(bytes).map_err(|error| Error::Codec(error.to_string()))?;
    let state = match stored {
        StoredWalCommit::State(state) => *state,
        StoredWalCommit::Legacy(cursor) => WalCommitState {
            format_version: WAL_COMMIT_FORMAT_VERSION,
            committed: cursor,
            committed_wal_bytes: None,
            pending: None,
        },
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

fn request_fingerprint<T: Serialize + ?Sized>(request: &T) -> Result<RequestFingerprint> {
    let encoded =
        postcard::to_allocvec(request).map_err(|error| Error::Codec(error.to_string()))?;
    Ok(RequestFingerprint {
        version: version_of(&encoded),
        legacy_version: Some(legacy_version_of(&encoded)),
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

pub(crate) fn validate_namespace_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAMESPACE_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::InvalidWrite(format!(
            "namespace name must match [A-Za-z0-9-_.]{{1,{MAX_NAMESPACE_NAME_BYTES}}}"
        )));
    }
    Ok(())
}

fn can_disable_backpressure(operations: &[WalOp]) -> bool {
    operations
        .iter()
        .all(|operation| matches!(operation, WalOp::Upsert { .. } | WalOp::Delete { .. }))
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
        validate_namespace_name(name)?;
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
        validate_namespace_name(name)?;
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
        let start = match from {
            Some(from) => {
                if from > to {
                    return Err(Error::Corrupt(format!(
                        "indexed WAL cursor {from:?} is ahead of committed cursor {to:?}"
                    )));
                }
                if from == to {
                    return Ok(Vec::new());
                }
                if from.epoch != to.epoch {
                    return Err(Error::Corrupt(format!(
                        "WAL overlay crosses unsupported epoch boundary {} -> {}",
                        from.epoch, to.epoch
                    )));
                }
                from.seq.checked_add(1).ok_or_else(|| {
                    Error::Corrupt("indexed WAL sequence overflow while reading overlay".into())
                })?
            }
            None => 1,
        };
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

    async fn load_commit_state(&self) -> Result<(GetResult, WalCommitState)> {
        loop {
            let got = self.store.get(&wal_commit_key(&self.name)).await?;
            let mut state = decode_commit_state(&self.name, &got.bytes)?;
            if state.committed_wal_bytes.is_some() {
                return Ok((got, state));
            }

            state.committed_wal_bytes = Some(
                self.reconstruct_committed_wal_bytes(state.committed)
                    .await?,
            );
            match self
                .store
                .compare_and_set(
                    &wal_commit_key(&self.name),
                    got.version.clone(),
                    Bytes::from(encode_commit_state(&state)?),
                )
                .await
            {
                Ok(version) => {
                    return Ok((
                        GetResult {
                            bytes: Bytes::from(encode_commit_state(&state)?),
                            version,
                        },
                        state,
                    ));
                }
                Err(Error::CasMismatch { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    async fn reconstruct_committed_wal_bytes(&self, committed: WalCursor) -> Result<u64> {
        let manifest = self.load_manifest().await?;
        let start = match manifest.indexed_cursor {
            Some(indexed) if indexed.epoch == committed.epoch => {
                if indexed.seq > committed.seq {
                    return Err(Error::Corrupt(format!(
                        "indexed WAL cursor {indexed:?} is ahead of committed cursor {committed:?}"
                    )));
                }
                if indexed.seq == committed.seq {
                    return Ok(manifest.indexed_wal_bytes);
                }
                indexed.seq.checked_add(1).ok_or_else(|| {
                    Error::Corrupt("indexed WAL sequence overflow during migration".into())
                })?
            }
            Some(indexed) => {
                return Err(Error::Corrupt(format!(
                    "indexed WAL epoch {} differs from committed epoch {}",
                    indexed.epoch, committed.epoch
                )));
            }
            None => 1,
        };
        if start > committed.seq {
            return Ok(0);
        }

        let prefix = format!("namespaces/{}/wal/{}/", self.name, committed.epoch);
        let mut entries = Vec::new();
        for object in self.store.list(&prefix).await? {
            let Some(sequence) = object
                .key
                .strip_prefix(&prefix)
                .and_then(|key| key.strip_suffix(".wal"))
                .and_then(|key| key.parse::<u64>().ok())
            else {
                continue;
            };
            if (start..=committed.seq).contains(&sequence) {
                entries.push((sequence, object.size));
            }
        }
        entries.sort_unstable_by_key(|entry| entry.0);

        let mut expected = Some(start);
        let mut total = manifest.indexed_wal_bytes;
        for (sequence, size) in entries {
            if Some(sequence) != expected {
                return Err(Error::Corrupt(format!(
                    "missing WAL sequence {} while migrating byte accounting",
                    expected.expect("a listed sequence cannot follow the committed cursor")
                )));
            }
            total = total
                .checked_add(size)
                .ok_or_else(|| Error::Corrupt("committed WAL byte counter overflow".into()))?;
            expected = if sequence == committed.seq {
                None
            } else {
                Some(sequence.checked_add(1).ok_or_else(|| {
                    Error::Corrupt("WAL sequence overflow during migration".into())
                })?)
            };
        }
        if let Some(expected) = expected {
            return Err(Error::Corrupt(format!(
                "missing WAL sequence {expected} while migrating byte accounting"
            )));
        }
        Ok(total)
    }

    pub(crate) async fn wal_commit_stats(&self) -> Result<(WalCursor, u64)> {
        let (_, state) = self.load_commit_state().await?;
        let committed_wal_bytes = state
            .committed_wal_bytes
            .ok_or_else(|| Error::Corrupt("WAL commit byte counter was not migrated".into()))?;
        Ok((state.committed, committed_wal_bytes))
    }

    /// Exact bytes in the committed WAL overlay not yet folded into the
    /// current manifest's indexes.
    pub async fn unindexed_wal_bytes(&self) -> Result<u64> {
        for _ in 0..3 {
            let manifest = self.load_manifest().await?;
            let (_, committed_wal_bytes) = self.wal_commit_stats().await?;
            if let Some(unindexed) = committed_wal_bytes.checked_sub(manifest.indexed_wal_bytes) {
                return Ok(unindexed);
            }
            tokio::task::yield_now().await;
        }
        Err(Error::Corrupt(
            "indexed WAL byte watermark exceeds committed WAL bytes".into(),
        ))
    }

    pub(crate) async fn wal_gc_state(&self) -> Result<(WalCursor, Vec<String>)> {
        let (_, state) = self.load_commit_state().await?;
        let mut pending_keys = Vec::new();
        if let Some(pending) = state.pending {
            pending_keys.push(pending.staging_key);
            if let Some(outcome) = pending.conditional_outcome {
                pending_keys.push(outcome.key);
            }
        }
        Ok((state.committed, pending_keys))
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
        self.append_with_options(operations, idempotency_key, WriteOptions::default())
            .await
    }

    pub async fn append_with_options(
        &self,
        operations: Vec<WalOp>,
        idempotency_key: Option<String>,
        options: WriteOptions,
    ) -> Result<WalCursor> {
        if operations.is_empty() {
            return Err(Error::InvalidWrite("write batch cannot be empty".into()));
        }
        if let Some(key) = &idempotency_key {
            validate_idempotency_key(key)?;
        }
        let fingerprint = request_fingerprint(&operations)?;
        let max_unindexed_wal_bytes =
            if options.disable_backpressure && can_disable_backpressure(&operations) {
                None
            } else {
                Some(options.max_unindexed_wal_bytes)
            };

        let append_guard = self.append_lock.lock().await;
        let committed = self
            .append_locked(
                operations,
                idempotency_key,
                fingerprint,
                max_unindexed_wal_bytes,
            )
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

    /// Atomically evaluate known-ID conditions and commit the operations that
    /// match. Conditions use the same scalar/filter semantics as queries.
    /// Missing upserts apply unconditionally; missing patches and deletes skip.
    pub async fn conditional_write(
        &self,
        writes: Vec<ConditionalWriteOp>,
        idempotency_key: Option<String>,
    ) -> Result<ConditionalWriteResult> {
        self.conditional_write_with_options(writes, idempotency_key, WriteOptions::default())
            .await
    }

    pub async fn conditional_write_with_options(
        &self,
        writes: Vec<ConditionalWriteOp>,
        idempotency_key: Option<String>,
        options: WriteOptions,
    ) -> Result<ConditionalWriteResult> {
        if writes.is_empty() {
            return Err(Error::InvalidWrite(
                "conditional write batch cannot be empty".into(),
            ));
        }
        if let Some(key) = &idempotency_key {
            validate_idempotency_key(key)?;
        }
        validate_unique_write_ids(&writes)?;
        let fingerprint = request_fingerprint(&writes)?;

        let append_guard = self.append_lock.lock().await;
        let result = self
            .conditional_write_locked(
                writes,
                idempotency_key,
                fingerprint,
                IdempotencyKind::Conditional,
                false,
                options.max_unindexed_wal_bytes,
            )
            .await?;
        drop(append_guard);

        let _ = IndexQueue::new(self.store.clone())
            .enqueue(&self.name, result.cursor)
            .await;
        Ok(result)
    }

    /// Two-phase Read Committed patch: capture matching IDs, then atomically
    /// recheck only those IDs and patch the rows that still match.
    pub async fn patch_by_filter(
        &self,
        request: PatchByFilterRequest,
        idempotency_key: Option<String>,
    ) -> Result<ConditionalWriteResult> {
        self.patch_by_filter_with_options(request, idempotency_key, WriteOptions::default())
            .await
    }

    pub async fn patch_by_filter_with_options(
        &self,
        request: PatchByFilterRequest,
        idempotency_key: Option<String>,
        options: WriteOptions,
    ) -> Result<ConditionalWriteResult> {
        if request.attributes.is_empty() && request.vectors.is_empty() {
            return Err(Error::InvalidWrite(
                "patch-by-filter requires at least one patched field".into(),
            ));
        }
        self.validate_filter_mutation_input(request.max_rows, idempotency_key.as_deref())?;
        self.validate_ops_for_current_schema(&[WalOp::Patch {
            id: Id::U64(0),
            attributes: request.attributes.clone(),
            vectors: request.vectors.clone(),
        }])
        .await?;
        let fingerprint = request_fingerprint(&request)?;
        {
            let append_guard = self.append_lock.lock().await;
            if let Some(result) = self
                .lookup_write_result(
                    idempotency_key.as_deref(),
                    &fingerprint,
                    IdempotencyKind::PatchByFilter,
                )
                .await?
            {
                drop(append_guard);
                return Ok(result);
            }
        }
        let candidates = self
            .matching_filter_ids(&request.filter, options.max_unindexed_wal_bytes)
            .await?;
        let append_guard = self.append_lock.lock().await;
        if let Some(result) = self
            .lookup_write_result(
                idempotency_key.as_deref(),
                &fingerprint,
                IdempotencyKind::PatchByFilter,
            )
            .await?
        {
            drop(append_guard);
            return Ok(result);
        }
        let (candidates, rows_remaining) = limit_filter_candidates(
            candidates,
            request.max_rows,
            request.allow_partial,
            "patch-by-filter",
        )?;
        let writes = candidates
            .into_iter()
            .map(|id| ConditionalWriteOp {
                operation: WalOp::Patch {
                    id,
                    attributes: request.attributes.clone(),
                    vectors: request.vectors.clone(),
                },
                condition: Some(request.filter.clone()),
            })
            .collect();
        let result = self
            .conditional_write_locked(
                writes,
                idempotency_key,
                fingerprint,
                IdempotencyKind::PatchByFilter,
                rows_remaining,
                options.max_unindexed_wal_bytes,
            )
            .await?;
        drop(append_guard);
        let _ = IndexQueue::new(self.store.clone())
            .enqueue(&self.name, result.cursor)
            .await;
        Ok(result)
    }

    /// Two-phase Read Committed delete: capture matching IDs, then atomically
    /// recheck only those IDs and delete the rows that still match.
    pub async fn delete_by_filter(
        &self,
        request: DeleteByFilterRequest,
        idempotency_key: Option<String>,
    ) -> Result<ConditionalWriteResult> {
        self.delete_by_filter_with_options(request, idempotency_key, WriteOptions::default())
            .await
    }

    pub async fn delete_by_filter_with_options(
        &self,
        request: DeleteByFilterRequest,
        idempotency_key: Option<String>,
        options: WriteOptions,
    ) -> Result<ConditionalWriteResult> {
        self.validate_filter_mutation_input(request.max_rows, idempotency_key.as_deref())?;
        let fingerprint = request_fingerprint(&request)?;
        {
            let append_guard = self.append_lock.lock().await;
            if let Some(result) = self
                .lookup_write_result(
                    idempotency_key.as_deref(),
                    &fingerprint,
                    IdempotencyKind::DeleteByFilter,
                )
                .await?
            {
                drop(append_guard);
                return Ok(result);
            }
        }
        let candidates = self
            .matching_filter_ids(&request.filter, options.max_unindexed_wal_bytes)
            .await?;
        let append_guard = self.append_lock.lock().await;
        if let Some(result) = self
            .lookup_write_result(
                idempotency_key.as_deref(),
                &fingerprint,
                IdempotencyKind::DeleteByFilter,
            )
            .await?
        {
            drop(append_guard);
            return Ok(result);
        }
        let (candidates, rows_remaining) = limit_filter_candidates(
            candidates,
            request.max_rows,
            request.allow_partial,
            "delete-by-filter",
        )?;
        let writes = candidates
            .into_iter()
            .map(|id| ConditionalWriteOp {
                operation: WalOp::Delete { id },
                condition: Some(request.filter.clone()),
            })
            .collect();
        let result = self
            .conditional_write_locked(
                writes,
                idempotency_key,
                fingerprint,
                IdempotencyKind::DeleteByFilter,
                rows_remaining,
                options.max_unindexed_wal_bytes,
            )
            .await?;
        drop(append_guard);
        let _ = IndexQueue::new(self.store.clone())
            .enqueue(&self.name, result.cursor)
            .await;
        Ok(result)
    }

    fn validate_filter_mutation_input(
        &self,
        max_rows: usize,
        idempotency_key: Option<&str>,
    ) -> Result<()> {
        if max_rows == 0 {
            return Err(Error::InvalidWrite(
                "filter mutation max_rows must be greater than zero".into(),
            ));
        }
        if let Some(key) = idempotency_key {
            validate_idempotency_key(key)?;
        }
        Ok(())
    }

    async fn matching_filter_ids(
        &self,
        filter: &crate::query::FilterExpr,
        max_unindexed_wal_bytes: u64,
    ) -> Result<Vec<Id>> {
        let manifest = self.load_manifest().await?;
        let (commit, committed_wal_bytes) = self.wal_commit_stats().await?;
        enforce_limit(
            unindexed_wal_bytes(&manifest, committed_wal_bytes)?,
            max_unindexed_wal_bytes,
        )?;
        let documents = self.replay_at(&manifest, commit).await?;
        let mut ids = Vec::new();
        for (id, document) in documents {
            if crate::query::filter_matches(filter, &document)? {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    async fn lookup_write_result(
        &self,
        idempotency_key: Option<&str>,
        fingerprint: &RequestFingerprint,
        kind: IdempotencyKind,
    ) -> Result<Option<ConditionalWriteResult>> {
        let Some(key) = idempotency_key else {
            return Ok(None);
        };
        loop {
            let (_, state) = self.load_commit_state().await?;
            if let Some(pending) = state.pending {
                self.finish_pending(pending).await?;
                continue;
            }
            let Some(record) = self
                .lookup_idempotency(key, fingerprint, kind, state.committed)
                .await?
            else {
                return Ok(None);
            };
            let outcome_ref = record.conditional_outcome.ok_or_else(|| {
                Error::Corrupt("filter mutation idempotency record has no outcome".into())
            })?;
            let outcome = self.load_write_outcome(&outcome_ref).await?;
            return Ok(Some(ConditionalWriteResult {
                cursor: record.cursor,
                outcome,
            }));
        }
    }

    async fn append_locked(
        &self,
        operations: Vec<WalOp>,
        idempotency_key: Option<String>,
        fingerprint: RequestFingerprint,
        max_unindexed_wal_bytes: Option<u64>,
    ) -> Result<WalCursor> {
        loop {
            let (current, state) = self.load_commit_state().await?;
            if let Some(pending) = state.pending.clone() {
                self.finish_pending(pending).await?;
                continue;
            }

            if let Some(key) = &idempotency_key
                && let Some(record) = self
                    .lookup_idempotency(key, &fingerprint, IdempotencyKind::Append, state.committed)
                    .await?
            {
                return Ok(record.cursor);
            }

            match self
                .try_reserve(
                    current,
                    state,
                    PreparedWrite {
                        operations: operations.clone(),
                        idempotency_key: idempotency_key.clone(),
                        request_fingerprint: fingerprint.clone(),
                        idempotency_kind: IdempotencyKind::Append,
                        conditional_outcome: None,
                        max_unindexed_wal_bytes,
                    },
                )
                .await?
            {
                Some(pending) => return self.finish_pending(pending).await,
                None => continue,
            }
        }
    }

    async fn conditional_write_locked(
        &self,
        writes: Vec<ConditionalWriteOp>,
        idempotency_key: Option<String>,
        fingerprint: RequestFingerprint,
        idempotency_kind: IdempotencyKind,
        rows_remaining: bool,
        max_unindexed_wal_bytes: u64,
    ) -> Result<ConditionalWriteResult> {
        let all_operations: Vec<WalOp> =
            writes.iter().map(|write| write.operation.clone()).collect();
        loop {
            let (current, state) = self.load_commit_state().await?;
            if let Some(pending) = state.pending.clone() {
                self.finish_pending(pending).await?;
                continue;
            }

            if let Some(key) = &idempotency_key
                && let Some(record) = self
                    .lookup_idempotency(key, &fingerprint, idempotency_kind, state.committed)
                    .await?
            {
                let outcome_ref = record.conditional_outcome.ok_or_else(|| {
                    Error::Corrupt("conditional idempotency record has no outcome".into())
                })?;
                let outcome = self.load_write_outcome(&outcome_ref).await?;
                return Ok(ConditionalWriteResult {
                    cursor: record.cursor,
                    outcome,
                });
            }

            // Validate the complete request even when a condition will skip an
            // operation. Only applied operations are later used for schema
            // publication.
            self.validate_ops_for_current_schema(&all_operations)
                .await?;
            let manifest = self.load_manifest().await?;
            let committed_wal_bytes = state
                .committed_wal_bytes
                .ok_or_else(|| Error::Corrupt("WAL commit byte counter was not migrated".into()))?;
            let current_unindexed = match unindexed_wal_bytes(&manifest, committed_wal_bytes) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let latest = self.store.get(&wal_commit_key(&self.name)).await?;
                    if latest.version != current.version {
                        continue;
                    }
                    return Err(error);
                }
            };
            enforce_limit(current_unindexed, max_unindexed_wal_bytes)?;
            let documents = self.replay_at(&manifest, state.committed).await?;
            let (operations, mut outcome) = evaluate_conditional_writes(&writes, &documents)?;
            outcome.rows_remaining = rows_remaining;

            match self
                .try_reserve(
                    current,
                    state,
                    PreparedWrite {
                        operations,
                        idempotency_key: idempotency_key.clone(),
                        request_fingerprint: fingerprint.clone(),
                        idempotency_kind,
                        conditional_outcome: Some(outcome.clone()),
                        max_unindexed_wal_bytes: Some(max_unindexed_wal_bytes),
                    },
                )
                .await?
            {
                Some(pending) => {
                    let cursor = self.finish_pending(pending).await?;
                    return Ok(ConditionalWriteResult { cursor, outcome });
                }
                None => continue,
            }
        }
    }

    async fn validate_ops_for_current_schema(&self, operations: &[WalOp]) -> Result<()> {
        let mut schema = self.load_manifest().await?.schema;
        schema.infer_and_validate_ops(operations)?;
        Ok(())
    }

    async fn try_reserve(
        &self,
        current: GetResult,
        state: WalCommitState,
        prepared: PreparedWrite,
    ) -> Result<Option<PendingWalCommit>> {
        // Do not reserve a request that is already invalid against the current
        // schema. A concurrent writer must reserve the commit state before
        // publishing its schema change, so a successful reservation fences
        // schema evolution until this request commits or is aborted.
        self.validate_ops_for_current_schema(&prepared.operations)
            .await?;

        let next_seq = state
            .committed
            .seq
            .checked_add(1)
            .ok_or_else(|| Error::InvalidWrite("WAL sequence exhausted".into()))?;
        let next = WalCursor::new(state.committed.epoch, next_seq);
        let wal_fingerprint = request_fingerprint(&prepared.operations)?;
        let batch = WalBatch {
            namespace: self.name.clone(),
            sequence: next.seq,
            created_at_ms: now_ms(),
            idempotency_key: prepared.idempotency_key.clone(),
            operations: prepared.operations,
        };
        let encoded = batch.encode()?;
        let wal_size_bytes = u64::try_from(encoded.len())
            .map_err(|_| Error::InvalidWrite("encoded WAL batch is too large".into()))?;
        if let Some(limit_bytes) = prepared.max_unindexed_wal_bytes {
            let manifest = self.load_manifest().await?;
            let committed_wal_bytes = state
                .committed_wal_bytes
                .ok_or_else(|| Error::Corrupt("WAL commit byte counter was not migrated".into()))?;
            let current_unindexed = match unindexed_wal_bytes(&manifest, committed_wal_bytes) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let latest = self.store.get(&wal_commit_key(&self.name)).await?;
                    if latest.version != current.version {
                        return Ok(None);
                    }
                    return Err(error);
                }
            };
            let projected_unindexed =
                current_unindexed
                    .checked_add(wal_size_bytes)
                    .ok_or_else(|| {
                        Error::InvalidWrite("projected unindexed WAL bytes overflow".into())
                    })?;
            enforce_limit(projected_unindexed, limit_bytes)?;
        }
        let staging_version = version_of(&encoded);
        let staging_key = wal_staging_key(&self.name, next, &staging_version);
        put_immutable_if_absent(&self.store, &staging_key, Bytes::from(encoded)).await?;
        let conditional_outcome = match prepared.conditional_outcome {
            Some(outcome) => {
                let encoded = serde_json::to_vec(&outcome)
                    .map_err(|error| Error::Codec(error.to_string()))?;
                let version = version_of(&encoded);
                let key = conditional_outcome_key(
                    &self.name,
                    next,
                    prepared.idempotency_key.as_deref(),
                    &version,
                );
                put_immutable_if_absent(&self.store, &key, Bytes::from(encoded)).await?;
                Some(ImmutableObjectRef { key, version })
            }
            None => None,
        };

        let pending = PendingWalCommit {
            cursor: next,
            staging_key,
            staging_version,
            wal_size_bytes: Some(wal_size_bytes),
            request_fingerprint: prepared.request_fingerprint,
            wal_fingerprint: Some(wal_fingerprint),
            idempotency_key: prepared.idempotency_key,
            idempotency_kind: prepared.idempotency_kind,
            conditional_outcome,
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
            Ok(_) => Ok(Some(pending)),
            Err(Error::CasMismatch { .. }) => Ok(None),
            Err(error) => Err(error),
        }
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
            if !version_matches(
                &pending.staging_version,
                &staged.version,
                &staged.bytes,
            ) {
                return Err(Error::Corrupt(format!(
                    "pending WAL staging version mismatch at {}",
                    pending.staging_key
                )));
            }
            let staged_size = u64::try_from(staged.bytes.len())
                .map_err(|_| Error::Corrupt("staged WAL object is too large".into()))?;
            if let Some(expected_size) = pending.wal_size_bytes
                && staged_size != expected_size
            {
                return Err(Error::Corrupt(format!(
                    "pending WAL staging size mismatch at {}",
                    pending.staging_key
                )));
            }
            let batch = WalBatch::decode(&staged.bytes)?;
            let expected_wal_fingerprint = pending
                .wal_fingerprint
                .as_ref()
                .unwrap_or(&pending.request_fingerprint);
            if batch.namespace != self.name
                || batch.sequence != pending.cursor.seq
                || batch.idempotency_key != pending.idempotency_key
                || !request_fingerprint(&batch.operations)?.matches(expected_wal_fingerprint)
            {
                return Err(Error::Corrupt(
                    "pending WAL staging object does not match its reservation".into(),
                ));
            }
            if let Some(outcome) = &pending.conditional_outcome {
                self.load_write_outcome(outcome).await?;
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

            put_immutable_if_absent(
                &self.store,
                &wal_key(&self.name, pending.cursor),
                staged.bytes.clone(),
            )
            .await?;
            if let Some(key) = &pending.idempotency_key {
                self.write_idempotency_record(key, &pending).await?;
            }

            let mut committed = state;
            committed.committed = pending.cursor;
            committed.committed_wal_bytes = Some(
                committed
                    .committed_wal_bytes
                    .ok_or_else(|| {
                        Error::Corrupt("WAL commit byte counter was not migrated".into())
                    })?
                    .checked_add(staged_size)
                    .ok_or_else(|| Error::Corrupt("committed WAL byte counter overflow".into()))?,
            );
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
        kind: IdempotencyKind,
        committed: WalCursor,
    ) -> Result<Option<IdempotencyRecord>> {
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
        if (!matches!(record.kind, IdempotencyKind::Append) && record.conditional_outcome.is_none())
            || (matches!(record.kind, IdempotencyKind::Append)
                && record.conditional_outcome.is_some())
        {
            return Err(Error::Corrupt(format!(
                "idempotency record kind/outcome mismatch at {object_key}"
            )));
        }
        if !record.request_fingerprint.matches(fingerprint) || record.kind != kind {
            return Err(Error::IdempotencyConflict(key.to_string()));
        }
        if record.cursor > committed {
            return Err(Error::Corrupt(format!(
                "idempotency record {object_key} points past the committed WAL"
            )));
        }
        Ok(Some(record))
    }

    async fn write_idempotency_record(&self, key: &str, pending: &PendingWalCommit) -> Result<()> {
        let object_key = idempotency_key_path(&self.name, key);
        let record = IdempotencyRecord {
            format_version: IDEMPOTENCY_FORMAT_VERSION,
            key: key.to_string(),
            request_fingerprint: pending.request_fingerprint.clone(),
            cursor: pending.cursor,
            kind: pending.idempotency_kind,
            conditional_outcome: pending.conditional_outcome.clone(),
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

    async fn load_write_outcome(&self, object: &ImmutableObjectRef) -> Result<WriteOutcome> {
        let got = self.store.get(&object.key).await?;
        if !version_matches(&object.version, &got.version, &got.bytes) {
            return Err(Error::Corrupt(format!(
                "conditional write outcome version mismatch at {}",
                object.key
            )));
        }
        serde_json::from_slice(&got.bytes).map_err(|error| Error::Codec(error.to_string()))
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
        self.upsert_with_options(document, WriteOptions::default())
            .await
    }

    pub async fn upsert_with_options(
        &self,
        document: Document,
        options: WriteOptions,
    ) -> Result<WalCursor> {
        let id = document.id.clone();
        self.append_with_options(vec![WalOp::Upsert { id, document }], None, options)
            .await
    }

    pub async fn delete(&self, id: Id) -> Result<WalCursor> {
        self.delete_with_options(id, WriteOptions::default()).await
    }

    pub async fn delete_with_options(&self, id: Id, options: WriteOptions) -> Result<WalCursor> {
        self.append_with_options(vec![WalOp::Delete { id }], None, options)
            .await
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
        self.query_with_options(query, QueryOptions::default())
            .await
    }

    pub async fn query_with_options(
        &self,
        query: Query,
        options: QueryOptions,
    ) -> Result<QueryResult> {
        crate::query::execute_with_options(self, query, options).await
    }

    /// Execute several independent query plans against one captured manifest
    /// and WAL commit snapshot. Hybrid rank fusion stays a caller concern; this
    /// gives text/vector/attribute subqueries a consistent read timestamp.
    pub async fn multi_query(&self, query: MultiQuery) -> Result<MultiQueryResult> {
        self.multi_query_with_options(query, QueryOptions::default())
            .await
    }

    pub async fn multi_query_with_options(
        &self,
        query: MultiQuery,
        options: QueryOptions,
    ) -> Result<MultiQueryResult> {
        crate::query::execute_multi_with_options(self, query, options).await
    }

    /// Evaluate ANN recall by comparing approximate vector search with exact
    /// search over sampled vectors from the strong snapshot.
    pub async fn recall(&self, request: RecallRequest) -> Result<RecallResult> {
        self.recall_with_options(request, QueryOptions::default())
            .await
    }

    pub async fn recall_with_options(
        &self,
        request: RecallRequest,
        options: QueryOptions,
    ) -> Result<RecallResult> {
        crate::query::recall_with_options(self, request, options).await
    }
}

fn validate_unique_write_ids(writes: &[ConditionalWriteOp]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for write in writes {
        let id = op_id(&write.operation);
        if !ids.insert(id.clone()) {
            return Err(Error::InvalidWrite(format!(
                "conditional write contains duplicate id {id:?}"
            )));
        }
    }
    Ok(())
}

fn limit_filter_candidates(
    mut candidates: Vec<Id>,
    max_rows: usize,
    allow_partial: bool,
    operation: &str,
) -> Result<(Vec<Id>, bool)> {
    let rows_remaining = candidates.len() > max_rows;
    if rows_remaining && !allow_partial {
        return Err(Error::InvalidWrite(format!(
            "{operation} matched {} rows, exceeding max_rows {max_rows}",
            candidates.len()
        )));
    }
    candidates.truncate(max_rows);
    Ok((candidates, rows_remaining))
}

fn evaluate_conditional_writes(
    writes: &[ConditionalWriteOp],
    documents: &BTreeMap<Id, Document>,
) -> Result<(Vec<WalOp>, WriteOutcome)> {
    let mut operations = Vec::new();
    let mut outcome = WriteOutcome::default();
    for write in writes {
        let id = op_id(&write.operation);
        let current = documents.get(id);
        let applies = match (&write.operation, current) {
            (WalOp::Upsert { .. }, None) => true,
            (WalOp::Patch { .. } | WalOp::Delete { .. }, None) => false,
            (_, Some(document)) => match &write.condition {
                Some(condition) => crate::query::filter_matches(condition, document)?,
                None => true,
            },
        };

        if !applies {
            outcome.skipped_ids.push(id.clone());
            continue;
        }

        outcome.rows_affected = outcome
            .rows_affected
            .checked_add(1)
            .ok_or_else(|| Error::InvalidWrite("conditional affected-row count overflow".into()))?;
        match &write.operation {
            WalOp::Upsert { .. } => outcome.rows_upserted += 1,
            WalOp::Patch { .. } => outcome.rows_patched += 1,
            WalOp::Delete { .. } => outcome.rows_deleted += 1,
        }
        outcome.applied_ids.push(id.clone());
        operations.push(write.operation.clone());
    }
    Ok((operations, outcome))
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

#[cfg(test)]
mod tests {
    use super::{RequestFingerprint, request_fingerprint};
    use crate::object_store::legacy_version_of;
    use crate::value::Id;
    use crate::wal::WalOp;

    #[test]
    fn request_fingerprint_matches_legacy_records() {
        let operations = vec![WalOp::Delete { id: Id::U64(7) }];
        let current = request_fingerprint(&operations).unwrap();
        let encoded = postcard::to_allocvec(&operations).unwrap();
        let legacy = RequestFingerprint {
            version: legacy_version_of(&encoded),
            legacy_version: None,
            size_bytes: current.size_bytes,
            crc32: current.crc32,
        };

        assert!(current.matches(&legacy));
        assert!(legacy.matches(&current));
    }
}
