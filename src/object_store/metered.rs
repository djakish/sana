//! Object-store decorator that counts backend traffic into a shared [`Metrics`].
//!
//! Wrap it *below* the cache so cache hits, which never reach the backend, do
//! not register as object-store round trips: `Caching(Metered(Fs))`. Each method
//! counts the attempt (including failures) and its wall-clock latency; byte
//! counters record successful payloads, plus every compare-and-set rejection as
//! a CAS mismatch.

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use super::{GetResult, ObjectMeta, ObjectStore, ObjectVersion};
use crate::error::{Error, Result};
use crate::metrics::{Metrics, ObjectStoreMetrics};

pub struct MeteredObjectStore {
    inner: Arc<dyn ObjectStore>,
    metrics: Arc<Metrics>,
}

impl MeteredObjectStore {
    pub fn new(inner: Arc<dyn ObjectStore>, metrics: Arc<Metrics>) -> Self {
        Self { inner, metrics }
    }
}

#[async_trait]
impl ObjectStore for MeteredObjectStore {
    async fn get(&self, key: &str) -> Result<GetResult> {
        let os = &self.metrics.object_store;
        ObjectStoreMetrics::incr(&os.gets);
        let result = os.request_latency.time(self.inner.get(key)).await?;
        ObjectStoreMetrics::add(&os.get_bytes, result.bytes.len() as u64);
        Ok(result)
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        let os = &self.metrics.object_store;
        ObjectStoreMetrics::incr(&os.get_ranges);
        let bytes = os
            .request_latency
            .time(self.inner.get_range(key, range))
            .await?;
        ObjectStoreMetrics::add(&os.range_bytes, bytes.len() as u64);
        Ok(bytes)
    }

    async fn put(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        let os = &self.metrics.object_store;
        ObjectStoreMetrics::incr(&os.puts);
        ObjectStoreMetrics::add(&os.put_bytes, bytes.len() as u64);
        os.request_latency.time(self.inner.put(key, bytes)).await
    }

    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        let os = &self.metrics.object_store;
        ObjectStoreMetrics::incr(&os.puts_if_absent);
        ObjectStoreMetrics::add(&os.put_bytes, bytes.len() as u64);
        os.request_latency
            .time(self.inner.put_if_absent(key, bytes))
            .await
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> Result<ObjectVersion> {
        let os = &self.metrics.object_store;
        ObjectStoreMetrics::incr(&os.compare_and_sets);
        ObjectStoreMetrics::add(&os.put_bytes, bytes.len() as u64);
        match os
            .request_latency
            .time(self.inner.compare_and_set(key, expected, bytes))
            .await
        {
            Ok(version) => Ok(version),
            Err(error) => {
                if matches!(error, Error::CasMismatch { .. }) {
                    ObjectStoreMetrics::incr(&os.cas_mismatches);
                }
                Err(error)
            }
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let os = &self.metrics.object_store;
        ObjectStoreMetrics::incr(&os.lists);
        os.request_latency.time(self.inner.list(prefix)).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let os = &self.metrics.object_store;
        ObjectStoreMetrics::incr(&os.deletes);
        os.request_latency.time(self.inner.delete(key)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{FsObjectStore, version_of};

    fn metered() -> (MeteredObjectStore, Arc<Metrics>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let metrics = Metrics::shared();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        (
            MeteredObjectStore::new(inner, metrics.clone()),
            metrics,
            dir,
        )
    }

    #[tokio::test]
    async fn counts_requests_and_bytes() {
        let (store, metrics, _dir) = metered();
        store.put("k", Bytes::from_static(b"hello")).await.unwrap();
        let got = store.get("k").await.unwrap();
        assert_eq!(got.bytes, Bytes::from_static(b"hello"));
        store.get_range("k", 0..2).await.unwrap();
        store.list("").await.unwrap();
        store.delete("k").await.unwrap();

        let snapshot = metrics.snapshot().object_store;
        assert_eq!(snapshot.puts, 1);
        assert_eq!(snapshot.gets, 1);
        assert_eq!(snapshot.get_ranges, 1);
        assert_eq!(snapshot.lists, 1);
        assert_eq!(snapshot.deletes, 1);
        assert_eq!(snapshot.put_bytes, 5);
        assert_eq!(snapshot.get_bytes, 5);
        assert_eq!(snapshot.range_bytes, 2);
        // Every backend round trip lands one latency observation.
        assert_eq!(snapshot.request_latency.count(), 5);
    }

    #[tokio::test]
    async fn counts_cas_mismatches() {
        let (store, metrics, _dir) = metered();
        let version = store
            .put_if_absent("k", Bytes::from_static(b"v0"))
            .await
            .unwrap();
        store
            .compare_and_set("k", version, Bytes::from_static(b"v1"))
            .await
            .unwrap();
        let stale = version_of(b"v0");
        let error = store
            .compare_and_set("k", stale, Bytes::from_static(b"v2"))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::CasMismatch { .. }));

        let snapshot = metrics.snapshot().object_store;
        assert_eq!(snapshot.puts_if_absent, 1);
        assert_eq!(snapshot.compare_and_sets, 2);
        assert_eq!(snapshot.cas_mismatches, 1);
    }
}
