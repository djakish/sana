use std::collections::BTreeMap;
use std::sync::Arc;

use sana::error::Error;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::value::{Document, Id, Value, VectorValue};
use sana::wal::WalOp;
use tempfile::TempDir;

fn store(dir: &TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
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
    assert_eq!(ns.lookup(&Id::U64(1)).await.unwrap(), Some(doc_with(1, "v2", 2)));

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
async fn fresh_namespace_replays_empty() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    assert!(ns.replay().await.unwrap().is_empty());
}
