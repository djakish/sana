//! Snapshot operations over immutable manifest generations.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::manifest::{BranchParent, NamespaceManifest};
use crate::namespace::{
    ManifestSnapshot, Namespace, manifest_pointer_key, now_ms, put_immutable_if_absent,
};
use crate::object_store::{ObjectStore, ObjectVersion, version_of};
use crate::wal::WalCursor;

pub const SNAPSHOT_EXPORT_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyReport {
    pub source_generation: u64,
    pub object_count: usize,
    pub copied_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportObject {
    pub source_key: String,
    pub export_key: String,
    pub size_bytes: u64,
    pub checksum: ObjectVersion,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotExportCatalog {
    pub format_version: u32,
    pub source_namespace: String,
    pub source_generation: u64,
    pub manifest: NamespaceManifest,
    pub objects: Vec<ExportObject>,
}

impl SnapshotExportCatalog {
    pub fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|error| Error::Codec(error.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let catalog: Self =
            serde_json::from_slice(bytes).map_err(|error| Error::Codec(error.to_string()))?;
        if catalog.format_version != SNAPSHOT_EXPORT_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported snapshot export format version {}",
                catalog.format_version
            )));
        }
        Ok(catalog)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportReport {
    pub catalog_key: String,
    pub source_generation: u64,
    pub object_count: usize,
    pub copied_bytes: u64,
}

impl Namespace {
    /// Create a zero-copy snapshot branch backed by this namespace's current
    /// immutable index objects and an independent child WAL.
    ///
    /// The source must be fully indexed so no source WAL object is needed to
    /// reproduce the snapshot. Branch publication should not race source GC;
    /// GC itself is an offline/quiescent operation.
    pub async fn branch(&self, child_name: &str) -> Result<Namespace> {
        let snapshot = self.fully_indexed_snapshot("branch").await?;

        let timestamp_ms = now_ms();
        let source_generation = snapshot.manifest.generation;
        let mut child_manifest = snapshot.manifest;
        child_manifest.namespace = child_name.to_string();
        child_manifest.generation = 0;
        child_manifest.wal_commit_cursor = None;
        child_manifest.indexed_cursor = None;
        child_manifest.branch_parent = Some(BranchParent {
            namespace: self.name().to_string(),
            generation: source_generation,
        });
        child_manifest.created_at_ms = timestamp_ms;
        child_manifest.updated_at_ms = timestamp_ms;

        Namespace::create_from_manifest(self.store().clone(), child_name, child_manifest).await
    }

    /// Copy a fully indexed snapshot into an independent namespace, optionally
    /// on a different object store. All referenced index objects are physically
    /// copied and the destination manifest is rewritten to those new keys.
    pub async fn copy_to(
        &self,
        target_store: Arc<dyn ObjectStore>,
        target_name: &str,
    ) -> Result<CopyReport> {
        match target_store.get(&manifest_pointer_key(target_name)).await {
            Ok(_) => {
                return Err(Error::AlreadyExists(format!("namespace {target_name}")));
            }
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let snapshot = self.fully_indexed_snapshot("copy").await?;
        let destination_root = format!("namespaces/{target_name}/index/g/0/copy");
        let transferred = self
            .transfer_snapshot_objects(target_store.clone(), &snapshot.manifest, &destination_root)
            .await?;

        let timestamp_ms = now_ms();
        let source_generation = snapshot.manifest.generation;
        let mut target_manifest = snapshot.manifest;
        rewrite_manifest_keys(&mut target_manifest, &transferred)?;
        target_manifest.namespace = target_name.to_string();
        target_manifest.generation = 0;
        target_manifest.wal_commit_cursor = None;
        target_manifest.indexed_cursor = None;
        target_manifest.branch_parent = None;
        target_manifest.created_at_ms = timestamp_ms;
        target_manifest.updated_at_ms = timestamp_ms;

        Namespace::create_from_manifest(target_store, target_name, target_manifest).await?;
        Ok(CopyReport {
            source_generation,
            object_count: transferred.len(),
            copied_bytes: transferred.values().map(|object| object.size_bytes).sum(),
        })
    }

    /// Export a fully indexed snapshot to an arbitrary object-store prefix.
    /// The catalog is written last and maps source keys to exported objects.
    pub async fn export_to(
        &self,
        target_store: Arc<dyn ObjectStore>,
        prefix: &str,
    ) -> Result<ExportReport> {
        let snapshot = self.fully_indexed_snapshot("export").await?;
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() {
            return Err(Error::InvalidWrite(
                "snapshot export prefix cannot be empty".into(),
            ));
        }
        let transferred = self
            .transfer_snapshot_objects(target_store.clone(), &snapshot.manifest, prefix)
            .await?;
        let objects: Vec<ExportObject> = transferred.values().cloned().collect();
        let copied_bytes = objects.iter().map(|object| object.size_bytes).sum();
        let catalog = SnapshotExportCatalog {
            format_version: SNAPSHOT_EXPORT_FORMAT_VERSION,
            source_namespace: self.name().to_string(),
            source_generation: snapshot.manifest.generation,
            manifest: snapshot.manifest,
            objects,
        };
        let catalog_key = format!("{prefix}/catalog.json");
        put_immutable_if_absent(
            &target_store,
            &catalog_key,
            bytes::Bytes::from(catalog.encode()?),
        )
        .await?;

        Ok(ExportReport {
            catalog_key,
            source_generation: catalog.source_generation,
            object_count: catalog.objects.len(),
            copied_bytes,
        })
    }

    async fn fully_indexed_snapshot(&self, operation: &str) -> Result<ManifestSnapshot> {
        let snapshot = self.load_manifest_snapshot().await?;
        let commit = self.commit_cursor().await?;
        let indexed = snapshot
            .manifest
            .indexed_cursor
            .unwrap_or_else(|| WalCursor::new(commit.epoch, 0));
        if indexed != commit {
            return Err(Error::InvalidWrite(format!(
                "cannot {operation} namespace {:?}: WAL is indexed through {:?}, committed through {:?}",
                self.name(),
                indexed,
                commit
            )));
        }
        Ok(snapshot)
    }

    async fn transfer_snapshot_objects(
        &self,
        target_store: Arc<dyn ObjectStore>,
        manifest: &NamespaceManifest,
        destination_root: &str,
    ) -> Result<BTreeMap<String, ExportObject>> {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(16));
        let mut transfers = tokio::task::JoinSet::new();
        for (ordinal, source_key) in manifest.referenced_index_keys().into_iter().enumerate() {
            let source_store = self.store().clone();
            let target_store = target_store.clone();
            let semaphore = semaphore.clone();
            let destination_root = destination_root.to_string();
            transfers.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| Error::Corrupt("snapshot transfer semaphore closed".into()))?;
                let source = source_store.get(&source_key).await?;
                let checksum = version_of(&source.bytes);
                let export_key =
                    format!("{destination_root}/objects/{ordinal:08}-{}.bin", checksum.0);
                put_immutable_if_absent(&target_store, &export_key, source.bytes.clone()).await?;
                Ok::<ExportObject, Error>(ExportObject {
                    source_key,
                    export_key,
                    size_bytes: source.bytes.len() as u64,
                    checksum,
                })
            });
        }

        let mut transferred = BTreeMap::new();
        while let Some(result) = transfers.join_next().await {
            let object = result.map_err(|error| {
                Error::Corrupt(format!("snapshot transfer join error: {error}"))
            })??;
            transferred.insert(object.source_key.clone(), object);
        }
        Ok(transferred)
    }
}

fn rewrite_manifest_keys(
    manifest: &mut NamespaceManifest,
    transferred: &BTreeMap<String, ExportObject>,
) -> Result<()> {
    for meta in manifest
        .doc_ssts
        .iter_mut()
        .chain(&mut manifest.attr_ssts)
        .chain(&mut manifest.text_ssts)
    {
        meta.key = transferred_key(transferred, &meta.key)?;
    }
    for meta in manifest.vector_indexes.values_mut() {
        meta.key = transferred_key(transferred, &meta.key)?;
        rewrite_optional_key(transferred, &mut meta.rabitq_key)?;
        rewrite_optional_key(transferred, &mut meta.version_map_key)?;
        for append in &mut meta.append_indexes {
            append.key = transferred_key(transferred, &append.key)?;
            rewrite_optional_key(transferred, &mut append.rabitq_key)?;
        }
    }
    Ok(())
}

fn rewrite_optional_key(
    transferred: &BTreeMap<String, ExportObject>,
    key: &mut Option<String>,
) -> Result<()> {
    if let Some(current) = key {
        *current = transferred_key(transferred, current)?;
    }
    Ok(())
}

fn transferred_key(
    transferred: &BTreeMap<String, ExportObject>,
    source_key: &str,
) -> Result<String> {
    transferred
        .get(source_key)
        .map(|object| object.export_key.clone())
        .ok_or_else(|| {
            Error::Corrupt(format!(
                "snapshot transfer omitted manifest object {source_key}"
            ))
        })
}
