use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use sana::error::Error;
use sana::indexer;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, GetResult, ObjectMeta, ObjectStore, ObjectVersion};
use sana::value::{Document, Id, Value, VectorValue};
use sana::wal::WalOp;
use tempfile::TempDir;

fn store(dir: &TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

/// Test-only `ObjectStore` decorator that records the key of every read, so a
/// test can assert *which* objects a lookup touched (not just how many).
struct RecordingStore {
    inner: Arc<dyn ObjectStore>,
    reads: Mutex<Vec<String>>,
}

impl RecordingStore {
    fn new(inner: Arc<dyn ObjectStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            reads: Mutex::new(Vec::new()),
        })
    }

    fn reset(&self) {
        self.reads.lock().unwrap().clear();
    }

    fn reads_of(&self, key: &str) -> usize {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|k| *k == key)
            .count()
    }
}

#[async_trait]
impl ObjectStore for RecordingStore {
    async fn get(&self, key: &str) -> sana::Result<GetResult> {
        self.reads.lock().unwrap().push(key.to_string());
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> sana::Result<Bytes> {
        self.reads.lock().unwrap().push(key.to_string());
        self.inner.get_range(key, range).await
    }

    async fn put(&self, key: &str, bytes: Bytes) -> sana::Result<ObjectVersion> {
        self.inner.put(key, bytes).await
    }

    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> sana::Result<ObjectVersion> {
        self.inner.put_if_absent(key, bytes).await
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> sana::Result<ObjectVersion> {
        self.inner.compare_and_set(key, expected, bytes).await
    }

    async fn list(&self, prefix: &str) -> sana::Result<Vec<ObjectMeta>> {
        self.inner.list(prefix).await
    }

    async fn delete(&self, key: &str) -> sana::Result<()> {
        self.inner.delete(key).await
    }
}

fn doc_with(id: u64, title: &str, score: i64) -> Document {
    let mut d = Document::new(Id::U64(id));
    d.attributes
        .insert("title".into(), Value::String(title.into()));
    d.attributes.insert("score".into(), Value::Int(score));
    d
}

#[tokio::test]
async fn create_append_replay_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    ns.upsert(doc_with(2, "beta", 20)).await.unwrap();

    let docs = ns.replay().await.unwrap();
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[&Id::U64(1)], doc_with(1, "alpha", 10));
    assert_eq!(docs[&Id::U64(2)], doc_with(2, "beta", 20));
}

#[tokio::test]
async fn commit_cursor_advances_per_append() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 0);

    let c1 = ns.upsert(doc_with(1, "a", 1)).await.unwrap();
    let c2 = ns.upsert(doc_with(2, "b", 2)).await.unwrap();
    assert_eq!(c1.seq, 1);
    assert_eq!(c2.seq, 2);
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 2);
}

#[tokio::test]
async fn upsert_overwrites_and_delete_removes() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    ns.upsert(doc_with(1, "v1", 1)).await.unwrap();
    ns.upsert(doc_with(1, "v2", 2)).await.unwrap();
    assert_eq!(
        ns.lookup(&Id::U64(1)).await.unwrap(),
        Some(doc_with(1, "v2", 2))
    );

    ns.delete(Id::U64(1)).await.unwrap();
    assert_eq!(ns.lookup(&Id::U64(1)).await.unwrap(), None);
    assert_eq!(ns.replay().await.unwrap().len(), 0);
}

#[tokio::test]
async fn patch_merges_attributes_and_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();

    let mut attrs = BTreeMap::new();
    attrs.insert("score".into(), Value::Int(99)); // overwrite
    attrs.insert("tag".into(), Value::String("new".into())); // add
    let mut vectors = BTreeMap::new();
    vectors.insert("embedding".into(), VectorValue::F32(vec![1.0, 2.0]));
    ns.append(
        vec![WalOp::Patch {
            id: Id::U64(1),
            attributes: attrs,
            vectors,
        }],
        None,
    )
    .await
    .unwrap();

    let doc = ns.lookup(&Id::U64(1)).await.unwrap().unwrap();
    assert_eq!(doc.attributes["title"], Value::String("alpha".into())); // untouched
    assert_eq!(doc.attributes["score"], Value::Int(99));
    assert_eq!(doc.attributes["tag"], Value::String("new".into()));
    assert_eq!(doc.vectors["embedding"], VectorValue::F32(vec![1.0, 2.0]));
}

#[tokio::test]
async fn patch_with_null_clears_field() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();

    let mut attrs = BTreeMap::new();
    attrs.insert("score".into(), Value::Null);
    ns.append(
        vec![WalOp::Patch {
            id: Id::U64(1),
            attributes: attrs,
            vectors: BTreeMap::new(),
        }],
        None,
    )
    .await
    .unwrap();

    let doc = ns.lookup(&Id::U64(1)).await.unwrap().unwrap();
    assert!(!doc.attributes.contains_key("score"));
    assert!(doc.attributes.contains_key("title"));
}

#[tokio::test]
async fn create_twice_is_already_exists() {
    let dir = tempfile::tempdir().unwrap();
    Namespace::create(store(&dir), "docs").await.unwrap();
    let err = Namespace::create(store(&dir), "docs").await.unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)));
}

#[tokio::test]
async fn concurrent_create_publishes_one_consistent_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let (first, second) = tokio::join!(
        Namespace::create(object_store.clone(), "docs"),
        Namespace::create(object_store.clone(), "docs")
    );

    assert_ne!(first.is_ok(), second.is_ok());
    let error = if let Err(error) = first {
        error
    } else {
        second.unwrap_err()
    };
    assert!(matches!(error, Error::AlreadyExists(_)));

    let namespace = Namespace::open(object_store, "docs").await.unwrap();
    assert_eq!(namespace.commit_cursor().await.unwrap().seq, 0);
    assert_eq!(namespace.load_manifest().await.unwrap().generation, 0);
}

#[tokio::test]
async fn open_missing_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let err = Namespace::open(store(&dir), "ghost").await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
}

#[tokio::test]
async fn data_survives_reopen_with_fresh_store() {
    let dir = tempfile::tempdir().unwrap();
    {
        let ns = Namespace::create(store(&dir), "docs").await.unwrap();
        ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
        ns.upsert(doc_with(2, "beta", 20)).await.unwrap();
    }
    // New store instance over the same directory simulates a process restart.
    let ns = Namespace::open(store(&dir), "docs").await.unwrap();
    let docs = ns.replay().await.unwrap();
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[&Id::U64(1)], doc_with(1, "alpha", 10));

    // Appends continue from the recovered cursor.
    let c = ns.upsert(doc_with(3, "gamma", 30)).await.unwrap();
    assert_eq!(c.seq, 3);
}

#[tokio::test]
async fn point_lookup_prunes_ssts_by_id_range() {
    let dir = tempfile::tempdir().unwrap();
    let store = RecordingStore::new(Arc::new(FsObjectStore::new(dir.path())));
    let ns = Namespace::create(store.clone(), "docs").await.unwrap();

    // Two flushes => two un-compacted doc SSTs with disjoint id ranges, since a
    // flush writes only the touched ids as complete documents (D18).
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    indexer::flush(&ns).await.unwrap();
    ns.upsert(doc_with(100, "omega", 99)).await.unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(
        manifest.doc_ssts.len(),
        2,
        "expected two un-compacted doc SSTs"
    );
    let key_for = |id: u64| {
        manifest
            .doc_ssts
            .iter()
            .find(|m| m.min_id == Some(Id::U64(id)) && m.max_id == Some(Id::U64(id)))
            .unwrap_or_else(|| panic!("no single-id SST for {id}"))
            .key
            .clone()
    };
    let sst_with_1 = key_for(1);
    let sst_with_100 = key_for(100);

    // Looking up id 1 must skip the SST whose [min,max]=[100,100] entirely:
    // a pruned SST receives zero ranged reads.
    store.reset();
    assert_eq!(
        ns.lookup(&Id::U64(1)).await.unwrap(),
        Some(doc_with(1, "alpha", 10))
    );
    assert_eq!(
        store.reads_of(&sst_with_100),
        0,
        "SST [100,100] should be pruned for id 1"
    );
    assert!(
        store.reads_of(&sst_with_1) > 0,
        "SST [1,1] should be read for id 1"
    );

    // Symmetric: id 100 must skip the SST whose [min,max]=[1,1].
    store.reset();
    assert_eq!(
        ns.lookup(&Id::U64(100)).await.unwrap(),
        Some(doc_with(100, "omega", 99))
    );
    assert_eq!(
        store.reads_of(&sst_with_1),
        0,
        "SST [1,1] should be pruned for id 100"
    );
    assert!(
        store.reads_of(&sst_with_100) > 0,
        "SST [100,100] should be read for id 100"
    );

    // A missing id between the ranges still resolves to None.
    assert_eq!(ns.lookup(&Id::U64(50)).await.unwrap(), None);
}

#[tokio::test]
async fn fresh_namespace_replays_empty() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    assert!(ns.replay().await.unwrap().is_empty());
}
