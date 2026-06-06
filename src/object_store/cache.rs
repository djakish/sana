//! Bounded memory cache for immutable, generation-addressed objects.
//!
//! Mutable namespace pointers, WAL commit cursors, queue files, and operation
//! state always bypass the cache. Manifest bodies and index objects are
//! immutable by key, so a resident entry can safely serve both full and ranged
//! reads without revalidation.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use super::{GetResult, ObjectMeta, ObjectStore, ObjectVersion, version_of};
use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub capacity_bytes: usize,
    pub resident_bytes: usize,
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    pub evictions: u64,
    pub admission_rejections: u64,
}

struct CacheEntry {
    bytes: Bytes,
    object_version: ObjectVersion,
    checksum: ObjectVersion,
    last_access: u64,
}

struct CacheState {
    entries: HashMap<String, CacheEntry>,
    capacity_bytes: usize,
    resident_bytes: usize,
    clock: u64,
    hits: u64,
    misses: u64,
    bypasses: u64,
    evictions: u64,
    admission_rejections: u64,
}

impl CacheState {
    fn new(capacity_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity_bytes,
            resident_bytes: 0,
            clock: 0,
            hits: 0,
            misses: 0,
            bypasses: 0,
            evictions: 0,
            admission_rejections: 0,
        }
    }

    fn get(&mut self, key: &str) -> Option<GetResult> {
        self.clock = self.clock.wrapping_add(1);
        let tick = self.clock;
        let entry = self.entries.get_mut(key)?;
        entry.last_access = tick;
        self.hits += 1;
        debug_assert_eq!(version_of(&entry.bytes), entry.checksum);
        Some(GetResult {
            bytes: entry.bytes.clone(),
            version: entry.object_version.clone(),
        })
    }

    fn record_miss(&mut self) {
        self.misses += 1;
    }

    fn record_bypass(&mut self) {
        self.bypasses += 1;
    }

    fn invalidate(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.resident_bytes -= entry.bytes.len();
        }
    }

    fn insert(&mut self, key: String, result: &GetResult) {
        self.invalidate(&key);
        let size = result.bytes.len();
        if size > self.capacity_bytes {
            self.admission_rejections += 1;
            return;
        }

        while self.resident_bytes.saturating_add(size) > self.capacity_bytes {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.invalidate(&victim);
            self.evictions += 1;
        }

        self.clock = self.clock.wrapping_add(1);
        self.resident_bytes += size;
        self.entries.insert(
            key,
            CacheEntry {
                bytes: result.bytes.clone(),
                object_version: result.version.clone(),
                checksum: version_of(&result.bytes),
                last_access: self.clock,
            },
        );
    }

    fn stats(&self) -> CacheStats {
        CacheStats {
            capacity_bytes: self.capacity_bytes,
            resident_bytes: self.resident_bytes,
            entries: self.entries.len(),
            hits: self.hits,
            misses: self.misses,
            bypasses: self.bypasses,
            evictions: self.evictions,
            admission_rejections: self.admission_rejections,
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.resident_bytes = 0;
    }
}

pub struct CachingObjectStore {
    inner: Arc<dyn ObjectStore>,
    state: tokio::sync::Mutex<CacheState>,
}

impl CachingObjectStore {
    pub fn new(inner: Arc<dyn ObjectStore>, capacity_bytes: usize) -> Self {
        Self {
            inner,
            state: tokio::sync::Mutex::new(CacheState::new(capacity_bytes)),
        }
    }

    pub async fn stats(&self) -> CacheStats {
        self.state.lock().await.stats()
    }

    pub async fn clear(&self) {
        self.state.lock().await.clear();
    }

    async fn cached(&self, key: &str) -> Option<GetResult> {
        self.state.lock().await.get(key)
    }

    async fn insert(&self, key: &str, result: &GetResult) {
        self.state.lock().await.insert(key.to_string(), result);
    }

    async fn invalidate(&self, key: &str) {
        self.state.lock().await.invalidate(key);
    }

    async fn record_miss(&self) {
        self.state.lock().await.record_miss();
    }

    async fn record_bypass(&self) {
        self.state.lock().await.record_bypass();
    }
}

fn is_cacheable(key: &str) -> bool {
    key.starts_with("namespaces/") && (key.contains("/manifest/g/") || key.contains("/index/g/"))
}

fn cached_range(result: GetResult, key: &str, range: Range<u64>) -> Result<Bytes> {
    let size = result.bytes.len() as u64;
    if range.start > range.end || range.end > size {
        return Err(Error::InvalidRange {
            start: range.start,
            end: range.end,
            size,
        });
    }
    let start = usize::try_from(range.start)
        .map_err(|_| Error::Corrupt(format!("cached range start overflows usize for {key}")))?;
    let end = usize::try_from(range.end)
        .map_err(|_| Error::Corrupt(format!("cached range end overflows usize for {key}")))?;
    Ok(result.bytes.slice(start..end))
}

#[async_trait]
impl ObjectStore for CachingObjectStore {
    async fn get(&self, key: &str) -> Result<GetResult> {
        if !is_cacheable(key) {
            self.record_bypass().await;
            return self.inner.get(key).await;
        }
        if let Some(result) = self.cached(key).await {
            return Ok(result);
        }

        self.record_miss().await;
        let result = self.inner.get(key).await?;
        self.insert(key, &result).await;
        Ok(result)
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        if !is_cacheable(key) {
            self.record_bypass().await;
            return self.inner.get_range(key, range).await;
        }
        if let Some(result) = self.cached(key).await {
            return cached_range(result, key, range);
        }

        self.record_miss().await;
        self.inner.get_range(key, range).await
    }

    async fn put(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        if is_cacheable(key) {
            self.invalidate(key).await;
        }
        let version = self.inner.put(key, bytes.clone()).await?;
        if is_cacheable(key) {
            self.insert(
                key,
                &GetResult {
                    bytes,
                    version: version.clone(),
                },
            )
            .await;
        }
        Ok(version)
    }

    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        if is_cacheable(key) {
            self.invalidate(key).await;
        }
        let version = self.inner.put_if_absent(key, bytes.clone()).await?;
        if is_cacheable(key) {
            self.insert(
                key,
                &GetResult {
                    bytes,
                    version: version.clone(),
                },
            )
            .await;
        }
        Ok(version)
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> Result<ObjectVersion> {
        if is_cacheable(key) {
            self.invalidate(key).await;
        }
        let version = self
            .inner
            .compare_and_set(key, expected, bytes.clone())
            .await?;
        if is_cacheable(key) {
            self.insert(
                key,
                &GetResult {
                    bytes,
                    version: version.clone(),
                },
            )
            .await;
        }
        Ok(version)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        self.inner.list(prefix).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.invalidate(key).await;
        self.inner.delete(key).await?;
        // A concurrent miss may have fetched the old object while deletion was
        // in flight, so invalidate once more after the backing delete lands.
        self.invalidate(key).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::FsObjectStore;

    #[tokio::test]
    async fn immutable_objects_are_cached_but_mutable_pointers_bypass() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let immutable = "namespaces/a/index/g/1/doc.sst";
        let mutable = "namespaces/a/manifest/current";
        inner
            .put(immutable, Bytes::from_static(b"index"))
            .await
            .unwrap();
        inner.put(mutable, Bytes::from_static(b"v1")).await.unwrap();
        let cache = CachingObjectStore::new(inner.clone(), 1024);

        assert_eq!(cache.get(immutable).await.unwrap().bytes, "index");
        inner.delete(immutable).await.unwrap();
        assert_eq!(cache.get(immutable).await.unwrap().bytes, "index");

        assert_eq!(cache.get(mutable).await.unwrap().bytes, "v1");
        inner.put(mutable, Bytes::from_static(b"v2")).await.unwrap();
        assert_eq!(cache.get(mutable).await.unwrap().bytes, "v2");

        let stats = cache.stats().await;
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.bypasses, 2);
    }

    #[tokio::test]
    async fn ranged_reads_use_a_resident_full_object() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let key = "namespaces/a/index/g/1/doc.sst";
        inner
            .put(key, Bytes::from_static(b"0123456789"))
            .await
            .unwrap();
        let cache = CachingObjectStore::new(inner.clone(), 1024);
        cache.get(key).await.unwrap();
        inner.delete(key).await.unwrap();

        assert_eq!(
            cache.get_range(key, 2..6).await.unwrap(),
            Bytes::from_static(b"2345")
        );
        assert!(matches!(
            cache.get_range(key, 8..12).await,
            Err(Error::InvalidRange { .. })
        ));
    }

    #[tokio::test]
    async fn byte_capacity_evicts_least_recently_used_entry() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let first = "namespaces/a/index/g/1/first.sst";
        let second = "namespaces/a/index/g/1/second.sst";
        inner.put(first, Bytes::from_static(b"123")).await.unwrap();
        inner.put(second, Bytes::from_static(b"456")).await.unwrap();
        let cache = CachingObjectStore::new(inner, 4);

        cache.get(first).await.unwrap();
        cache.get(second).await.unwrap();
        cache.get(second).await.unwrap();
        cache.get(first).await.unwrap();

        let stats = cache.stats().await;
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.resident_bytes, 3);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 3);
        assert_eq!(stats.evictions, 2);
    }

    #[tokio::test]
    async fn oversized_object_is_not_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let key = "namespaces/a/index/g/1/large.sst";
        inner.put(key, Bytes::from_static(b"12345")).await.unwrap();
        let cache = CachingObjectStore::new(inner, 4);

        cache.get(key).await.unwrap();
        cache.get(key).await.unwrap();
        let stats = cache.stats().await;
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.admission_rejections, 2);
    }

    #[tokio::test]
    async fn delete_through_cache_invalidates_resident_object() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
        let key = "namespaces/a/index/g/1/deleted.sst";
        inner.put(key, Bytes::from_static(b"data")).await.unwrap();
        let cache = CachingObjectStore::new(inner, 1024);
        cache.get(key).await.unwrap();

        cache.delete(key).await.unwrap();
        assert!(matches!(cache.get(key).await, Err(Error::NotFound(_))));
        assert_eq!(cache.stats().await.entries, 0);
    }
}
