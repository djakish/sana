//! The object-store boundary. Everything durable in Sana is expressed against
//! this minimal interface so the engine is agnostic to filesystem vs S3/GCS.
//!
//! Versioning is content-addressed: [`version_of`] hashes the bytes, which
//! gives correct compare-and-set-by-content semantics that survive restarts
//! without any sidecar state. Mutable CAS objects include a monotonically
//! increasing manifest generation, queue job-id counter, or control-plane
//! revision, so the ABA problem inherent to content versioning does not occur
//! in normal operation.

use std::ops::Range;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::Result;

pub mod cache;
pub mod fs;

pub use cache::{CacheStats, CachingObjectStore};
pub use fs::FsObjectStore;

/// An opaque object version token. For the filesystem backend this is a hash
/// of the object's contents; for S3/GCS it will wrap an ETag or generation.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ObjectVersion(pub String);

impl std::fmt::Display for ObjectVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Bytes plus the version observed at read time. Returning both in one call
/// avoids a read-modify-CAS race that a separate `get` + `head` would have.
#[derive(Clone, Debug)]
pub struct GetResult {
    pub bytes: Bytes,
    pub version: ObjectVersion,
}

#[derive(Clone, Debug)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub version: ObjectVersion,
}

#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<GetResult>;

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes>;

    async fn put(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion>;

    /// Write only if the key does not exist. Errors with [`Error::AlreadyExists`].
    ///
    /// [`Error::AlreadyExists`]: crate::error::Error::AlreadyExists
    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion>;

    /// Write only if the current version matches `expected`. Errors with
    /// [`Error::CasMismatch`] otherwise (including when the key is absent).
    ///
    /// [`Error::CasMismatch`]: crate::error::Error::CasMismatch
    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> Result<ObjectVersion>;

    /// List objects under a key prefix. Not for the query hot path; manifests
    /// name exact files to read. Use for recovery, tooling, and offline repair.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>>;

    /// Idempotent delete: succeeds whether or not the key existed (S3 semantics).
    async fn delete(&self, key: &str) -> Result<()>;
}

/// Compute the content-addressed version of an object's bytes. Stable across
/// runs because `DefaultHasher::new` uses a fixed seed.
pub fn version_of(bytes: &[u8]) -> ObjectVersion {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    ObjectVersion(format!("{:016x}", h.finish()))
}
