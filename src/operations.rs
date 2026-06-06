//! Snapshot operations over immutable manifest generations.

use crate::error::{Error, Result};
use crate::manifest::BranchParent;
use crate::namespace::{Namespace, now_ms};
use crate::wal::WalCursor;

impl Namespace {
    /// Create a zero-copy snapshot branch backed by this namespace's current
    /// immutable index objects and an independent child WAL.
    ///
    /// The source must be fully indexed so no source WAL object is needed to
    /// reproduce the snapshot. Branch publication should not race source GC;
    /// GC itself is an offline/quiescent operation.
    pub async fn branch(&self, child_name: &str) -> Result<Namespace> {
        let snapshot = self.load_manifest_snapshot().await?;
        let commit = self.commit_cursor().await?;
        let indexed = snapshot
            .manifest
            .indexed_cursor
            .unwrap_or_else(|| WalCursor::new(commit.epoch, 0));
        if indexed != commit {
            return Err(Error::InvalidWrite(format!(
                "cannot branch namespace {:?}: WAL is indexed through {:?}, committed through {:?}",
                self.name(),
                indexed,
                commit
            )));
        }

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
}
