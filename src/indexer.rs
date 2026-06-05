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

use crate::doc::{DocRecord, encode_id};
use crate::error::Result;
use crate::manifest::{ManifestPointer, NamespaceManifest, SstMeta};
use crate::namespace::{
    Namespace, apply_op, manifest_body_key, manifest_pointer_key, now_ms, op_id,
};
use crate::object_store::ObjectVersion;
use crate::sst::SstWriter;
use crate::value::{Document, Id};

/// Pointer object version (to CAS against), the current pointer, and the body.
type ManifestState = (ObjectVersion, ManifestPointer, NamespaceManifest);

/// Read the manifest pointer and body together; callers need the pointer's
/// object version to CAS against.
async fn read_manifest(ns: &Namespace) -> Result<ManifestState> {
    let ptr = ns.store().get(&manifest_pointer_key(ns.name())).await?;
    let pointer = ManifestPointer::decode(&ptr.bytes)?;
    let body = ns
        .store()
        .get(&manifest_body_key(ns.name(), pointer.generation))
        .await?;
    Ok((ptr.version, pointer, NamespaceManifest::decode(&body.bytes)?))
}

struct BuiltSst {
    bytes: Vec<u8>,
    row_count: u64,
    min_id: Option<Id>,
    max_id: Option<Id>,
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
    let (ptr_version, pointer, mut manifest) = read_manifest(ns).await?;
    let commit = ns.commit_cursor().await?;
    let from_seq = manifest.indexed_cursor.map(|c| c.seq).unwrap_or(0);
    if from_seq >= commit.seq {
        return Ok(false);
    }

    let ops = ns.read_overlay_ops(manifest.indexed_cursor, commit).await?;
    let touched: BTreeSet<Id> = ops.iter().map(|op| op_id(op).clone()).collect();

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
    let new_gen = pointer.generation + 1;
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
    let row_count = merged
        .into_values()
        .filter(|rec| matches!(rec, DocRecord::Present(_)))
        .count() as u64;

    manifest.generation = new_gen;
    manifest.updated_at_ms = now_ms();
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
    manifest.approx_row_count = row_count;
    manifest.approx_logical_bytes = manifest.doc_ssts.iter().map(|m| m.size_bytes).sum();

    commit_manifest(ns, ptr_version, new_gen, &manifest).await?;
    Ok(true)
}

/// Merge all document SSTs into a single file, dropping shadowed values and
/// tombstones. Returns `true` if work was done.
pub async fn compact(ns: &Namespace) -> Result<bool> {
    let (ptr_version, pointer, mut manifest) = read_manifest(ns).await?;
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

    let built = build_sst(&live)?;
    let new_gen = pointer.generation + 1;
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
    manifest.approx_row_count = built.row_count;
    manifest.approx_logical_bytes = built.bytes.len() as u64;
    manifest.doc_ssts = vec![SstMeta {
        key: sst_key,
        size_bytes: built.bytes.len() as u64,
        row_count: built.row_count,
        min_id: built.min_id,
        max_id: built.max_id,
    }];

    commit_manifest(ns, ptr_version, new_gen, &manifest).await?;
    Ok(true)
}

async fn commit_manifest(
    ns: &Namespace,
    expected: ObjectVersion,
    new_gen: u64,
    manifest: &NamespaceManifest,
) -> Result<()> {
    ns.store()
        .put(
            &manifest_body_key(ns.name(), new_gen),
            Bytes::from(manifest.encode()?),
        )
        .await?;
    ns.store()
        .compare_and_set(
            &manifest_pointer_key(ns.name()),
            expected,
            Bytes::from(ManifestPointer::new(new_gen).encode()?),
        )
        .await?;
    Ok(())
}
