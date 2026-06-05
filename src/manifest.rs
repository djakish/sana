//! Namespace manifest: the per-namespace catalog. A query starts by loading it;
//! an indexer publishes work by writing immutable files then CAS-advancing the
//! `manifest/current` pointer to a new immutable `manifest/g/{generation}.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::{DistanceMetric, Schema};
use crate::value::Id;
use crate::wal::WalCursor;

pub const MANIFEST_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchParent {
    pub namespace: String,
    pub generation: u64,
}

/// One immutable SST file referenced by the manifest. `min_id`/`max_id` bound
/// the keys so point lookups can skip files that cannot contain a key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SstMeta {
    pub key: String,
    pub size_bytes: u64,
    pub row_count: u64,
    #[serde(default)]
    pub min_id: Option<Id>,
    #[serde(default)]
    pub max_id: Option<Id>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorIndexMeta {
    pub key: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_map_key: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub version_map_size_bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub append_indexes: Vec<VectorAppendMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_plan: Option<VectorMaintenancePlan>,
    pub row_count: u64,
    pub centroid_count: u64,
    pub dim: usize,
    pub metric: DistanceMetric,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorAppendMeta {
    pub key: String,
    pub size_bytes: u64,
    pub row_count: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorMaintenancePlan {
    pub thresholds: VectorMaintenanceThresholds,
    pub tasks: Vec<VectorMaintenanceTask>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorMaintenanceThresholds {
    pub min_posting_rows: u64,
    pub max_posting_rows: u64,
    pub reassign_neighborhood: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorMaintenanceTask {
    pub action: VectorMaintenanceAction,
    pub cluster_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partner_cluster_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub neighbor_cluster_ids: Vec<u32>,
    pub live_rows: u64,
    pub stale_rows: u64,
    pub append_rows: u64,
    pub total_rows: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorMaintenanceAction {
    Split,
    Merge,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The immutable manifest body for one generation. Stored as pretty JSON so it
/// is human-inspectable; deterministic because all maps are `BTreeMap`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamespaceManifest {
    pub format_version: u32,
    pub namespace: String,
    pub generation: u64,
    #[serde(default)]
    pub schema: Schema,
    /// Last WAL position durably committed (None for an empty namespace).
    #[serde(default)]
    pub wal_commit_cursor: Option<WalCursor>,
    /// Last WAL position folded into the index (None until first index build).
    #[serde(default)]
    pub indexed_cursor: Option<WalCursor>,
    /// Document-family SST files, ordered newest-first: on a read, the first
    /// file containing a key wins (a tombstone there hides older files).
    #[serde(default)]
    pub doc_ssts: Vec<SstMeta>,
    /// Attribute-family SST files. Stage 3 writes one full-snapshot postings
    /// SST per indexed manifest generation; the vector is kept for future LSM
    /// levels and compatibility with the document family shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attr_ssts: Vec<SstMeta>,
    #[serde(default)]
    pub vector_index_generations: BTreeMap<String, u64>,
    /// Full-snapshot immutable IVF indexes by vector column.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vector_indexes: BTreeMap<String, VectorIndexMeta>,
    #[serde(default)]
    pub branch_parent: Option<BranchParent>,
    #[serde(default)]
    pub approx_logical_bytes: u64,
    #[serde(default)]
    pub approx_row_count: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl NamespaceManifest {
    /// A fresh, empty namespace at generation 0.
    pub fn new(namespace: impl Into<String>, created_at_ms: u64) -> Self {
        Self {
            format_version: MANIFEST_FORMAT_VERSION,
            namespace: namespace.into(),
            generation: 0,
            schema: Schema::default(),
            wal_commit_cursor: None,
            indexed_cursor: None,
            doc_ssts: Vec::new(),
            attr_ssts: Vec::new(),
            vector_index_generations: BTreeMap::new(),
            vector_indexes: BTreeMap::new(),
            branch_parent: None,
            approx_logical_bytes: 0,
            approx_row_count: 0,
            created_at_ms,
            updated_at_ms: created_at_ms,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|e| Error::Codec(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let m: NamespaceManifest =
            serde_json::from_slice(bytes).map_err(|e| Error::Codec(e.to_string()))?;
        if m.format_version != MANIFEST_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported manifest format version {}",
                m.format_version
            )));
        }
        Ok(m)
    }
}

/// The tiny object at `manifest/current`: it names the live generation. Reading
/// a namespace is: get this pointer, then get `manifest/g/{generation}.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPointer {
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_key: Option<String>,
}

impl ManifestPointer {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            body_key: None,
        }
    }

    pub fn for_body(generation: u64, body_key: impl Into<String>) -> Self {
        Self {
            generation,
            body_key: Some(body_key.into()),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| Error::Codec(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| Error::Codec(e.to_string()))
    }
}
