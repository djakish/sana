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
use crate::doc::{DocRecord, decode_id, encode_id};
use crate::error::Result;
use crate::manifest::{
    NamespaceManifest, SstMeta, VectorAppendKind, VectorAppendMeta, VectorIndexMeta,
    VectorMaintenancePlan, VectorMaintenanceTask,
};
use crate::namespace::{Namespace, apply_op, now_ms, op_id};
use crate::schema::{ColumnType, DistanceMetric};
use crate::sst::SstWriter;
use crate::value::{Document, Id};
use crate::vector::{VectorEntry, VectorIndex, VectorVersionMap, vector_to_f32};

fn vector_family_bytes(meta: &VectorIndexMeta) -> u64 {
    meta.size_bytes
        + meta.version_map_size_bytes
        + meta
            .append_indexes
            .iter()
            .map(|append| append.size_bytes)
            .sum::<u64>()
}

fn maintenance_plan_if_not_empty(plan: VectorMaintenancePlan) -> Option<VectorMaintenancePlan> {
    (!plan.tasks.is_empty()).then_some(plan)
}

struct BuiltSst {
    bytes: Vec<u8>,
    row_count: u64,
    min_id: Option<Id>,
    max_id: Option<Id>,
}

#[derive(Clone, Copy)]
struct VectorColumnPublish<'a> {
    name: &'a str,
    metric: DistanceMetric,
    dim: usize,
}

async fn load_vector_append_segments(
    ns: &Namespace,
    meta: &VectorIndexMeta,
) -> Result<Vec<VectorIndex>> {
    let mut append_segments = Vec::with_capacity(meta.append_indexes.len());
    for append_meta in &meta.append_indexes {
        append_segments.push(VectorIndex::decode(
            &ns.store().get(&append_meta.key).await?.bytes,
        )?);
    }
    Ok(append_segments)
}

async fn load_vector_version_map(
    ns: &Namespace,
    meta: &VectorIndexMeta,
    base: &VectorIndex,
) -> Result<VectorVersionMap> {
    match &meta.version_map_key {
        Some(key) => VectorVersionMap::decode(&ns.store().get(key).await?.bytes),
        None => Ok(VectorVersionMap::from_index(base)),
    }
}

async fn publish_vector_version_map(
    ns: &Namespace,
    generation: u64,
    column: &str,
    version_map: &VectorVersionMap,
) -> Result<(String, u64)> {
    let version_map_key = format!(
        "namespaces/{}/index/g/{}/vector/{}/versions.bin",
        ns.name(),
        generation,
        object_path_component(column)
    );
    let version_map_bytes = version_map.encode()?;
    ns.store()
        .put(&version_map_key, Bytes::from(version_map_bytes.clone()))
        .await?;
    Ok((version_map_key, version_map_bytes.len() as u64))
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
        level: 0,
    }])
}

async fn publish_vector_indexes(
    ns: &Namespace,
    generation: u64,
    manifest: &NamespaceManifest,
    docs: &BTreeMap<Id, Document>,
    touched: Option<&BTreeSet<Id>>,
) -> Result<BTreeMap<String, VectorIndexMeta>> {
    let mut out = BTreeMap::new();
    for (column, spec) in &manifest.schema.columns {
        let ColumnType::Vector { dim, metric, .. } = spec.column_type else {
            continue;
        };
        let vector_column = VectorColumnPublish {
            name: column,
            metric,
            dim,
        };

        if let (Some(prev), Some(touched)) = (manifest.vector_indexes.get(column), touched) {
            if let Some(meta) =
                publish_vector_append(ns, generation, vector_column, prev, docs, touched).await?
            {
                out.insert(column.clone(), meta);
            }
            continue;
        }

        if let Some(meta) = publish_full_vector_index(ns, generation, vector_column, docs).await? {
            out.insert(column.clone(), meta);
        }
    }
    Ok(out)
}

async fn publish_full_vector_index(
    ns: &Namespace,
    generation: u64,
    column: VectorColumnPublish<'_>,
    docs: &BTreeMap<Id, Document>,
) -> Result<Option<VectorIndexMeta>> {
    let entries = vector_entries_for_docs(column.name, generation, docs.iter())?;
    let Some(index) = VectorIndex::build(
        column.name.to_string(),
        column.metric,
        column.dim,
        entries,
        docs,
    )?
    else {
        return Ok(None);
    };
    let bytes = index.encode()?;
    let version_map = VectorVersionMap::from_index(&index);
    let maintenance_plan = maintenance_plan_if_not_empty(index.plan_maintenance(
        &[],
        Some(&version_map),
        index.maintenance_thresholds(),
    )?);
    let component = object_path_component(column.name);
    let key = format!(
        "namespaces/{}/index/g/{}/vector/{}/ivf.bin",
        ns.name(),
        generation,
        component
    );
    ns.store().put(&key, Bytes::from(bytes.clone())).await?;
    let (version_map_key, version_map_size_bytes) =
        publish_vector_version_map(ns, generation, column.name, &version_map).await?;
    Ok(Some(VectorIndexMeta {
        key,
        size_bytes: bytes.len() as u64,
        version_map_key: Some(version_map_key),
        version_map_size_bytes,
        append_indexes: Vec::new(),
        maintenance_plan,
        row_count: index.row_count() as u64,
        centroid_count: index.centroids.len() as u64,
        dim: column.dim,
        metric: column.metric,
    }))
}

async fn publish_vector_append(
    ns: &Namespace,
    generation: u64,
    column: VectorColumnPublish<'_>,
    prev: &VectorIndexMeta,
    docs: &BTreeMap<Id, Document>,
    touched: &BTreeSet<Id>,
) -> Result<Option<VectorIndexMeta>> {
    if prev.dim != column.dim || prev.metric != column.metric {
        return publish_full_vector_index(ns, generation, column, docs).await;
    }

    let base = VectorIndex::decode(&ns.store().get(&prev.key).await?.bytes)?;
    let mut append_segments = load_vector_append_segments(ns, prev).await?;
    let mut version_map = load_vector_version_map(ns, prev, &base).await?;

    let mut touched_docs = Vec::new();
    for id in touched {
        if let Some(doc) = docs.get(id)
            && doc.vectors.contains_key(column.name)
        {
            version_map.versions.insert(id.clone(), generation);
            touched_docs.push((id.clone(), doc.clone()));
        } else {
            version_map.versions.remove(id);
        }
    }

    if version_map.versions.is_empty() {
        return Ok(None);
    }

    let entries = vector_entries_for_docs(
        column.name,
        generation,
        touched_docs.iter().map(|(id, doc)| (id, doc)),
    )?;
    let mut append_indexes = prev.append_indexes.clone();
    let component = object_path_component(column.name);
    if let Some(append) = VectorIndex::build_append(
        column.name.to_string(),
        column.metric,
        column.dim,
        base.centroids.clone(),
        entries,
        docs,
    )? {
        let bytes = append.encode()?;
        let key = format!(
            "namespaces/{}/index/g/{}/vector/{}/append-{}.ivf.bin",
            ns.name(),
            generation,
            component,
            generation
        );
        ns.store().put(&key, Bytes::from(bytes.clone())).await?;
        let size_bytes = bytes.len() as u64;
        append_indexes.push(VectorAppendMeta {
            key,
            size_bytes,
            row_count: append.row_count() as u64,
            generation,
            kind: VectorAppendKind::Append,
        });
        append_segments.push(append);
    }

    let maintenance_plan = maintenance_plan_if_not_empty(base.plan_maintenance(
        &append_segments,
        Some(&version_map),
        base.maintenance_thresholds(),
    )?);

    let (version_map_key, version_map_size_bytes) =
        publish_vector_version_map(ns, generation, column.name, &version_map).await?;

    Ok(Some(VectorIndexMeta {
        key: prev.key.clone(),
        size_bytes: prev.size_bytes,
        version_map_key: Some(version_map_key),
        version_map_size_bytes,
        append_indexes,
        maintenance_plan,
        row_count: version_map.versions.len() as u64,
        centroid_count: prev.centroid_count,
        dim: column.dim,
        metric: column.metric,
    }))
}

fn vector_entries_for_docs<'a>(
    column: &str,
    version: u64,
    docs: impl Iterator<Item = (&'a Id, &'a Document)>,
) -> Result<Vec<VectorEntry>> {
    let mut entries = Vec::new();
    for (id, doc) in docs {
        let Some(vector) = doc.vectors.get(column) else {
            continue;
        };
        entries.push(VectorEntry {
            id: id.clone(),
            vector: vector_to_f32(vector),
            local_id: 0,
            version,
        });
    }
    Ok(entries)
}

fn object_path_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 2);
    for b in value.as_bytes() {
        use std::fmt::Write;
        write!(&mut out, "{b:02x}").expect("writing to String cannot fail");
    }
    out
}

/// The live documents in a resolved record set: present rows, tombstones dropped.
fn live_documents(records: &BTreeMap<Id, DocRecord>) -> BTreeMap<Id, Document> {
    records
        .iter()
        .filter_map(|(id, rec)| match rec {
            DocRecord::Present(doc) => Some((id.clone(), doc.clone())),
            DocRecord::Deleted => None,
        })
        .collect()
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
    let touched_ids = touched.clone();

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
    let live_docs = live_documents(&merged);
    let row_count = live_docs.len() as u64;

    // Attribute postings as a delta: write only the touched live docs and append
    // to the prior levels, instead of rewriting every id's postings. Untouched
    // ids keep their postings in older levels; the query path unions across levels
    // and rechecks. The first flush touches every id, so its delta is already a
    // complete snapshot. Then size-tier to bound read fan-out.
    let touched_live: BTreeMap<Id, Document> = touched_ids
        .iter()
        .filter_map(|id| live_docs.get(id).map(|doc| (id.clone(), doc.clone())))
        .collect();
    let mut attr_ssts = manifest.attr_ssts.clone();
    for meta in
        publish_attr_sst(ns, new_gen, &format!("delta-{}", commit.seq), &touched_live).await?
    {
        attr_ssts.insert(0, meta);
    }
    tier_attr_ssts(ns, new_gen, &mut attr_ssts, commit.seq).await?;

    let vector_indexes =
        publish_vector_indexes(ns, new_gen, &manifest, &live_docs, Some(&touched_ids)).await?;

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
            level: 0,
        },
    );
    // Size-tiered compaction: fold any overflowing level into the next, bounding
    // read fan-out without rewriting the whole index on every flush.
    tier_doc_ssts(ns, new_gen, &mut manifest.doc_ssts, commit.seq).await?;
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
            .map(vector_family_bytes)
            .sum::<u64>();

    ns.publish_manifest(snapshot.pointer_version, &manifest)
        .await?;
    Ok(true)
}

/// Number of runs at one level that triggers a size-tiered merge into the next.
const TIER_TRIGGER: usize = 4;

/// Size-tiered minor compaction over `doc_ssts`. While some level holds at least
/// [`TIER_TRIGGER`] runs, merge that level's runs (newest-wins) into a single
/// run at the next level. Tombstones are *retained* — older levels may still
/// hold the key, so only the full [`compact`] (which merges everything) may drop
/// them. The live document set is unchanged, so attribute/vector families are
/// untouched; this only reorganizes the document family and bounds read
/// fan-out. Old run objects become unreferenced orphans (GC is future work).
///
/// `doc_ssts` is kept ordered by read precedence: lower level (and, within a
/// level, more recently written) first. The merged run is the newest of its new
/// level, so it is inserted just before the first run of a strictly higher level.
async fn tier_doc_ssts(
    ns: &Namespace,
    generation: u64,
    doc_ssts: &mut Vec<SstMeta>,
    commit_seq: u64,
) -> Result<()> {
    let mut step = 0u32;
    loop {
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for meta in doc_ssts.iter() {
            *counts.entry(meta.level).or_default() += 1;
        }
        let Some((&level, _)) = counts.iter().find(|&(_, &count)| count >= TIER_TRIGGER) else {
            return Ok(());
        };

        // Merge this level's runs in precedence order (they are already
        // newest-first in `doc_ssts`); first write of an id wins.
        let mut merged: BTreeMap<Id, DocRecord> = BTreeMap::new();
        for meta in doc_ssts.iter().filter(|meta| meta.level == level) {
            let reader = ns.load_sst(&meta.key).await?;
            for (key, value) in reader.entries()? {
                merged
                    .entry(decode_id(&key)?)
                    .or_insert(DocRecord::decode(&value)?);
            }
        }

        let built = build_sst(&merged)?;
        let sst_key = format!(
            "namespaces/{}/index/g/{}/doc/tier-{}-{}-{}.sst",
            ns.name(),
            generation,
            level + 1,
            commit_seq,
            step
        );
        ns.store()
            .put(&sst_key, Bytes::from(built.bytes.clone()))
            .await?;
        let merged_meta = SstMeta {
            key: sst_key,
            size_bytes: built.bytes.len() as u64,
            row_count: built.row_count,
            min_id: built.min_id,
            max_id: built.max_id,
            level: level + 1,
        };

        doc_ssts.retain(|meta| meta.level != level);
        let pos = doc_ssts
            .iter()
            .position(|meta| meta.level > level)
            .unwrap_or(doc_ssts.len());
        doc_ssts.insert(pos, merged_meta);
        step += 1;
    }
}

/// Size-tiered minor compaction over `attr_ssts`, the analogue of
/// [`tier_doc_ssts`] for the attribute family. While some level holds at least
/// [`TIER_TRIGGER`] runs, union its postings into one run at the next level. Order
/// within `attr_ssts` is irrelevant (the query path unions all levels), so the
/// merged run is simply prepended. Stale entries are retained — only the full
/// [`compact`] rebuild from live documents removes them.
async fn tier_attr_ssts(
    ns: &Namespace,
    generation: u64,
    attr_ssts: &mut Vec<SstMeta>,
    commit_seq: u64,
) -> Result<()> {
    let mut step = 0u32;
    loop {
        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for meta in attr_ssts.iter() {
            *counts.entry(meta.level).or_default() += 1;
        }
        let Some((&level, _)) = counts.iter().find(|&(_, &count)| count >= TIER_TRIGGER) else {
            return Ok(());
        };

        let mut readers = Vec::new();
        for meta in attr_ssts.iter().filter(|meta| meta.level == level) {
            readers.push(ns.load_sst(&meta.key).await?);
        }
        attr_ssts.retain(|meta| meta.level != level);
        let Some(built) = attr::merge_attr_ssts(&readers)? else {
            continue;
        };

        let sst_key = format!(
            "namespaces/{}/index/g/{}/attr/tier-{}-{}-{}.sst",
            ns.name(),
            generation,
            level + 1,
            commit_seq,
            step
        );
        ns.store()
            .put(&sst_key, Bytes::from(built.bytes.clone()))
            .await?;
        attr_ssts.insert(
            0,
            SstMeta {
                key: sst_key,
                size_bytes: built.bytes.len() as u64,
                row_count: built.entry_count,
                min_id: None,
                max_id: None,
                level: level + 1,
            },
        );
        step += 1;
    }
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
    let live_docs = live_documents(&live);

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
    manifest.vector_indexes =
        publish_vector_indexes(ns, new_gen, &manifest, &live_docs, None).await?;
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
            .map(vector_family_bytes)
            .sum::<u64>();
    manifest.doc_ssts = vec![SstMeta {
        key: sst_key,
        size_bytes: built.bytes.len() as u64,
        row_count: built.row_count,
        min_id: built.min_id,
        max_id: built.max_id,
        level: 0,
    }];

    ns.publish_manifest(snapshot.pointer_version, &manifest)
        .await?;
    Ok(true)
}

/// Execute one bounded vector maintenance pass from the manifest's planned
/// split/merge tasks. Returns `true` if it published at least one vector delta.
pub async fn maintain_vectors(ns: &Namespace) -> Result<bool> {
    let snapshot = ns.load_manifest_snapshot().await?;
    let mut manifest = snapshot.manifest;
    let commit = ns.commit_cursor().await?;
    if manifest.indexed_cursor != Some(commit) {
        return Ok(false);
    }

    let new_gen = snapshot.pointer.generation + 1;
    let live_docs = ns.replay().await?;
    let mut vector_indexes = manifest.vector_indexes.clone();
    let mut changed = false;

    for (column, meta) in &manifest.vector_indexes {
        let Some(plan) = &meta.maintenance_plan else {
            continue;
        };

        let base = VectorIndex::decode(&ns.store().get(&meta.key).await?.bytes)?;
        let mut append_segments = load_vector_append_segments(ns, meta).await?;
        let version_map = load_vector_version_map(ns, meta, &base).await?;
        let published = match first_local_rebuild_delta(
            &base,
            &append_segments,
            &version_map,
            plan,
            new_gen,
            &live_docs,
        )? {
            Some((task_idx, task, delta)) => {
                let bytes = delta.index.encode()?;
                let key = format!(
                    "namespaces/{}/index/g/{}/vector/{}/local-rebuild-{}-{}.ivf.bin",
                    ns.name(),
                    new_gen,
                    object_path_component(column),
                    new_gen,
                    task_idx
                );
                ns.store().put(&key, Bytes::from(bytes.clone())).await?;

                let mut append_indexes = meta.append_indexes.clone();
                append_indexes.push(VectorAppendMeta {
                    key,
                    size_bytes: bytes.len() as u64,
                    row_count: delta.index.row_count() as u64,
                    generation: new_gen,
                    kind: VectorAppendKind::LocalRebuild,
                });
                append_segments.push(delta.index);
                let mut maintenance_plan = maintenance_plan_if_not_empty(base.plan_maintenance(
                    &append_segments,
                    Some(&delta.version_map),
                    base.maintenance_thresholds(),
                )?);
                suppress_rebuilt_cluster_tasks(&mut maintenance_plan, &task);
                Some((append_indexes, delta.version_map, maintenance_plan))
            }
            None => {
                let Some((task_idx, delta)) = first_reassignment_delta(
                    &base,
                    &append_segments,
                    &version_map,
                    plan,
                    new_gen,
                    &live_docs,
                )?
                else {
                    continue;
                };

                let bytes = delta.index.encode()?;
                let key = format!(
                    "namespaces/{}/index/g/{}/vector/{}/reassign-{}-{}.ivf.bin",
                    ns.name(),
                    new_gen,
                    object_path_component(column),
                    new_gen,
                    task_idx
                );
                ns.store().put(&key, Bytes::from(bytes.clone())).await?;

                let mut append_indexes = meta.append_indexes.clone();
                append_indexes.push(VectorAppendMeta {
                    key,
                    size_bytes: bytes.len() as u64,
                    row_count: delta.index.row_count() as u64,
                    generation: new_gen,
                    kind: VectorAppendKind::Reassign,
                });
                append_segments.push(delta.index);
                let maintenance_plan = maintenance_plan_if_not_empty(base.plan_maintenance(
                    &append_segments,
                    Some(&delta.version_map),
                    base.maintenance_thresholds(),
                )?);
                Some((append_indexes, delta.version_map, maintenance_plan))
            }
        };

        let Some((append_indexes, version_map, maintenance_plan)) = published else {
            continue;
        };
        let (version_map_key, version_map_size_bytes) =
            publish_vector_version_map(ns, new_gen, column, &version_map).await?;

        vector_indexes.insert(
            column.clone(),
            VectorIndexMeta {
                key: meta.key.clone(),
                size_bytes: meta.size_bytes,
                version_map_key: Some(version_map_key),
                version_map_size_bytes,
                append_indexes,
                maintenance_plan,
                row_count: version_map.versions.len() as u64,
                centroid_count: meta.centroid_count,
                dim: meta.dim,
                metric: meta.metric,
            },
        );
        manifest
            .vector_index_generations
            .insert(column.clone(), new_gen);
        changed = true;
    }

    if !changed {
        return Ok(false);
    }

    manifest.generation = new_gen;
    manifest.updated_at_ms = now_ms();
    manifest.vector_indexes = vector_indexes;
    manifest.approx_logical_bytes = manifest
        .doc_ssts
        .iter()
        .chain(&manifest.attr_ssts)
        .map(|m| m.size_bytes)
        .sum::<u64>()
        + manifest
            .vector_indexes
            .values()
            .map(vector_family_bytes)
            .sum::<u64>();

    ns.publish_manifest(snapshot.pointer_version, &manifest)
        .await?;
    Ok(true)
}

fn first_local_rebuild_delta(
    base: &VectorIndex,
    append_segments: &[VectorIndex],
    version_map: &VectorVersionMap,
    plan: &VectorMaintenancePlan,
    new_version: u64,
    docs: &BTreeMap<Id, Document>,
) -> Result<
    Option<(
        usize,
        VectorMaintenanceTask,
        crate::vector::VectorLocalRebuildDelta,
    )>,
> {
    for (task_idx, task) in plan.tasks.iter().enumerate() {
        if let Some(delta) =
            base.build_local_rebuild_delta(append_segments, version_map, task, new_version, docs)?
        {
            return Ok(Some((task_idx, task.clone(), delta)));
        }
    }
    Ok(None)
}

fn first_reassignment_delta(
    base: &VectorIndex,
    append_segments: &[VectorIndex],
    version_map: &VectorVersionMap,
    plan: &VectorMaintenancePlan,
    new_version: u64,
    docs: &BTreeMap<Id, Document>,
) -> Result<Option<(usize, crate::vector::VectorReassignmentDelta)>> {
    for (task_idx, task) in plan.tasks.iter().enumerate() {
        if let Some(delta) =
            base.build_reassignment_delta(append_segments, version_map, task, new_version, docs)?
        {
            return Ok(Some((task_idx, delta)));
        }
    }
    Ok(None)
}

fn suppress_rebuilt_cluster_tasks(
    maintenance_plan: &mut Option<VectorMaintenancePlan>,
    completed_task: &VectorMaintenanceTask,
) {
    let Some(plan) = maintenance_plan else {
        return;
    };
    let mut completed = BTreeSet::new();
    completed.insert(completed_task.cluster_id);
    if let Some(partner) = completed_task.partner_cluster_id {
        completed.insert(partner);
    }
    completed.extend(completed_task.neighbor_cluster_ids.iter().copied());
    plan.tasks.retain(|task| {
        !completed.contains(&task.cluster_id)
            && task
                .partner_cluster_id
                .is_none_or(|partner| !completed.contains(&partner))
    });
    if plan.tasks.is_empty() {
        *maintenance_plan = None;
    }
}
