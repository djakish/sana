use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use sana::error::Error;
use sana::indexer;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, GetResult, ObjectMeta, ObjectStore, ObjectVersion};
use sana::query::{FilterExpr, RangeBound};
use sana::value::{Document, Id, Value};
use sana::wal::WalOp;
use sana::write::{ConditionalWriteOp, DeleteByFilterRequest, PatchByFilterRequest};

fn store(dir: &tempfile::TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

struct PauseFirstCommitReadStore {
    inner: Arc<dyn ObjectStore>,
    paused: AtomicBool,
    captured: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

impl PauseFirstCommitReadStore {
    fn new(inner: Arc<dyn ObjectStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            paused: AtomicBool::new(false),
            captured: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        })
    }

    async fn wait_until_captured(&self) {
        self.captured.notified().await;
    }

    fn resume(&self) {
        self.resume.notify_one();
    }
}

#[async_trait]
impl ObjectStore for PauseFirstCommitReadStore {
    async fn get(&self, key: &str) -> sana::Result<GetResult> {
        let result = self.inner.get(key).await?;
        if key.ends_with("/wal_commit/current") && !self.paused.swap(true, Ordering::SeqCst) {
            self.captured.notify_one();
            self.resume.notified().await;
        }
        Ok(result)
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
        self.inner.compare_and_set(key, expected, bytes).await
    }

    async fn list(&self, prefix: &str) -> sana::Result<Vec<ObjectMeta>> {
        self.inner.list(prefix).await
    }

    async fn delete(&self, key: &str) -> sana::Result<()> {
        self.inner.delete(key).await
    }
}

fn document(id: u64, version: i64, state: &str) -> Document {
    let mut document = Document::new(Id::U64(id));
    document
        .attributes
        .insert("version".into(), Value::Int(version));
    document
        .attributes
        .insert("state".into(), Value::String(state.into()));
    document
}

fn conditional(operation: WalOp, condition: FilterExpr) -> ConditionalWriteOp {
    ConditionalWriteOp {
        operation,
        condition: Some(condition),
    }
}

fn upsert(document: Document, condition: FilterExpr) -> ConditionalWriteOp {
    ConditionalWriteOp {
        operation: WalOp::Upsert {
            id: document.id.clone(),
            document,
        },
        condition: Some(condition),
    }
}

#[tokio::test]
async fn conditional_upserts_apply_per_existing_snapshot_and_insert_missing_rows() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    namespace.upsert(document(1, 10, "old")).await.unwrap();
    namespace.upsert(document(2, 10, "old")).await.unwrap();

    let result = namespace
        .conditional_write(
            vec![
                upsert(
                    document(1, 20, "updated"),
                    FilterExpr::Range {
                        column: "version".into(),
                        lower: None,
                        upper: Some(RangeBound::Excluded(Value::Int(20))),
                    },
                ),
                upsert(
                    document(2, 20, "must-skip"),
                    FilterExpr::Eq {
                        column: "version".into(),
                        value: Value::Int(999),
                    },
                ),
                // Missing upserts apply unconditionally, even when the
                // condition would not match an existing document.
                upsert(
                    document(3, 1, "inserted"),
                    FilterExpr::Eq {
                        column: "version".into(),
                        value: Value::Int(999),
                    },
                ),
            ],
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.outcome.rows_affected, 2);
    assert_eq!(result.outcome.rows_upserted, 2);
    assert_eq!(result.outcome.applied_ids, vec![Id::U64(1), Id::U64(3)]);
    assert_eq!(result.outcome.skipped_ids, vec![Id::U64(2)]);
    assert_eq!(
        namespace.lookup(&Id::U64(1)).await.unwrap(),
        Some(document(1, 20, "updated"))
    );
    assert_eq!(
        namespace.lookup(&Id::U64(2)).await.unwrap(),
        Some(document(2, 10, "old"))
    );
    assert_eq!(
        namespace.lookup(&Id::U64(3)).await.unwrap(),
        Some(document(3, 1, "inserted"))
    );
}

#[tokio::test]
async fn conditional_patch_and_delete_skip_missing_or_nonmatching_rows() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    namespace.upsert(document(1, 1, "active")).await.unwrap();
    namespace.upsert(document(2, 1, "active")).await.unwrap();

    let mut patch = BTreeMap::new();
    patch.insert("state".into(), Value::String("patched".into()));
    let writes = vec![
        conditional(
            WalOp::Patch {
                id: Id::U64(1),
                attributes: patch.clone(),
                vectors: BTreeMap::new(),
            },
            FilterExpr::Eq {
                column: "state".into(),
                value: Value::String("active".into()),
            },
        ),
        conditional(
            WalOp::Delete { id: Id::U64(2) },
            FilterExpr::Eq {
                column: "state".into(),
                value: Value::String("inactive".into()),
            },
        ),
        conditional(
            WalOp::Patch {
                id: Id::U64(3),
                attributes: patch,
                vectors: BTreeMap::new(),
            },
            FilterExpr::Not(Box::new(FilterExpr::Eq {
                column: "state".into(),
                value: Value::String("never".into()),
            })),
        ),
        ConditionalWriteOp {
            operation: WalOp::Delete { id: Id::U64(4) },
            condition: None,
        },
    ];

    let result = namespace.conditional_write(writes, None).await.unwrap();
    assert_eq!(result.outcome.rows_affected, 1);
    assert_eq!(result.outcome.rows_patched, 1);
    assert_eq!(result.outcome.rows_deleted, 0);
    assert_eq!(result.outcome.applied_ids, vec![Id::U64(1)]);
    assert_eq!(
        result.outcome.skipped_ids,
        vec![Id::U64(2), Id::U64(3), Id::U64(4)]
    );
    assert_eq!(
        namespace
            .lookup(&Id::U64(1))
            .await
            .unwrap()
            .unwrap()
            .attributes["state"],
        Value::String("patched".into())
    );
    assert!(namespace.lookup(&Id::U64(2)).await.unwrap().is_some());
}

#[tokio::test]
async fn concurrent_compare_and_set_writes_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let namespace = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    namespace.upsert(document(1, 0, "initial")).await.unwrap();
    let left = Namespace::open(object_store.clone(), "docs").await.unwrap();
    let right = Namespace::open(object_store, "docs").await.unwrap();
    let expected_zero = FilterExpr::Eq {
        column: "version".into(),
        value: Value::Int(0),
    };

    let (left_result, right_result) = tokio::join!(
        left.conditional_write(
            vec![upsert(document(1, 1, "left"), expected_zero.clone())],
            None,
        ),
        right.conditional_write(vec![upsert(document(1, 2, "right"), expected_zero)], None,)
    );
    let left_result = left_result.unwrap();
    let right_result = right_result.unwrap();
    assert_eq!(
        left_result.outcome.rows_affected + right_result.outcome.rows_affected,
        1
    );
    assert_eq!(
        left_result.outcome.skipped_ids.len() + right_result.outcome.skipped_ids.len(),
        1
    );
    let final_document = left.lookup(&Id::U64(1)).await.unwrap().unwrap();
    assert!(final_document == document(1, 1, "left") || final_document == document(1, 2, "right"));
}

#[tokio::test]
async fn conditional_idempotency_replays_original_outcome_after_wal_gc() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    namespace.upsert(document(1, 1, "old")).await.unwrap();
    let writes = vec![upsert(
        document(1, 2, "new"),
        FilterExpr::Eq {
            column: "version".into(),
            value: Value::Int(1),
        },
    )];

    let first = namespace
        .conditional_write(writes.clone(), Some("conditional-1".into()))
        .await
        .unwrap();
    assert_eq!(first.outcome.rows_affected, 1);
    assert!(indexer::flush(&namespace).await.unwrap());
    indexer::gc(&namespace, true).await.unwrap();

    let retry = namespace
        .conditional_write(writes.clone(), Some("conditional-1".into()))
        .await
        .unwrap();
    assert_eq!(retry, first);
    assert_eq!(namespace.commit_cursor().await.unwrap(), first.cursor);

    let conflict = namespace
        .conditional_write(
            vec![upsert(
                document(1, 3, "conflict"),
                FilterExpr::Eq {
                    column: "version".into(),
                    value: Value::Int(2),
                },
            )],
            Some("conditional-1".into()),
        )
        .await
        .unwrap_err();
    assert!(matches!(conflict, Error::IdempotencyConflict(_)));
    assert_eq!(namespace.commit_cursor().await.unwrap(), first.cursor);
}

#[tokio::test]
async fn skipped_operations_do_not_evolve_schema_but_noop_batch_is_durable() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    namespace.upsert(document(1, 1, "old")).await.unwrap();
    let schema_before = namespace.load_manifest().await.unwrap().schema;

    let mut skipped = document(1, 2, "new");
    skipped
        .attributes
        .insert("future".into(), Value::String("not-applied".into()));
    let result = namespace
        .conditional_write(
            vec![upsert(
                skipped,
                FilterExpr::Eq {
                    column: "version".into(),
                    value: Value::Int(999),
                },
            )],
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.outcome.rows_affected, 0);
    assert_eq!(result.outcome.skipped_ids, vec![Id::U64(1)]);
    assert_eq!(
        namespace.load_manifest().await.unwrap().schema,
        schema_before
    );
    assert_eq!(namespace.commit_cursor().await.unwrap(), result.cursor);
    assert!(indexer::flush(&namespace).await.unwrap());
}

#[tokio::test]
async fn conditional_batches_reject_duplicate_ids_without_advancing_wal() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    let before = namespace.commit_cursor().await.unwrap();
    let condition = FilterExpr::Eq {
        column: "version".into(),
        value: Value::Int(1),
    };

    let error = namespace
        .conditional_write(
            vec![
                upsert(document(1, 1, "one"), condition.clone()),
                conditional(WalOp::Delete { id: Id::U64(1) }, condition),
            ],
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidWrite(_)));
    assert_eq!(namespace.commit_cursor().await.unwrap(), before);
}

#[tokio::test]
async fn patch_by_filter_updates_only_matching_rows() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    namespace.upsert(document(1, 1, "active")).await.unwrap();
    namespace.upsert(document(2, 1, "inactive")).await.unwrap();
    namespace.upsert(document(3, 1, "active")).await.unwrap();
    let mut attributes = BTreeMap::new();
    attributes.insert("state".into(), Value::String("patched".into()));

    let result = namespace
        .patch_by_filter(
            PatchByFilterRequest {
                filter: FilterExpr::Eq {
                    column: "state".into(),
                    value: Value::String("active".into()),
                },
                attributes,
                vectors: BTreeMap::new(),
                max_rows: 10,
                allow_partial: false,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.outcome.rows_patched, 2);
    assert_eq!(result.outcome.applied_ids, vec![Id::U64(1), Id::U64(3)]);
    assert!(!result.outcome.rows_remaining);
    assert_eq!(
        namespace
            .lookup(&Id::U64(2))
            .await
            .unwrap()
            .unwrap()
            .attributes["state"],
        Value::String("inactive".into())
    );
}

#[tokio::test]
async fn delete_by_filter_partial_batches_report_rows_remaining() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    for id in 1..=5 {
        namespace.upsert(document(id, 1, "delete")).await.unwrap();
    }
    let request = DeleteByFilterRequest {
        filter: FilterExpr::Eq {
            column: "state".into(),
            value: Value::String("delete".into()),
        },
        max_rows: 2,
        allow_partial: true,
    };

    let first = namespace
        .delete_by_filter(request.clone(), None)
        .await
        .unwrap();
    assert_eq!(first.outcome.rows_deleted, 2);
    assert_eq!(first.outcome.applied_ids, vec![Id::U64(1), Id::U64(2)]);
    assert!(first.outcome.rows_remaining);

    let second = namespace
        .delete_by_filter(request.clone(), None)
        .await
        .unwrap();
    assert_eq!(second.outcome.applied_ids, vec![Id::U64(3), Id::U64(4)]);
    assert!(second.outcome.rows_remaining);

    let third = namespace.delete_by_filter(request, None).await.unwrap();
    assert_eq!(third.outcome.applied_ids, vec![Id::U64(5)]);
    assert!(!third.outcome.rows_remaining);
    assert!(namespace.replay().await.unwrap().is_empty());
}

#[tokio::test]
async fn filter_mutation_limit_error_does_not_advance_wal() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    namespace.upsert(document(1, 1, "delete")).await.unwrap();
    namespace.upsert(document(2, 1, "delete")).await.unwrap();
    let before = namespace.commit_cursor().await.unwrap();

    let error = namespace
        .delete_by_filter(
            DeleteByFilterRequest {
                filter: FilterExpr::Eq {
                    column: "state".into(),
                    value: Value::String("delete".into()),
                },
                max_rows: 1,
                allow_partial: false,
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidWrite(_)));
    assert_eq!(namespace.commit_cursor().await.unwrap(), before);
    assert_eq!(namespace.replay().await.unwrap().len(), 2);
}

#[tokio::test]
async fn filter_mutation_idempotency_returns_original_result_after_matches_change() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    namespace.upsert(document(1, 1, "old")).await.unwrap();
    namespace.upsert(document(2, 1, "old")).await.unwrap();
    let mut attributes = BTreeMap::new();
    attributes.insert("state".into(), Value::String("new".into()));
    let request = PatchByFilterRequest {
        filter: FilterExpr::Eq {
            column: "state".into(),
            value: Value::String("old".into()),
        },
        attributes,
        vectors: BTreeMap::new(),
        max_rows: 10,
        allow_partial: false,
    };

    let first = namespace
        .patch_by_filter(request.clone(), Some("patch-old".into()))
        .await
        .unwrap();
    assert_eq!(first.outcome.rows_patched, 2);
    assert!(
        namespace
            .query(sana::query::Query {
                filter: Some(request.filter.clone()),
                ..sana::query::Query::all()
            })
            .await
            .unwrap()
            .rows
            .is_empty()
    );

    let retry = namespace
        .patch_by_filter(request, Some("patch-old".into()))
        .await
        .unwrap();
    assert_eq!(retry, first);
    assert_eq!(namespace.commit_cursor().await.unwrap(), first.cursor);
}

#[tokio::test]
async fn patch_by_filter_rechecks_candidates_after_phase_one() {
    let dir = tempfile::tempdir().unwrap();
    let inner = store(&dir);
    let seed = Namespace::create(inner.clone(), "docs").await.unwrap();
    seed.upsert(document(1, 1, "active")).await.unwrap();

    let pausing = PauseFirstCommitReadStore::new(inner.clone());
    let filter_namespace = Namespace::open(pausing.clone(), "docs").await.unwrap();
    let mut attributes = BTreeMap::new();
    attributes.insert("version".into(), Value::Int(99));
    let mutation = tokio::spawn(async move {
        filter_namespace
            .patch_by_filter(
                PatchByFilterRequest {
                    filter: FilterExpr::Eq {
                        column: "state".into(),
                        value: Value::String("active".into()),
                    },
                    attributes,
                    vectors: BTreeMap::new(),
                    max_rows: 10,
                    allow_partial: false,
                },
                None,
            )
            .await
    });

    // Phase one has captured the old commit cursor but has not materialized it.
    pausing.wait_until_captured().await;
    let concurrent = Namespace::open(inner, "docs").await.unwrap();
    concurrent.upsert(document(1, 2, "inactive")).await.unwrap();
    pausing.resume();

    let result = mutation.await.unwrap().unwrap();
    assert_eq!(result.outcome.rows_affected, 0);
    assert_eq!(result.outcome.skipped_ids, vec![Id::U64(1)]);
    assert_eq!(
        concurrent.lookup(&Id::U64(1)).await.unwrap(),
        Some(document(1, 2, "inactive"))
    );
}

#[tokio::test]
async fn zero_match_patch_by_filter_still_validates_patch_schema() {
    let dir = tempfile::tempdir().unwrap();
    let namespace = Namespace::create(store(&dir), "docs").await.unwrap();
    namespace.upsert(document(1, 1, "active")).await.unwrap();
    let before = namespace.commit_cursor().await.unwrap();
    let mut attributes = BTreeMap::new();
    attributes.insert("version".into(), Value::String("invalid".into()));

    let error = namespace
        .patch_by_filter(
            PatchByFilterRequest {
                filter: FilterExpr::Eq {
                    column: "state".into(),
                    value: Value::String("never".into()),
                },
                attributes,
                vectors: BTreeMap::new(),
                max_rows: 10,
                allow_partial: false,
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidSchema(_)));
    assert_eq!(namespace.commit_cursor().await.unwrap(), before);
}
