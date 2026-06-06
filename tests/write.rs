use std::collections::BTreeMap;
use std::sync::Arc;

use sana::error::Error;
use sana::indexer;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::query::{FilterExpr, RangeBound};
use sana::value::{Document, Id, Value};
use sana::wal::WalOp;
use sana::write::ConditionalWriteOp;

fn store(dir: &tempfile::TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
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
