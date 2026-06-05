//! Indexing: fold committed WAL into immutable document SSTs and publish them
//! by CAS-advancing the manifest. Two operations for Stage 2:
//!
//! - [`flush`]: turn the WAL delta since `indexed_cursor` into one new SST and
//!   advance `indexed_cursor`. Each touched id is written as a *complete*
//!   resolved document (base from existing SSTs + delta ops) or a tombstone, so
//!   newest-first reads see whole documents.
//! - [`compact`]: merge all document SSTs into one, dropping overwritten values
//!   and tombstones.
//!
//! Both are idempotent: re-running with nothing to do is a no-op, and a lost
//! CAS race leaves an orphaned SST object (harmless; GC is future work).
//! Publishing follows the architecture: write immutable files, then CAS
//! `manifest/current` to a new generation.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;

use crate::attr;
use crate::doc::{DocRecord, encode_id};
use crate::error::Result;
use crate::manifest::{NamespaceManifest, SstMeta, VectorIndexMeta};
use crate::namespace::{Namespace, apply_op, now_ms, op_id};
use crate::schema::ColumnType;
use crate::sst::SstWriter;
use crate::value::{Document, Id};
use crate::vector::{VectorEntry, VectorIndex, vector_to_f32};

struct BuiltSst {
    bytes: Vec<u8>,
    row_count: u64,
    min_id: Option<Id>,
    max_id: Option<Id>,
}

async fn publish_attr_sst(
    ns: &Namespace,
    generation: u64,
    suffix: &str,
    docs: &BTreeMap<Id, Document>,
) -> Result<Vec<SstMeta>> {
    let Some(built) = attr::build_attr_sst(docs)? else {
        return Ok(Vec::new());
    };
    let sst_key = format!(
        "namespaces/{}/index/g/{}/attr/{}.sst",
        ns.name(),
        generation,
        suffix
    );
    ns.store()
        .put(&sst_key, Bytes::from(built.bytes.clone()))
        .await?;
    Ok(vec![SstMeta {
        key: sst_key,
        size_bytes: built.bytes.len() as u64,
        row_count: built.entry_count,
        min_id: None,
        max_id: None,
    }])
}

async fn publish_vector_indexes(
    ns: &Namespace,
    generation: u64,
    manifest: &NamespaceManifest,
    docs: &BTreeMap<Id, Document>,
) -> Result<BTreeMap<String, VectorIndexMeta>> {
    let mut out = BTreeMap::new();
    for (column, spec) in &manifest.schema.columns {
        let ColumnType::Vector { dim, metric, .. } = spec.column_type else {
            continue;
        };

        let mut entries = Vec::new();
        for (id, doc) in docs {
            let Some(vector) = doc.vectors.get(column) else {
                continue;
            };
            entries.push(VectorEntry {
                id: id.clone(),
                vector: vector_to_f32(vector),
                local_id: 0,
            });
        }

        let Some(index) = VectorIndex::build(column.clone(), metric, dim, entries, docs)? else {
            continue;
        };
        let bytes = index.encode()?;
        let key = format!(
            "namespaces/{}/index/g/{}/vector/{}/ivf.bin",
            ns.name(),
            generation,
            object_path_component(column)
        );
        ns.store().put(&key, Bytes::from(bytes.clone())).await?;
        out.insert(
            column.clone(),
            VectorIndexMeta {
                key,
                size_bytes: bytes.len() as u64,
                row_count: index.row_count() as u64,
                centroid_count: index.centroids.len() as u64,
                dim,
                metric,
            },
        );
    }
    Ok(out)
}

fn object_path_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for b in value.as_bytes() {
        use std::fmt::Write;
        write!(&mut out, "{b:02x}").expect("writing to String cannot fail");
    }
    out
}

fn build_sst(records: &BTreeMap<Id, DocRecord>) -> Result<BuiltSst> {
    let mut writer = SstWriter::new();
    let mut built = BuiltSst {
        bytes: Vec::new(),
        row_count: 0,
        min_id: None,
        max_id: None,
    };
    // BTreeMap iterates by Id order, which matches encode_id byte order, so keys
    // are added to the SST strictly increasing.
    for (id, record) in records {
        writer.add(&encode_id(id), &record.encode()?)?;
        if built.min_id.is_none() {
            built.min_id = Some(id.clone());
        }
        built.max_id = Some(id.clone());
        built.row_count += 1;
    }
    built.bytes = writer.finish();
    Ok(built)
}

/// Flush the WAL delta since `indexed_cursor` into a new document SST. Returns
/// `true` if work was done.
pub async fn flush(ns: &Namespace) -> Result<bool> {
    let snapshot = ns.load_manifest_snapshot().await?;
    let mut manifest = snapshot.manifest;
    let commit = ns.commit_cursor().await?;
    let from_seq = manifest.indexed_cursor.map(|c| c.seq).unwrap_or(0);
    if from_seq >= commit.seq {
        return Ok(false);
    }

    let ops = ns.read_overlay_ops(manifest.indexed_cursor, commit).await?;
    let touched: BTreeSet<Id> = ops.iter().map(|op| op_id(op).clone()).collect();
    if touched.is_empty() {
        manifest.generation = snapshot.pointer.generation + 1;
        manifest.updated_at_ms = now_ms();
        manifest.wal_commit_cursor = Some(commit);
        manifest.indexed_cursor = Some(commit);
        ns.publish_manifest(snapshot.pointer_version, &manifest)
            .await?;
        return Ok(true);
    }

    // Load existing SST records once (not one point-get per touched id), then
    // seed each touched id with its resolved base so a Patch in the delta merges
    // onto the full document rather than a fragment.
    let base = ns.sst_records(&manifest).await?;
    let mut docs: BTreeMap<Id, Document> = BTreeMap::new();
    for id in &touched {
        if let Some(DocRecord::Present(doc)) = base.get(id) {
            docs.insert(id.clone(), doc.clone());
        }
    }
    for op in ops {
        apply_op(&mut docs, op);
    }

    // Every touched id gets a record: present if it survived, else a tombstone.
    let records: BTreeMap<Id, DocRecord> = touched
        .into_iter()
        .map(|id| {
            let rec = match docs.get(&id) {
                Some(doc) => DocRecord::Present(doc.clone()),
                None => DocRecord::Deleted,
            };
            (id, rec)
        })
        .collect();

    let built = build_sst(&records)?;
    let new_gen = snapshot.pointer.generation + 1;
    let sst_key = format!(
        "namespaces/{}/index/g/{}/doc/flush-{}.sst",
        ns.name(),
        new_gen,
        commit.seq
    );
    ns.store()
        .put(&sst_key, Bytes::from(built.bytes.clone()))
        .await?;

    // Exact live-row count after this flush: the new records override the base.
    let mut merged = base;
    for (id, rec) in &records {
        merged.insert(id.clone(), rec.clone());
    }
    let live_docs: BTreeMap<Id, Document> = merged
        .iter()
        .filter_map(|(id, rec)| match rec {
            DocRecord::Present(doc) => Some((id.clone(), doc.clone())),
            DocRecord::Deleted => None,
        })
        .collect();
    let row_count = live_docs.len() as u64;
    let attr_ssts =
        publish_attr_sst(ns, new_gen, &format!("full-{}", commit.seq), &live_docs).await?;
    let vector_indexes = publish_vector_indexes(ns, new_gen, &manifest, &live_docs).await?;

    manifest.generation = new_gen;
    manifest.updated_at_ms = now_ms();
    manifest.wal_commit_cursor = Some(commit);
    manifest.indexed_cursor = Some(commit);
    manifest.doc_ssts.insert(
        0,
        SstMeta {
            key: sst_key,
            size_bytes: built.bytes.len() as u64,
            row_count: built.row_count,
            min_id: built.min_id,
            max_id: built.max_id,
        },
    );
    manifest.attr_ssts = attr_ssts;
    manifest.vector_index_generations = vector_indexes
        .keys()
        .map(|column| (column.clone(), new_gen))
        .collect();
    manifest.vector_indexes = vector_indexes;
    manifest.approx_row_count = row_count;
    manifest.approx_logical_bytes = manifest
        .doc_ssts
        .iter()
        .chain(&manifest.attr_ssts)
        .map(|m| m.size_bytes)
        .sum::<u64>()
        + manifest
            .vector_indexes
            .values()
            .map(|m| m.size_bytes)
            .sum::<u64>();

    ns.publish_manifest(snapshot.pointer_version, &manifest)
        .await?;
    Ok(true)
}

/// Merge all document SSTs into a single file, dropping shadowed values and
/// tombstones. Returns `true` if work was done.
pub async fn compact(ns: &Namespace) -> Result<bool> {
    let snapshot = ns.load_manifest_snapshot().await?;
    let mut manifest = snapshot.manifest;
    if manifest.doc_ssts.len() <= 1 {
        return Ok(false);
    }

    // Full compaction: nothing older remains, so tombstones can be dropped.
    let live: BTreeMap<Id, DocRecord> = ns
        .sst_records(&manifest)
        .await?
        .into_iter()
        .filter(|(_, rec)| matches!(rec, DocRecord::Present(_)))
        .collect();
    let live_docs: BTreeMap<Id, Document> = live
        .iter()
        .filter_map(|(id, rec)| match rec {
            DocRecord::Present(doc) => Some((id.clone(), doc.clone())),
            DocRecord::Deleted => None,
        })
        .collect();

    let built = build_sst(&live)?;
    let new_gen = snapshot.pointer.generation + 1;
    let sst_key = format!(
        "namespaces/{}/index/g/{}/doc/compacted.sst",
        ns.name(),
        new_gen
    );
    ns.store()
        .put(&sst_key, Bytes::from(built.bytes.clone()))
        .await?;

    manifest.generation = new_gen;
    manifest.updated_at_ms = now_ms();
    manifest.wal_commit_cursor = Some(ns.commit_cursor().await?);
    manifest.approx_row_count = built.row_count;
    manifest.attr_ssts = publish_attr_sst(ns, new_gen, "full", &live_docs).await?;
    manifest.vector_indexes = publish_vector_indexes(ns, new_gen, &manifest, &live_docs).await?;
    manifest.vector_index_generations = manifest
        .vector_indexes
        .keys()
        .map(|column| (column.clone(), new_gen))
        .collect();
    manifest.approx_logical_bytes = built.bytes.len() as u64
        + manifest.attr_ssts.iter().map(|m| m.size_bytes).sum::<u64>()
        + manifest
            .vector_indexes
            .values()
            .map(|m| m.size_bytes)
            .sum::<u64>();
    manifest.doc_ssts = vec![SstMeta {
        key: sst_key,
        size_bytes: built.bytes.len() as u64,
        row_count: built.row_count,
        min_id: built.min_id,
        max_id: built.max_id,
    }];

    ns.publish_manifest(snapshot.pointer_version, &manifest)
        .await?;
    Ok(true)
}
