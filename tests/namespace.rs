use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use sana::error::Error;
use sana::indexer;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, GetResult, ObjectMeta, ObjectStore, ObjectVersion};
use sana::value::{Document, Id, Value, VectorValue};
use sana::wal::WalOp;
use sana::write::WriteOptions;
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

struct FailWalCommitCasStore {
    inner: Arc<dyn ObjectStore>,
    fail_attempt: usize,
    fail_after_apply: bool,
    attempts: AtomicUsize,
}

impl FailWalCommitCasStore {
    fn new(inner: Arc<dyn ObjectStore>, fail_attempt: usize) -> Arc<Self> {
        Self::with_mode(inner, fail_attempt, false)
    }

    fn after_apply(inner: Arc<dyn ObjectStore>, fail_attempt: usize) -> Arc<Self> {
        Self::with_mode(inner, fail_attempt, true)
    }

    fn with_mode(
        inner: Arc<dyn ObjectStore>,
        fail_attempt: usize,
        fail_after_apply: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail_attempt,
            fail_after_apply,
            attempts: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ObjectStore for FailWalCommitCasStore {
    async fn get(&self, key: &str) -> sana::Result<GetResult> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> sana::Result<Bytes> {
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
        let should_fail = if key.ends_with("/wal_commit/current") {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            attempt == self.fail_attempt
        } else {
            false
        };
        if should_fail && !self.fail_after_apply {
            return Err(Error::Io(std::io::Error::other(
                "injected WAL commit CAS failure",
            )));
        }
        let result = self.inner.compare_and_set(key, expected, bytes).await;
        if should_fail && self.fail_after_apply {
            result?;
            return Err(Error::Io(std::io::Error::other(
                "injected ambiguous WAL commit response",
            )));
        }
        result
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
async fn unindexed_wal_bytes_tracks_commits_and_flushes() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    assert_eq!(ns.unindexed_wal_bytes().await.unwrap(), 0);

    let first = ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    let first_size = object_store
        .get(&format!(
            "namespaces/docs/wal/{}/{}.wal",
            first.epoch, first.seq
        ))
        .await
        .unwrap()
        .bytes
        .len() as u64;
    assert_eq!(ns.unindexed_wal_bytes().await.unwrap(), first_size);

    let second = ns.upsert(doc_with(2, "beta", 20)).await.unwrap();
    let second_size = object_store
        .get(&format!(
            "namespaces/docs/wal/{}/{}.wal",
            second.epoch, second.seq
        ))
        .await
        .unwrap()
        .bytes
        .len() as u64;
    assert_eq!(
        ns.unindexed_wal_bytes().await.unwrap(),
        first_size + second_size
    );

    assert!(indexer::flush(&ns).await.unwrap());
    assert_eq!(ns.unindexed_wal_bytes().await.unwrap(), 0);
    assert_eq!(
        ns.load_manifest().await.unwrap().indexed_wal_bytes,
        first_size + second_size
    );

    let third = ns.delete(Id::U64(1)).await.unwrap();
    let third_size = object_store
        .get(&format!(
            "namespaces/docs/wal/{}/{}.wal",
            third.epoch, third.seq
        ))
        .await
        .unwrap()
        .bytes
        .len() as u64;
    assert_eq!(ns.unindexed_wal_bytes().await.unwrap(), third_size);
}

#[tokio::test]
async fn write_backpressure_is_projected_and_bulk_bypass_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    let blocked = WriteOptions {
        disable_backpressure: false,
        max_unindexed_wal_bytes: 0,
    };
    let bulk = WriteOptions {
        disable_backpressure: true,
        max_unindexed_wal_bytes: 0,
    };
    let operation = vec![WalOp::Upsert {
        id: Id::U64(1),
        document: doc_with(1, "alpha", 10),
    }];

    let error = ns
        .append_with_options(operation.clone(), Some("bulk-one".into()), blocked)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Backpressure { limit_bytes: 0, .. }));
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 0);

    let cursor = ns
        .append_with_options(operation.clone(), Some("bulk-one".into()), bulk)
        .await
        .unwrap();
    assert_eq!(cursor.seq, 1);

    assert_eq!(
        ns.append_with_options(operation, Some("bulk-one".into()), blocked)
            .await
            .unwrap(),
        cursor,
        "an exact idempotent retry must resolve before backpressure"
    );

    let error = ns
        .append_with_options(
            vec![WalOp::Patch {
                id: Id::U64(1),
                attributes: BTreeMap::from([("score".into(), Value::Int(11))]),
                vectors: BTreeMap::new(),
            }],
            None,
            bulk,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Backpressure { .. }));
    assert_eq!(ns.commit_cursor().await.unwrap(), cursor);
}

#[tokio::test]
async fn concurrent_writers_share_one_backpressure_budget() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    let calibration = ns.delete(Id::U64(1)).await.unwrap();
    let calibration_size = object_store
        .get(&format!(
            "namespaces/docs/wal/{}/{}.wal",
            calibration.epoch, calibration.seq
        ))
        .await
        .unwrap()
        .bytes
        .len() as u64;
    indexer::flush(&ns).await.unwrap();

    let options = WriteOptions {
        disable_backpressure: false,
        max_unindexed_wal_bytes: calibration_size + 32,
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let namespace = Namespace::open(object_store.clone(), "docs").await.unwrap();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            namespace
                .append_with_options(vec![WalOp::Delete { id: Id::U64(1) }], None, options)
                .await
        }));
    }
    barrier.wait().await;

    let mut committed = 0;
    let mut throttled = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => committed += 1,
            Err(Error::Backpressure { .. }) => throttled += 1,
            Err(error) => panic!("unexpected concurrent write error: {error}"),
        }
    }
    assert_eq!(committed, 1);
    assert_eq!(throttled, 7);
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 2);
}

#[tokio::test]
async fn idempotent_retry_survives_reopen_without_consuming_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    let operations = vec![WalOp::Upsert {
        id: Id::U64(1),
        document: doc_with(1, "alpha", 10),
    }];

    let first = ns
        .append(operations.clone(), Some("../request/one".into()))
        .await
        .unwrap();
    drop(ns);
    let reopened = Namespace::open(object_store.clone(), "docs").await.unwrap();
    let retry = reopened
        .append(operations, Some("../request/one".into()))
        .await
        .unwrap();

    assert_eq!(first, retry);
    assert_eq!(retry.seq, 1);
    assert_eq!(reopened.commit_cursor().await.unwrap().seq, 1);
    assert_eq!(
        object_store
            .list("namespaces/docs/wal/")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn idempotency_key_rejects_a_different_payload() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.append(
        vec![WalOp::Upsert {
            id: Id::U64(1),
            document: doc_with(1, "alpha", 10),
        }],
        Some("request-1".into()),
    )
    .await
    .unwrap();

    let error = ns
        .append(
            vec![WalOp::Upsert {
                id: Id::U64(2),
                document: doc_with(2, "beta", 20),
            }],
            Some("request-1".into()),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::IdempotencyConflict(_)));
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 1);
    assert_eq!(ns.lookup(&Id::U64(2)).await.unwrap(), None);
}

#[tokio::test]
async fn concurrent_identical_idempotent_appends_commit_once() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    let first = Namespace::open(object_store.clone(), "docs").await.unwrap();
    let second = Namespace::open(object_store, "docs").await.unwrap();
    let operations = vec![WalOp::Upsert {
        id: Id::U64(1),
        document: doc_with(1, "alpha", 10),
    }];

    let (left, right) = tokio::join!(
        first.append(operations.clone(), Some("same-request".into())),
        second.append(operations, Some("same-request".into()))
    );
    assert_eq!(left.unwrap().seq, 1);
    assert_eq!(right.unwrap().seq, 1);
    assert_eq!(first.commit_cursor().await.unwrap().seq, 1);
}

#[tokio::test]
async fn concurrent_conflicting_idempotent_appends_choose_one_payload() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    let first = Namespace::open(object_store.clone(), "docs").await.unwrap();
    let second = Namespace::open(object_store, "docs").await.unwrap();

    let (left, right) = tokio::join!(
        first.append(
            vec![WalOp::Upsert {
                id: Id::U64(1),
                document: doc_with(1, "left", 10),
            }],
            Some("conflict".into()),
        ),
        second.append(
            vec![WalOp::Upsert {
                id: Id::U64(1),
                document: doc_with(1, "right", 20),
            }],
            Some("conflict".into()),
        )
    );

    assert!(left.is_ok() ^ right.is_ok());
    let error = if let Err(error) = left {
        error
    } else {
        right.unwrap_err()
    };
    assert!(matches!(error, Error::IdempotencyConflict(_)));
    assert_eq!(first.commit_cursor().await.unwrap().seq, 1);
}

#[tokio::test]
async fn concurrent_namespace_handles_assign_distinct_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for id in 1..=16 {
        let namespace = Namespace::open(object_store.clone(), "docs").await.unwrap();
        tasks.spawn(async move {
            namespace
                .upsert(doc_with(id, &format!("doc-{id}"), id as i64))
                .await
        });
    }
    let mut sequences = Vec::new();
    while let Some(result) = tasks.join_next().await {
        sequences.push(result.unwrap().unwrap().seq);
    }
    sequences.sort_unstable();
    assert_eq!(sequences, (1..=16).collect::<Vec<_>>());

    let namespace = Namespace::open(object_store, "docs").await.unwrap();
    assert_eq!(namespace.replay().await.unwrap().len(), 16);
}

#[tokio::test]
async fn pending_idempotent_commit_recovers_after_failure_and_gc() {
    let dir = tempfile::tempdir().unwrap();
    let inner = store(&dir);
    let failing: Arc<dyn ObjectStore> = FailWalCommitCasStore::new(inner.clone(), 2);
    let ns = Namespace::create(failing, "docs").await.unwrap();
    let operations = vec![WalOp::Upsert {
        id: Id::U64(1),
        document: doc_with(1, "alpha", 10),
    }];

    let error = ns
        .append(operations.clone(), Some("recover-me".into()))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Io(_)));
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 0);

    // A pending staging object and its pre-published dedup record must survive
    // cleanup, even though the canonical WAL is not committed yet.
    indexer::gc(&ns, true).await.unwrap();

    let reopened = Namespace::open(inner, "docs").await.unwrap();
    let cursor = reopened
        .append(operations, Some("recover-me".into()))
        .await
        .unwrap();
    assert_eq!(cursor.seq, 1);
    assert_eq!(
        reopened.lookup(&Id::U64(1)).await.unwrap(),
        Some(doc_with(1, "alpha", 10))
    );
}

#[tokio::test]
async fn ambiguous_successful_commit_response_retries_without_duplication() {
    let dir = tempfile::tempdir().unwrap();
    let inner = store(&dir);
    let failing: Arc<dyn ObjectStore> = FailWalCommitCasStore::after_apply(inner.clone(), 2);
    let ns = Namespace::create(failing, "docs").await.unwrap();
    let operations = vec![WalOp::Upsert {
        id: Id::U64(1),
        document: doc_with(1, "alpha", 10),
    }];

    let error = ns
        .append(operations.clone(), Some("ambiguous".into()))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Io(_)));
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 1);

    let reopened = Namespace::open(inner, "docs").await.unwrap();
    let cursor = reopened
        .append(operations, Some("ambiguous".into()))
        .await
        .unwrap();
    assert_eq!(cursor.seq, 1);
    assert_eq!(reopened.commit_cursor().await.unwrap().seq, 1);
}

#[tokio::test]
async fn indexed_wal_gc_preserves_idempotency_record() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    let operations = vec![WalOp::Upsert {
        id: Id::U64(1),
        document: doc_with(1, "alpha", 10),
    }];
    let cursor = ns
        .append(operations.clone(), Some("keep-marker".into()))
        .await
        .unwrap();
    assert!(indexer::flush(&ns).await.unwrap());
    indexer::gc(&ns, true).await.unwrap();
    assert!(
        object_store
            .list("namespaces/docs/wal/")
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(
        ns.append(operations, Some("keep-marker".into()))
            .await
            .unwrap(),
        cursor
    );
    assert_eq!(ns.commit_cursor().await.unwrap(), cursor);
}

#[tokio::test]
async fn legacy_commit_cursor_is_migrated_on_append() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    object_store
        .put(
            "namespaces/docs/wal_commit/current",
            Bytes::from_static(br#"{"epoch":0,"seq":0}"#),
        )
        .await
        .unwrap();

    let cursor = ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    assert_eq!(cursor.seq, 1);
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 1);
}

#[tokio::test]
async fn legacy_commit_cursor_reconstructs_only_the_unindexed_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    ns.upsert(doc_with(2, "beta", 20)).await.unwrap();
    indexer::flush(&ns).await.unwrap();
    let cursor = ns.delete(Id::U64(1)).await.unwrap();
    let unindexed_size = object_store
        .get(&format!(
            "namespaces/docs/wal/{}/{}.wal",
            cursor.epoch, cursor.seq
        ))
        .await
        .unwrap()
        .bytes
        .len() as u64;

    object_store
        .put(
            "namespaces/docs/wal_commit/current",
            Bytes::from(serde_json::to_vec(&cursor).unwrap()),
        )
        .await
        .unwrap();

    assert_eq!(ns.unindexed_wal_bytes().await.unwrap(), unindexed_size);
    assert_eq!(ns.commit_cursor().await.unwrap(), cursor);
}

#[tokio::test]
async fn idempotency_keys_are_bounded_and_nonempty() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    let operations = vec![WalOp::Delete { id: Id::U64(1) }];

    assert!(matches!(
        ns.append(operations.clone(), Some(String::new()))
            .await
            .unwrap_err(),
        Error::InvalidWrite(_)
    ));
    assert!(matches!(
        ns.append(operations, Some("x".repeat(257)))
            .await
            .unwrap_err(),
        Error::InvalidWrite(_)
    ));
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 0);
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
async fn namespace_names_are_validated_before_storage_access() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["", "with/slash", "../escape", "space name"] {
        let error = Namespace::create(store(&dir), name).await.unwrap_err();
        assert!(matches!(error, Error::InvalidWrite(_)));
    }
    let too_long = "a".repeat(129);
    let error = Namespace::open(store(&dir), &too_long).await.unwrap_err();
    assert!(matches!(error, Error::InvalidWrite(_)));
    assert!(store(&dir).list("").await.unwrap().is_empty());
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
