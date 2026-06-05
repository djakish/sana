use std::collections::BTreeMap;
use std::sync::Arc;

use sana::indexer;
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
async fn flush_moves_overlay_into_sst() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    ns.upsert(doc_with(2, "beta", 20)).await.unwrap();

    assert!(indexer::flush(&ns).await.unwrap());

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.doc_ssts.len(), 1);
    assert_eq!(manifest.doc_ssts[0].row_count, 2);
    // indexed_cursor caught up to the commit cursor: the overlay is now empty.
    assert_eq!(manifest.indexed_cursor, Some(ns.commit_cursor().await.unwrap()));

    // Reads now come from the SST and are unchanged.
    assert_eq!(ns.lookup(&Id::U64(1)).await.unwrap(), Some(doc_with(1, "alpha", 10)));
    assert_eq!(ns.replay().await.unwrap().len(), 2);
}

#[tokio::test]
async fn flush_is_idempotent_when_up_to_date() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();

    assert!(indexer::flush(&ns).await.unwrap());
    assert!(!indexer::flush(&ns).await.unwrap()); // nothing new to index
    assert_eq!(ns.load_manifest().await.unwrap().doc_ssts.len(), 1);
}

#[tokio::test]
async fn delete_flushes_as_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.delete(Id::U64(1)).await.unwrap();
    indexer::flush(&ns).await.unwrap();

    assert_eq!(ns.lookup(&Id::U64(1)).await.unwrap(), None);
    assert_eq!(ns.replay().await.unwrap().len(), 0);
    assert_eq!(ns.load_manifest().await.unwrap().doc_ssts.len(), 2);
}

#[tokio::test]
async fn newest_sst_wins_across_flushes() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    ns.upsert(doc_with(1, "v1", 1)).await.unwrap();
    indexer::flush(&ns).await.unwrap();
    ns.upsert(doc_with(1, "v2", 2)).await.unwrap();
    indexer::flush(&ns).await.unwrap();

    assert_eq!(ns.load_manifest().await.unwrap().doc_ssts.len(), 2);
    assert_eq!(ns.lookup(&Id::U64(1)).await.unwrap(), Some(doc_with(1, "v2", 2)));
}

#[tokio::test]
async fn patch_after_flush_merges_with_sst_base() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    indexer::flush(&ns).await.unwrap();

    // Patch lands in the overlay; base is in the SST.
    let mut attrs = BTreeMap::new();
    attrs.insert("score".into(), Value::Int(99));
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
    assert_eq!(doc.attributes["title"], Value::String("alpha".into())); // from SST base
    assert_eq!(doc.attributes["score"], Value::Int(99)); // from overlay
    assert_eq!(doc.vectors["embedding"], VectorValue::F32(vec![1.0, 2.0]));

    // Flushing again folds the merged document into a new SST.
    indexer::flush(&ns).await.unwrap();
    let doc = ns.lookup(&Id::U64(1)).await.unwrap().unwrap();
    assert_eq!(doc.attributes["score"], Value::Int(99));
}

#[tokio::test]
async fn patch_then_flush_merges_within_delta() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();

    let mut attrs = BTreeMap::new();
    attrs.insert("score".into(), Value::Int(42));
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

    // Both upsert and patch are in the same unindexed delta.
    indexer::flush(&ns).await.unwrap();
    let doc = ns.lookup(&Id::U64(1)).await.unwrap().unwrap();
    assert_eq!(doc.attributes["title"], Value::String("alpha".into()));
    assert_eq!(doc.attributes["score"], Value::Int(42));
}

#[tokio::test]
async fn compaction_collapses_ssts_and_drops_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    ns.upsert(doc_with(1, "v1", 1)).await.unwrap();
    ns.upsert(doc_with(2, "keep", 2)).await.unwrap();
    indexer::flush(&ns).await.unwrap();
    ns.upsert(doc_with(1, "v2", 2)).await.unwrap(); // overwrite id 1
    ns.delete(Id::U64(2)).await.unwrap(); // tombstone id 2
    indexer::flush(&ns).await.unwrap();

    assert_eq!(ns.load_manifest().await.unwrap().doc_ssts.len(), 2);
    assert!(indexer::compact(&ns).await.unwrap());

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.doc_ssts.len(), 1);
    assert_eq!(manifest.doc_ssts[0].row_count, 1); // only id 1 survives
    assert_eq!(manifest.approx_row_count, 1);

    assert_eq!(ns.lookup(&Id::U64(1)).await.unwrap(), Some(doc_with(1, "v2", 2)));
    assert_eq!(ns.lookup(&Id::U64(2)).await.unwrap(), None);
    assert_eq!(ns.replay().await.unwrap().len(), 1);
}

#[tokio::test]
async fn flush_and_compact_update_stats() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    ns.upsert(doc_with(2, "beta", 20)).await.unwrap();
    indexer::flush(&ns).await.unwrap();

    let m = ns.load_manifest().await.unwrap();
    assert_eq!(m.approx_row_count, 2);
    assert!(m.approx_logical_bytes > 0);
    assert_eq!(
        m.approx_logical_bytes,
        m.doc_ssts.iter().map(|s| s.size_bytes).sum::<u64>()
    );

    // Overwrite one, delete one, flush: live rows drop to 1 (counted across the
    // SST base + the new delta, not just the touched ids).
    ns.upsert(doc_with(1, "alpha2", 11)).await.unwrap();
    ns.delete(Id::U64(2)).await.unwrap();
    indexer::flush(&ns).await.unwrap();
    let m = ns.load_manifest().await.unwrap();
    assert_eq!(m.approx_row_count, 1);
    assert_eq!(
        m.approx_logical_bytes,
        m.doc_ssts.iter().map(|s| s.size_bytes).sum::<u64>()
    );

    // Compaction keeps the count and resets bytes to the single compacted file.
    assert!(indexer::compact(&ns).await.unwrap());
    let m = ns.load_manifest().await.unwrap();
    assert_eq!(m.approx_row_count, 1);
    assert_eq!(m.doc_ssts.len(), 1);
    assert_eq!(m.approx_logical_bytes, m.doc_ssts[0].size_bytes);
}

#[tokio::test]
async fn compaction_noop_with_single_sst() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    indexer::flush(&ns).await.unwrap();
    assert!(!indexer::compact(&ns).await.unwrap());
}

#[tokio::test]
async fn indexed_data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let ns = Namespace::create(store(&dir), "docs").await.unwrap();
        ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
        ns.upsert(doc_with(2, "beta", 20)).await.unwrap();
        indexer::flush(&ns).await.unwrap();
    }
    let ns = Namespace::open(store(&dir), "docs").await.unwrap();
    let docs = ns.replay().await.unwrap();
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[&Id::U64(2)], doc_with(2, "beta", 20));

    // New writes layer on top of the recovered SST.
    ns.upsert(doc_with(3, "gamma", 30)).await.unwrap();
    assert_eq!(ns.replay().await.unwrap().len(), 3);
}

#[tokio::test]
async fn flush_then_write_then_read_merges_layers() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with(1, "alpha", 10)).await.unwrap();
    indexer::flush(&ns).await.unwrap();

    // These live only in the overlay (SST base is empty for them).
    ns.upsert(doc_with(2, "beta", 20)).await.unwrap();
    ns.delete(Id::U64(1)).await.unwrap();

    let docs = ns.replay().await.unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[&Id::U64(2)], doc_with(2, "beta", 20));
    assert_eq!(ns.lookup(&Id::U64(1)).await.unwrap(), None); // overlay tombstone hides SST
}
