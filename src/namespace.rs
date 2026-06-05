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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use crate::doc::{DocRecord, encode_id};
use crate::error::{Error, Result};
use crate::manifest::{ManifestPointer, NamespaceManifest};
use crate::object_store::ObjectStore;
use crate::sst::SstReader;
use crate::value::{Document, Id, Value};
use crate::wal::{WalBatch, WalCursor, WalOp};

pub(crate) fn manifest_pointer_key(ns: &str) -> String {
    format!("namespaces/{ns}/manifest/current")
}

pub(crate) fn manifest_body_key(ns: &str, generation: u64) -> String {
    format!("namespaces/{ns}/manifest/g/{generation}.json")
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

impl std::fmt::Debug for Namespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Namespace").field("name", &self.name).finish()
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
            .put_if_absent(
                &pointer_key,
                Bytes::from(ManifestPointer::new(0).encode()?),
            )
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
    pub(crate) async fn read_overlay_ops(
        &self,
        from: Option<WalCursor>,
        to: WalCursor,
    ) -> Result<Vec<WalOp>> {
        let start = from.map(|c| c.seq + 1).unwrap_or(1);
        let mut ops = Vec::new();
        for seq in start..=to.seq {
            let got = self
                .store
                .get(&wal_key(&self.name, WalCursor::new(to.epoch, seq)))
                .await?;
            ops.extend(WalBatch::decode(&got.bytes)?.operations);
        }
        Ok(ops)
    }

    /// Resolve the newest SST record for an id (point lookup, newest-first),
    /// skipping files whose `[min_id, max_id]` cannot contain it.
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
            if let Some(value) = self.load_sst(&meta.key).await?.get(&key)? {
                return Ok(Some(DocRecord::decode(&value)?));
            }
        }
        Ok(None)
    }

    /// Load the current manifest body via the pointer.
    pub async fn load_manifest(&self) -> Result<NamespaceManifest> {
        let pointer = ManifestPointer::decode(
            &self.store.get(&manifest_pointer_key(&self.name)).await?.bytes,
        )?;
        let body = self
            .store
            .get(&manifest_body_key(&self.name, pointer.generation))
            .await?;
        NamespaceManifest::decode(&body.bytes)
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
        let _g = self.append_lock.lock().await;

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

        Ok(next)
    }

    pub async fn upsert(&self, document: Document) -> Result<WalCursor> {
        let id = document.id.clone();
        self.append(vec![WalOp::Upsert { id, document }], None).await
    }

    pub async fn delete(&self, id: Id) -> Result<WalCursor> {
        self.append(vec![WalOp::Delete { id }], None).await
    }

    /// Materialize the full document snapshot: SST base (newest-first wins,
    /// tombstones dropped) with the recent-WAL overlay applied on top.
    pub async fn replay(&self) -> Result<BTreeMap<Id, Document>> {
        let manifest = self.load_manifest().await?;
        let commit = self.commit_cursor().await?;

        let mut seen: BTreeMap<Id, DocRecord> = BTreeMap::new();
        for meta in &manifest.doc_ssts {
            let reader = self.load_sst(&meta.key).await?;
            for (key, value) in reader.entries()? {
                seen.entry(crate::doc::decode_id(&key)?)
                    .or_insert(DocRecord::decode(&value)?);
            }
        }
        let mut docs: BTreeMap<Id, Document> = seen
            .into_iter()
            .filter_map(|(id, rec)| match rec {
                DocRecord::Present(d) => Some((id, d)),
                DocRecord::Deleted => None,
            })
            .collect();

        for op in self.read_overlay_ops(manifest.indexed_cursor, commit).await? {
            apply_op(&mut docs, op);
        }
        Ok(docs)
    }

    /// Strong primary-key lookup: SST base for the id with the overlay applied.
    pub async fn lookup(&self, id: &Id) -> Result<Option<Document>> {
        let manifest = self.load_manifest().await?;
        let commit = self.commit_cursor().await?;

        let mut docs: BTreeMap<Id, Document> = BTreeMap::new();
        if let Some(DocRecord::Present(doc)) = self.sst_point_get(&manifest, id).await? {
            docs.insert(id.clone(), doc);
        }
        for op in self.read_overlay_ops(manifest.indexed_cursor, commit).await? {
            if op_id(&op) == id {
                apply_op(&mut docs, op);
            }
        }
        Ok(docs.remove(id))
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
