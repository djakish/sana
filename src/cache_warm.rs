//! Manifest-driven cache warming for one namespace snapshot.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::namespace::{Namespace, manifest_body_key_for_pointer};

pub const MAX_CACHE_WARM_CONCURRENCY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheObjectKind {
    Manifest,
    VectorIndex,
    VectorVersionMap,
    Rabitq,
    TextSst,
    AttributeSst,
    DocumentSst,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheWarmObject {
    pub key: String,
    pub size_bytes: u64,
    pub kind: CacheObjectKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheWarmOptions {
    pub max_bytes: u64,
    pub max_concurrency: usize,
}

impl Default for CacheWarmOptions {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_concurrency: 16,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheWarmPlan {
    pub manifest_generation: u64,
    pub objects: Vec<CacheWarmObject>,
    pub planned_bytes: u64,
    pub skipped_objects: usize,
    pub skipped_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheWarmReport {
    pub plan: CacheWarmPlan,
    pub loaded_objects: usize,
    pub loaded_bytes: u64,
}

impl Namespace {
    /// Select immutable objects from one manifest snapshot, respecting a byte
    /// budget. Higher-value metadata/vector objects are admitted first.
    pub async fn cache_warm_plan(&self, max_bytes: u64) -> Result<CacheWarmPlan> {
        let snapshot = self.load_manifest_snapshot().await?;
        let manifest = &snapshot.manifest;
        let mut candidates = Vec::new();

        candidates.push(CacheWarmObject {
            key: manifest_body_key_for_pointer(self.name(), &snapshot.pointer),
            size_bytes: snapshot.body_size_bytes,
            kind: CacheObjectKind::Manifest,
        });

        for meta in manifest.vector_indexes.values() {
            candidates.push(CacheWarmObject {
                key: meta.key.clone(),
                size_bytes: meta.size_bytes,
                kind: CacheObjectKind::VectorIndex,
            });
            if let Some(key) = &meta.version_map_key {
                candidates.push(CacheWarmObject {
                    key: key.clone(),
                    size_bytes: meta.version_map_size_bytes,
                    kind: CacheObjectKind::VectorVersionMap,
                });
            }
            if let Some(key) = &meta.rabitq_key {
                candidates.push(CacheWarmObject {
                    key: key.clone(),
                    size_bytes: meta.rabitq_size_bytes,
                    kind: CacheObjectKind::Rabitq,
                });
            }
            for append in &meta.append_indexes {
                candidates.push(CacheWarmObject {
                    key: append.key.clone(),
                    size_bytes: append.size_bytes,
                    kind: CacheObjectKind::VectorIndex,
                });
                if let Some(key) = &append.rabitq_key {
                    candidates.push(CacheWarmObject {
                        key: key.clone(),
                        size_bytes: append.rabitq_size_bytes,
                        kind: CacheObjectKind::Rabitq,
                    });
                }
            }
        }

        extend_ssts(
            &mut candidates,
            &manifest.text_ssts,
            CacheObjectKind::TextSst,
        );
        extend_ssts(
            &mut candidates,
            &manifest.attr_ssts,
            CacheObjectKind::AttributeSst,
        );
        extend_ssts(
            &mut candidates,
            &manifest.doc_ssts,
            CacheObjectKind::DocumentSst,
        );

        let mut seen = BTreeSet::new();
        let mut objects = Vec::new();
        let mut planned_bytes = 0u64;
        let mut skipped_objects = 0usize;
        let mut skipped_bytes = 0u64;
        for object in candidates {
            if !seen.insert(object.key.clone()) {
                continue;
            }
            if planned_bytes.saturating_add(object.size_bytes) <= max_bytes {
                planned_bytes += object.size_bytes;
                objects.push(object);
            } else {
                skipped_objects += 1;
                skipped_bytes = skipped_bytes.saturating_add(object.size_bytes);
            }
        }

        Ok(CacheWarmPlan {
            manifest_generation: manifest.generation,
            objects,
            planned_bytes,
            skipped_objects,
            skipped_bytes,
        })
    }

    /// Load the selected immutable objects through this namespace's object
    /// store. A [`CachingObjectStore`](crate::object_store::CachingObjectStore)
    /// retains them; a backend without a cache may treat this as a read hint.
    pub async fn hint_cache_warm(&self, options: CacheWarmOptions) -> Result<CacheWarmReport> {
        if !(1..=MAX_CACHE_WARM_CONCURRENCY).contains(&options.max_concurrency) {
            return Err(Error::InvalidQuery(format!(
                "cache warm max_concurrency must be between 1 and {MAX_CACHE_WARM_CONCURRENCY}"
            )));
        }
        let plan = self.cache_warm_plan(options.max_bytes).await?;
        let semaphore = Arc::new(tokio::sync::Semaphore::new(options.max_concurrency));
        let mut loads = tokio::task::JoinSet::new();
        for object in &plan.objects {
            let store = self.store().clone();
            let semaphore = semaphore.clone();
            let object = object.clone();
            loads.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| Error::Corrupt("cache warm semaphore closed".into()))?;
                let result = store.get(&object.key).await?;
                let actual_size = result.bytes.len() as u64;
                if object.size_bytes != 0 && actual_size != object.size_bytes {
                    return Err(Error::Corrupt(format!(
                        "cache warm size mismatch for {}: manifest {}, object {}",
                        object.key, object.size_bytes, actual_size
                    )));
                }
                Ok::<u64, Error>(actual_size)
            });
        }

        let mut loaded_objects = 0usize;
        let mut loaded_bytes = 0u64;
        while let Some(result) = loads.join_next().await {
            let bytes = result
                .map_err(|error| Error::Corrupt(format!("cache warm join error: {error}")))??;
            loaded_objects += 1;
            loaded_bytes = loaded_bytes.saturating_add(bytes);
        }

        Ok(CacheWarmReport {
            plan,
            loaded_objects,
            loaded_bytes,
        })
    }
}

fn extend_ssts(
    objects: &mut Vec<CacheWarmObject>,
    metas: &[crate::manifest::SstMeta],
    kind: CacheObjectKind,
) {
    objects.extend(metas.iter().map(|meta| CacheWarmObject {
        key: meta.key.clone(),
        size_bytes: meta.size_bytes,
        kind,
    }));
}
