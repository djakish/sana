//! Namespace manifest: the per-namespace catalog. A query starts by loading it;
//! an indexer publishes work by writing immutable files then CAS-advancing the
//! `manifest/current` pointer to a new immutable `manifest/g/{generation}.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::Schema;
use crate::wal::WalCursor;

pub const MANIFEST_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchParent {
    pub namespace: String,
    pub generation: u64,
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
    #[serde(default)]
    pub vector_index_generations: BTreeMap<String, u64>,
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
            vector_index_generations: BTreeMap::new(),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPointer {
    pub generation: u64,
}

impl ManifestPointer {
    pub fn new(generation: u64) -> Self {
        Self { generation }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|e| Error::Codec(e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| Error::Codec(e.to_string()))
    }
}
