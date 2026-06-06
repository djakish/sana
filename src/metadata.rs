//! Service-facing namespace metadata assembled from one manifest snapshot and
//! the durable WAL/pinning control state.

use serde::{Deserialize, Serialize};

use crate::backpressure::unindexed_wal_bytes;
use crate::error::Result;
use crate::manifest::BranchParent;
use crate::namespace::Namespace;
use crate::pinning::PinningController;
use crate::schema::Schema;
use crate::wal::WalCursor;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndexStatus {
    UpToDate,
    Updating,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub status: IndexStatus,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unindexed_bytes: u64,
    pub committed_cursor: WalCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_cursor: Option<WalCursor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetadataPinning {
    pub replicas: u32,
    pub assigned_replicas: usize,
    pub ready_replicas: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub average_utilization: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamespaceMetadata {
    pub namespace: String,
    pub schema: Schema,
    pub approx_logical_bytes: u64,
    pub approx_row_count: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub index: IndexMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinning: Option<MetadataPinning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_parent: Option<BranchParent>,
}

impl Namespace {
    pub async fn metadata(&self) -> Result<NamespaceMetadata> {
        let manifest = self.load_manifest().await?;
        let (committed_cursor, committed_wal_bytes) = self.wal_commit_stats().await?;
        let unindexed_bytes = unindexed_wal_bytes(&manifest, committed_wal_bytes)?;
        let pinning = PinningController::new(self.store().clone())
            .metadata(self.name())
            .await?
            .map(|metadata| MetadataPinning {
                replicas: metadata.replicas,
                assigned_replicas: metadata.assigned_replicas,
                ready_replicas: metadata.ready_replicas,
                average_utilization: metadata.average_utilization,
            });

        Ok(NamespaceMetadata {
            namespace: self.name().to_string(),
            schema: manifest.schema,
            approx_logical_bytes: manifest.approx_logical_bytes,
            approx_row_count: manifest.approx_row_count,
            created_at_ms: manifest.created_at_ms,
            updated_at_ms: manifest.updated_at_ms,
            index: IndexMetadata {
                status: if unindexed_bytes == 0 {
                    IndexStatus::UpToDate
                } else {
                    IndexStatus::Updating
                },
                unindexed_bytes,
                committed_cursor,
                indexed_cursor: manifest.indexed_cursor,
            },
            pinning,
            branch_parent: manifest.branch_parent,
        })
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}
