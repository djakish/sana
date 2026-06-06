use std::sync::Arc;

use sana::error::Error;
use sana::indexer;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::value::{Document, Id, Value};

fn store(dir: &tempfile::TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

fn doc(id: u64, title: &str) -> Document {
    let mut document = Document::new(Id::U64(id));
    document
        .attributes
        .insert("title".into(), Value::String(title.into()));
    document
}

#[tokio::test]
async fn branch_requires_fully_indexed_source() {
    let dir = tempfile::tempdir().unwrap();
    let source = Namespace::create(store(&dir), "source").await.unwrap();
    source.upsert(doc(1, "one")).await.unwrap();

    assert!(matches!(
        source.branch("child").await,
        Err(Error::InvalidWrite(_))
    ));
    assert!(matches!(
        Namespace::open(store(&dir), "child").await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn branch_shares_snapshot_then_diverges_with_its_own_wal() {
    let dir = tempfile::tempdir().unwrap();
    let source = Namespace::create(store(&dir), "source").await.unwrap();
    source.upsert(doc(1, "one")).await.unwrap();
    source.upsert(doc(2, "two")).await.unwrap();
    indexer::flush(&source).await.unwrap();

    let child = source.branch("child").await.unwrap();
    let child_manifest = child.load_manifest().await.unwrap();
    let parent = child_manifest.branch_parent.as_ref().unwrap();
    assert_eq!(parent.namespace, "source");
    assert_eq!(
        parent.generation,
        source.load_manifest().await.unwrap().generation
    );
    assert_eq!(
        child.replay().await.unwrap(),
        source.replay().await.unwrap()
    );
    assert_eq!(child.commit_cursor().await.unwrap().seq, 0);

    child.upsert(doc(1, "child-one")).await.unwrap();
    child.delete(Id::U64(2)).await.unwrap();
    child.upsert(doc(3, "child-three")).await.unwrap();
    let child_docs = child.replay().await.unwrap();
    assert_eq!(child_docs.len(), 2);
    assert_eq!(
        child_docs[&Id::U64(1)].attributes["title"],
        Value::String("child-one".into())
    );
    assert!(child_docs.contains_key(&Id::U64(3)));

    let source_docs = source.replay().await.unwrap();
    assert_eq!(source_docs.len(), 2);
    assert_eq!(
        source_docs[&Id::U64(1)].attributes["title"],
        Value::String("one".into())
    );
    assert!(source_docs.contains_key(&Id::U64(2)));

    assert!(indexer::flush(&child).await.unwrap());
    assert_eq!(child.replay().await.unwrap(), child_docs);
}

#[tokio::test]
async fn source_gc_preserves_objects_referenced_by_branch() {
    let dir = tempfile::tempdir().unwrap();
    let source = Namespace::create(store(&dir), "source").await.unwrap();
    source.upsert(doc(1, "one")).await.unwrap();
    source.upsert(doc(2, "two")).await.unwrap();
    indexer::flush(&source).await.unwrap();
    let child = source.branch("child").await.unwrap();
    let shared_keys = child.load_manifest().await.unwrap().referenced_index_keys();

    source.upsert(doc(1, "source-new")).await.unwrap();
    indexer::flush(&source).await.unwrap();
    assert!(indexer::compact(&source).await.unwrap());

    let report = indexer::gc(&source, false).await.unwrap();
    for key in &shared_keys {
        assert!(
            !report.orphan_keys.contains(key),
            "branch-owned source object was marked orphan: {key}"
        );
    }
    indexer::gc(&source, true).await.unwrap();

    let child_docs = child.replay().await.unwrap();
    assert_eq!(child_docs.len(), 2);
    assert_eq!(
        child_docs[&Id::U64(1)].attributes["title"],
        Value::String("one".into())
    );
}
