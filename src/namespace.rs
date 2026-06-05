//! Namespace lifecycle over the object store: create, append WAL batches,
//! CAS-advance the commit cursor, and replay the WAL into documents.
//!
//! This realizes Stage 1's "first useful milestone". The write path advances a
//! lightweight `wal_commit/current` cursor (not the full manifest) on every
//! commit, keeping write durability separate from indexing freshness
//! (architecture Principle 2). The manifest only changes when indexing
//! publishes files (Stage 2+).
//!
//! Concurrency model (Stage 1): single writer per namespace per process. An
//! in-process append lock serializes commits, and the cursor CAS is a
//! belt-and-suspenders check. Cross-process append safety needs S3/GCS
//! conditional writes plus crash-orphan handling, same caveat as the
//! filesystem object store (see decision D4 in docs/PROGRESS.md).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::manifest::{ManifestPointer, NamespaceManifest};
use crate::object_store::ObjectStore;
use crate::value::{Document, Id, Value};
use crate::wal::{WalBatch, WalCursor, WalOp};

fn manifest_pointer_key(ns: &str) -> String {
    format!("namespaces/{ns}/manifest/current")
}

fn manifest_body_key(ns: &str, generation: u64) -> String {
    format!("namespaces/{ns}/manifest/g/{generation}.json")
}

fn wal_commit_key(ns: &str) -> String {
    format!("namespaces/{ns}/wal_commit/current")
}

fn wal_key(ns: &str, cursor: WalCursor) -> String {
    format!("namespaces/{ns}/wal/{}/{}.wal", cursor.epoch, cursor.seq)
}

fn now_ms() -> u64 {
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

    /// Replay the committed WAL into a materialized document snapshot. O(WAL):
    /// Stage 2's SSTs make this efficient; correct and simple for now.
    pub async fn replay(&self) -> Result<BTreeMap<Id, Document>> {
        let cursor = self.commit_cursor().await?;
        let mut docs: BTreeMap<Id, Document> = BTreeMap::new();
        for seq in 1..=cursor.seq {
            let pos = WalCursor::new(cursor.epoch, seq);
            let got = self.store.get(&wal_key(&self.name, pos)).await?;
            let batch = WalBatch::decode(&got.bytes)?;
            for op in batch.operations {
                apply_op(&mut docs, op);
            }
        }
        Ok(docs)
    }

    /// Strong primary-key lookup. Replays the WAL overlay; see `replay`.
    pub async fn lookup(&self, id: &Id) -> Result<Option<Document>> {
        Ok(self.replay().await?.remove(id))
    }
}

fn apply_op(docs: &mut BTreeMap<Id, Document>, op: WalOp) {
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
