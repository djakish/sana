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
//! Concurrency model: single writer per namespace per process. An in-process
//! append lock serializes commits, and the cursor CAS is a belt-and-suspenders
//! check. Cross-process append safety needs S3/GCS conditional writes plus
//! crash-orphan handling, same caveat as the filesystem object store (D4).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use crate::doc::{DocRecord, decode_id, encode_id};
use crate::error::{Error, Result};
use crate::index_queue::{EnqueueOutcome, IndexQueue};
use crate::manifest::{ManifestPointer, NamespaceManifest};
use crate::object_store::{ObjectStore, ObjectVersion, version_of};
use crate::query::{MultiQuery, MultiQueryResult, Query, QueryResult, RecallRequest, RecallResult};
use crate::sst::SstReader;
use crate::value::{Document, Id, Value};
use crate::wal::{WalBatch, WalCursor, WalOp};

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

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn encode_cursor(cursor: &WalCursor) -> Result<Vec<u8>> {
    serde_json::to_vec(cursor).map_err(|e| Error::Codec(e.to_string()))
}

fn decode_cursor(bytes: &[u8]) -> Result<WalCursor> {
    serde_json::from_slice(bytes).map_err(|e| Error::Codec(e.to_string()))
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
        let pointer_key = manifest_pointer_key(name);
        if store.get(&pointer_key).await.is_ok() {
            return Err(Error::AlreadyExists(format!("namespace {name}")));
        }

        let manifest = NamespaceManifest::new(name, now_ms());
        store
            .put(&manifest_body_key(name, 0), Bytes::from(manifest.encode()?))
            .await?;
        store
            .put(
                &wal_commit_key(name),
                Bytes::from(encode_cursor(&WalCursor::new(0, 0))?),
            )
            .await?;
        // The pointer is the existence sentinel; create it last and atomically.
        store
            .put_if_absent(&pointer_key, Bytes::from(ManifestPointer::new(0).encode()?))
            .await
            .map_err(|_| Error::AlreadyExists(format!("namespace {name}")))?;

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
        let got = self.store.get(&wal_commit_key(&self.name)).await?;
        decode_cursor(&got.bytes)
    }

    /// Append one atomic batch and advance the commit cursor. Returns the
    /// committed position.
    pub async fn append(
        &self,
        operations: Vec<WalOp>,
        idempotency_key: Option<String>,
    ) -> Result<WalCursor> {
        if operations.is_empty() {
            return Err(Error::InvalidWrite("write batch cannot be empty".into()));
        }

        let append_guard = self.append_lock.lock().await;
        self.evolve_schema_for_ops(&operations).await?;

        let cursor_key = wal_commit_key(&self.name);
        let current = self.store.get(&cursor_key).await?;
        let cursor = decode_cursor(&current.bytes)?;
        let next = WalCursor::new(cursor.epoch, cursor.seq + 1);

        let batch = WalBatch {
            namespace: self.name.clone(),
            sequence: next.seq,
            created_at_ms: now_ms(),
            idempotency_key,
            operations,
        };

        // Write the WAL object first; it is not "committed" until the cursor
        // advances, so a crashed prior attempt at this seq is a harmless orphan
        // we overwrite here (safe under the single-writer model).
        self.store
            .put(&wal_key(&self.name, next), Bytes::from(batch.encode()?))
            .await?;
        self.store
            .compare_and_set(
                &cursor_key,
                current.version,
                Bytes::from(encode_cursor(&next)?),
            )
            .await?;
        drop(append_guard);

        // Indexing jobs are advisory. A queue outage must not turn a durable
        // WAL commit into a reported write failure; reconciliation can enqueue
        // this cursor again later.
        let _ = IndexQueue::new(self.store.clone())
            .enqueue(&self.name, next)
            .await;

        Ok(next)
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
