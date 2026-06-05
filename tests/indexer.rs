use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use sana::indexer;
use sana::manifest::{
    ManifestPointer, VectorAppendKind, VectorMaintenanceAction, VectorMaintenancePlan,
    VectorMaintenanceTask, VectorMaintenanceThresholds,
};
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::query::{ApproxVectorQuery, Query};
use sana::schema::DistanceMetric;
use sana::value::{Document, Id, Value, VectorValue};
use sana::vector::{VectorIndex, VectorVersionMap};
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

fn indexed_bytes(manifest: &sana::manifest::NamespaceManifest) -> u64 {
    manifest
        .doc_ssts
        .iter()
        .chain(&manifest.attr_ssts)
        .map(|s| s.size_bytes)
        .sum::<u64>()
        + manifest
            .vector_indexes
            .values()
            .map(|m| {
                m.size_bytes
                    + m.version_map_size_bytes
                    + m.append_indexes
                        .iter()
                        .map(|append| append.size_bytes)
                        .sum::<u64>()
            })
            .sum::<u64>()
}

fn doc_with_vector(id: u64, title: &str, score: i64, vector: [f32; 2]) -> Document {
    let mut doc = doc_with(id, title, score);
    doc.vectors
        .insert("embedding".into(), VectorValue::F32(vector.to_vec()));
    doc
}

async fn overwrite_current_manifest_body(
    object_store: &Arc<dyn ObjectStore>,
    ns: &str,
    manifest: &sana::manifest::NamespaceManifest,
) {
    let pointer = ManifestPointer::decode(
        &object_store
            .get(&format!("namespaces/{ns}/manifest/current"))
            .await
            .unwrap()
            .bytes,
    )
    .unwrap();
    let body_key = pointer
        .body_key
        .unwrap_or_else(|| format!("namespaces/{ns}/manifest/g/{}.json", pointer.generation));
    object_store
        .put(&body_key, Bytes::from(manifest.encode().unwrap()))
        .await
        .unwrap();
}

fn move_index_entry_to_other_cluster(index: &mut VectorIndex, id: Id) -> (u32, u32) {
    let mut moved = None;
    for posting in &mut index.postings {
        if let Some(pos) = posting.vectors.iter().position(|entry| entry.id == id) {
            moved = Some((posting.centroid_id, posting.vectors.remove(pos)));
            break;
        }
    }
    let (from_cluster, mut entry) = moved.expect("entry exists in vector index");
    let to_cluster = index
        .postings
        .iter()
        .map(|posting| posting.centroid_id)
        .find(|cluster_id| *cluster_id != from_cluster)
        .expect("test index has at least two clusters");
    entry.local_id = index.postings[to_cluster as usize].vectors.len() as u32;
    index.postings[to_cluster as usize].vectors.push(entry);

    index.addresses.clear();
    for posting in &mut index.postings {
        for (local_id, entry) in posting.vectors.iter_mut().enumerate() {
            entry.local_id = local_id as u32;
            index.addresses.push(sana::vector::VectorAddress {
                id: entry.id.clone(),
                cluster_id: posting.centroid_id,
                local_id: entry.local_id,
                version: entry.version,
            });
        }
    }
    index.addresses.sort_by(|a, b| a.id.cmp(&b.id));
    (from_cluster, to_cluster)
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
    assert_eq!(manifest.attr_ssts.len(), 1);
    assert_eq!(manifest.doc_ssts[0].row_count, 2);
    // indexed_cursor caught up to the commit cursor: the overlay is now empty.
    assert_eq!(
        manifest.indexed_cursor,
        Some(ns.commit_cursor().await.unwrap())
    );

    // Reads now come from the SST and are unchanged.
    assert_eq!(
        ns.lookup(&Id::U64(1)).await.unwrap(),
        Some(doc_with(1, "alpha", 10))
    );
    assert_eq!(ns.replay().await.unwrap().len(), 2);
}

#[tokio::test]
async fn flush_publishes_vector_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    ns.upsert(doc_with_vector(1, "alpha", 10, [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc_with_vector(2, "beta", 20, [2.0, 0.0]))
        .await
        .unwrap();

    assert!(indexer::flush(&ns).await.unwrap());

    let manifest = ns.load_manifest().await.unwrap();
    let meta = manifest.vector_indexes.get("embedding").unwrap();
    assert_eq!(meta.row_count, 2);
    assert_eq!(meta.dim, 2);
    assert!(meta.centroid_count >= 1);
    assert!(meta.append_indexes.is_empty());
    assert_eq!(
        manifest.vector_index_generations["embedding"],
        manifest.generation
    );
    assert_eq!(manifest.approx_logical_bytes, indexed_bytes(&manifest));

    let index = VectorIndex::decode(&object_store.get(&meta.key).await.unwrap().bytes).unwrap();
    assert_eq!(index.addresses.len(), 2);
    assert!(
        index
            .addresses
            .iter()
            .any(|addr| addr.id == Id::U64(1) && addr.local_id == 0 && addr.version > 0)
    );
    assert!(
        index
            .postings
            .iter()
            .flat_map(|posting| &posting.vectors)
            .all(|entry| entry.version == manifest.generation)
    );
    assert!(index.filter_index.columns.contains_key("title"));
    assert!(index.filter_index.columns.contains_key("score"));

    let version_map = VectorVersionMap::decode(
        &object_store
            .get(meta.version_map_key.as_ref().unwrap())
            .await
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(
        version_map.live_version(&Id::U64(1)),
        Some(manifest.generation)
    );
    assert!(version_map.is_live(&Id::U64(2), manifest.generation));

    let mask = index
        .filter_mask_by_value("title", |value| value == &Value::String("alpha".into()))
        .unwrap();
    let hits = index
        .search_with_filter(
            &[0.0, 0.0],
            2,
            Some(16),
            Some(DistanceMetric::L2),
            Some(&mask),
        )
        .unwrap();
    let ids: Vec<Id> = hits.into_iter().map(|hit| hit.id).collect();
    assert_eq!(ids, vec![Id::U64(1)]);
}

#[tokio::test]
async fn second_flush_publishes_vector_append_delta() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    ns.upsert(doc_with_vector(1, "base-a", 10, [10.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc_with_vector(2, "base-b", 20, [20.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let first_manifest = ns.load_manifest().await.unwrap();
    let first_generation = first_manifest.generation;
    let first_meta = first_manifest
        .vector_indexes
        .get("embedding")
        .unwrap()
        .clone();

    ns.upsert(doc_with_vector(3, "append", 30, [0.05, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    let meta = manifest.vector_indexes.get("embedding").unwrap();
    assert_eq!(meta.key, first_meta.key);
    assert_eq!(meta.size_bytes, first_meta.size_bytes);
    assert_eq!(meta.row_count, 3);
    assert_eq!(meta.append_indexes.len(), 1);
    assert_eq!(
        manifest.vector_index_generations["embedding"],
        manifest.generation
    );
    assert_eq!(manifest.approx_logical_bytes, indexed_bytes(&manifest));

    let append_meta = &meta.append_indexes[0];
    assert_eq!(append_meta.generation, manifest.generation);
    assert_eq!(append_meta.row_count, 1);
    assert!(append_meta.size_bytes > 0);

    let append =
        VectorIndex::decode(&object_store.get(&append_meta.key).await.unwrap().bytes).unwrap();
    assert_eq!(append.row_count(), 1);
    assert_eq!(append.centroids.len() as u64, first_meta.centroid_count);
    assert_eq!(append.addresses.len(), 1);
    assert_eq!(append.addresses[0].id, Id::U64(3));
    assert_eq!(append.addresses[0].version, manifest.generation);

    let version_map = VectorVersionMap::decode(
        &object_store
            .get(meta.version_map_key.as_ref().unwrap())
            .await
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(
        version_map.live_version(&Id::U64(1)),
        Some(first_generation)
    );
    assert_eq!(
        version_map.live_version(&Id::U64(3)),
        Some(manifest.generation)
    );
}

#[tokio::test]
async fn append_flush_plans_overfull_vector_posting_split() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with_vector(1, "base-a", 10, [1.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc_with_vector(2, "base-b", 20, [2.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    ns.upsert(doc_with_vector(3, "append-a", 30, [3.0, 0.0]))
        .await
        .unwrap();
    ns.upsert(doc_with_vector(4, "append-b", 40, [4.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    let meta = manifest.vector_indexes.get("embedding").unwrap();
    assert_eq!(meta.append_indexes.len(), 1);

    let plan = meta
        .maintenance_plan
        .as_ref()
        .expect("overfull posting should publish a maintenance plan");
    let split = plan
        .tasks
        .iter()
        .find(|task| task.action == VectorMaintenanceAction::Split)
        .expect("overfull posting should be planned for split");
    assert!(split.live_rows > plan.thresholds.max_posting_rows);
    assert_eq!(split.partner_cluster_id, None);
    assert!(!split.neighbor_cluster_ids.contains(&split.cluster_id));
}

#[tokio::test]
async fn vector_maintenance_publishes_local_rebuild_delta() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();
    ns.upsert(doc_with_vector(1, "north", 10, [0.0, 10.0]))
        .await
        .unwrap();
    ns.upsert(doc_with_vector(2, "east", 20, [10.0, 0.0]))
        .await
        .unwrap();
    indexer::flush(&ns).await.unwrap();

    let mut manifest = ns.load_manifest().await.unwrap();
    let meta = manifest.vector_indexes.get("embedding").unwrap().clone();
    let mut index = VectorIndex::decode(&object_store.get(&meta.key).await.unwrap().bytes).unwrap();
    let (correct_cluster, wrong_cluster) =
        move_index_entry_to_other_cluster(&mut index, Id::U64(1));
    object_store
        .put(&meta.key, Bytes::from(index.encode().unwrap()))
        .await
        .unwrap();

    let meta = manifest.vector_indexes.get_mut("embedding").unwrap();
    meta.maintenance_plan = Some(VectorMaintenancePlan {
        thresholds: VectorMaintenanceThresholds {
            min_posting_rows: 1,
            max_posting_rows: 2,
            reassign_neighborhood: 2,
        },
        tasks: vec![VectorMaintenanceTask {
            action: VectorMaintenanceAction::Merge,
            cluster_id: wrong_cluster,
            partner_cluster_id: Some(correct_cluster),
            neighbor_cluster_ids: vec![correct_cluster],
            live_rows: 2,
            stale_rows: 0,
            append_rows: 0,
            total_rows: 2,
        }],
    });
    overwrite_current_manifest_body(&object_store, "docs", &manifest).await;

    assert!(indexer::maintain_vectors(&ns).await.unwrap());

    let maintained = ns.load_manifest().await.unwrap();
    let maintained_meta = maintained.vector_indexes.get("embedding").unwrap();
    assert_eq!(maintained_meta.append_indexes.len(), 1);
    assert_eq!(
        maintained_meta.append_indexes[0].kind,
        VectorAppendKind::LocalRebuild
    );
    assert!(maintained.generation > manifest.generation);
    let version_map = VectorVersionMap::decode(
        &object_store
            .get(maintained_meta.version_map_key.as_ref().unwrap())
            .await
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(
        version_map.live_version(&Id::U64(1)),
        Some(maintained.generation)
    );

    let local_rebuild = VectorIndex::decode(
        &object_store
            .get(&maintained_meta.append_indexes[0].key)
            .await
            .unwrap()
            .bytes,
    )
    .unwrap();
    assert_eq!(local_rebuild.row_count(), 2);
    assert_eq!(local_rebuild.centroids.len(), 1);
    assert!(
        local_rebuild
            .addresses
            .iter()
            .any(|addr| addr.id == Id::U64(1) && addr.version == maintained.generation)
    );

    let ann = ns
        .query(Query {
            filter: None,
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: vec![0.0, 10.0],
                k: 1,
                probes: Some(1),
                metric: Some(DistanceMetric::Cosine),
            }),
        })
        .await
        .unwrap();
    assert_eq!(ann.rows[0].id, Id::U64(1));
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
    assert_eq!(
        ns.lookup(&Id::U64(1)).await.unwrap(),
        Some(doc_with(1, "v2", 2))
    );
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

    assert_eq!(
        ns.lookup(&Id::U64(1)).await.unwrap(),
        Some(doc_with(1, "v2", 2))
    );
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
    assert_eq!(m.approx_logical_bytes, indexed_bytes(&m));

    // Overwrite one, delete one, flush: live rows drop to 1 (counted across the
    // SST base + the new delta, not just the touched ids).
    ns.upsert(doc_with(1, "alpha2", 11)).await.unwrap();
    ns.delete(Id::U64(2)).await.unwrap();
    indexer::flush(&ns).await.unwrap();
    let m = ns.load_manifest().await.unwrap();
    assert_eq!(m.approx_row_count, 1);
    assert_eq!(m.approx_logical_bytes, indexed_bytes(&m));

    // Compaction keeps the count and resets bytes to the compacted index files.
    assert!(indexer::compact(&ns).await.unwrap());
    let m = ns.load_manifest().await.unwrap();
    assert_eq!(m.approx_row_count, 1);
    assert_eq!(m.doc_ssts.len(), 1);
    assert_eq!(m.attr_ssts.len(), 1);
    assert_eq!(m.approx_logical_bytes, indexed_bytes(&m));
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
