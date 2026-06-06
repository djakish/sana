//! Shared unindexed-WAL backpressure policy.

use crate::error::{Error, Result};
use crate::manifest::NamespaceManifest;

pub const DEFAULT_MAX_UNINDEXED_WAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(crate) fn unindexed_wal_bytes(
    manifest: &NamespaceManifest,
    committed_wal_bytes: u64,
) -> Result<u64> {
    committed_wal_bytes
        .checked_sub(manifest.indexed_wal_bytes)
        .ok_or_else(|| {
            Error::Corrupt("indexed WAL byte watermark exceeds committed WAL bytes".into())
        })
}

pub(crate) fn enforce_limit(unindexed_bytes: u64, limit_bytes: u64) -> Result<()> {
    if unindexed_bytes > limit_bytes {
        return Err(Error::Backpressure {
            unindexed_bytes,
            limit_bytes,
        });
    }
    Ok(())
}
