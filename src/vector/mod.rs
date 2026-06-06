//! Immutable IVF vector index for Stage 4 ANN v0.
//!
//! The implementation is intentionally small and deterministic: build a
//! full-snapshot IVF index per vector column during index publication, store it
//! as one immutable object, then probe centroids and exact-rerank vectors in the
//! selected postings at query time.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::frame;
use crate::schema::DistanceMetric;
use crate::value::{Document, Id, VectorValue};

mod filter;
mod kernels;
mod maintenance;

pub use filter::{
    VectorFilterColumn, VectorFilterIndex, VectorFilterMask, VectorFilterRows, VectorFilterValue,
};
pub use kernels::{
    AutoDistanceKernel, DistanceKernel, DistanceKernelKind, ScalarDistanceKernel,
    distance_kernel_kind, score_batch,
};
pub use maintenance::{VectorLocalRebuildDelta, VectorReassignment, VectorReassignmentDelta};

const VECTOR_MAGIC: &[u8; 8] = b"SANAVEC1";
const VERSION_MAP_MAGIC: &[u8; 8] = b"SANAVM1!";
const VECTOR_FORMAT_VERSION: u32 = 1;
const VERSION_MAP_FORMAT_VERSION: u32 = 1;
const KMEANS_ITERS: usize = 8;
const MAX_CLUSTERS: usize = 16;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorIndex {
    pub format_version: u32,
    pub column: String,
    pub dim: usize,
    pub metric: DistanceMetric,
    pub centroids: Vec<Vec<f32>>,
    pub postings: Vec<VectorPosting>,
    pub addresses: Vec<VectorAddress>,
    pub filter_index: VectorFilterIndex,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorPosting {
    pub centroid_id: u32,
    pub vectors: Vec<VectorEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorEntry {
    pub id: Id,
    pub vector: Vec<f32>,
    pub local_id: u32,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorAddress {
    pub id: Id,
    pub cluster_id: u32,
    pub local_id: u32,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    pub id: Id,
    pub version: u64,
    pub score: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorVersionMap {
    pub format_version: u32,
    pub column: String,
    pub versions: BTreeMap<Id, u64>,
}

impl VectorVersionMap {
    pub fn from_index(index: &VectorIndex) -> Self {
        let mut versions = BTreeMap::new();
        for posting in &index.postings {
            for entry in &posting.vectors {
                let version = versions.entry(entry.id.clone()).or_insert(entry.version);
                *version = (*version).max(entry.version);
            }
        }
        Self {
            format_version: VERSION_MAP_FORMAT_VERSION,
            column: index.column.clone(),
            versions,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let body = postcard::to_allocvec(self).map_err(|e| Error::Codec(e.to_string()))?;
        frame::encode(
            VERSION_MAP_MAGIC,
            VERSION_MAP_FORMAT_VERSION,
            &body,
        )
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let body = frame::decode(
            bytes,
            VERSION_MAP_MAGIC,
            VERSION_MAP_FORMAT_VERSION,
            "vector version map",
        )?;
        let map: Self = postcard::from_bytes(body).map_err(|e| Error::Codec(e.to_string()))?;
        if map.format_version != VERSION_MAP_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported vector version map body version {}",
                map.format_version
            )));
        }
        Ok(map)
    }

    pub fn live_version(&self, id: &Id) -> Option<u64> {
        self.versions.get(id).copied()
    }

    pub fn is_live(&self, id: &Id, version: u64) -> bool {
        self.live_version(id) == Some(version)
    }
}

impl VectorIndex {
    pub fn build(
        column: impl Into<String>,
        metric: DistanceMetric,
        dim: usize,
        entries: Vec<VectorEntry>,
        docs: &BTreeMap<Id, Document>,
    ) -> Result<Option<Self>> {
        Self::build_with_cluster_count(column.into(), metric, dim, entries, docs, cluster_count)
    }

    fn build_with_cluster_count(
        column: String,
        metric: DistanceMetric,
        dim: usize,
        mut entries: Vec<VectorEntry>,
        docs: &BTreeMap<Id, Document>,
        choose_cluster_count: impl FnOnce(usize) -> usize,
    ) -> Result<Option<Self>> {
        if entries.is_empty() {
            return Ok(None);
        }
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        for entry in &entries {
            validate_query_vector(&entry.vector, dim, "indexed vector")?;
        }

        let cluster_count = choose_cluster_count(entries.len()).clamp(1, entries.len());
        let mut centroids = initial_centroids(&entries, cluster_count);
        let mut assignments = vec![0usize; entries.len()];

        for _ in 0..KMEANS_ITERS {
            assign_entries(&entries, &centroids, metric, &mut assignments)?;
            recompute_centroids(&entries, &assignments, &mut centroids, metric);
        }
        assign_entries(&entries, &centroids, metric, &mut assignments)?;

        let assigned = entries
            .into_iter()
            .zip(assignments)
            .map(|(entry, cluster_id)| (cluster_id as u32, entry))
            .collect();
        Ok(Some(assemble_index(
            column, metric, dim, centroids, assigned, docs,
        )?))
    }

    pub fn build_append(
        column: impl Into<String>,
        metric: DistanceMetric,
        dim: usize,
        centroids: Vec<Vec<f32>>,
        mut entries: Vec<VectorEntry>,
        docs: &BTreeMap<Id, Document>,
    ) -> Result<Option<Self>> {
        if entries.is_empty() {
            return Ok(None);
        }
        if centroids.is_empty() {
            return Err(Error::Corrupt(
                "cannot append vectors without base centroids".into(),
            ));
        }
        for centroid in &centroids {
            validate_query_vector(centroid, dim, "append centroid")?;
        }
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        for entry in &entries {
            validate_query_vector(&entry.vector, dim, "appended vector")?;
        }

        let mut assignments = vec![0usize; entries.len()];
        assign_entries(&entries, &centroids, metric, &mut assignments)?;

        let assigned = entries
            .into_iter()
            .zip(assignments)
            .map(|(entry, cluster_id)| (cluster_id as u32, entry))
            .collect();
        Ok(Some(assemble_index(
            column.into(),
            metric,
            dim,
            centroids,
            assigned,
            docs,
        )?))
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let body = postcard::to_allocvec(self).map_err(|e| Error::Codec(e.to_string()))?;
        frame::encode(VECTOR_MAGIC, VECTOR_FORMAT_VERSION, &body)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let body = frame::decode(bytes, VECTOR_MAGIC, VECTOR_FORMAT_VERSION, "vector index")?;
        let index: Self = postcard::from_bytes(body).map_err(|e| Error::Codec(e.to_string()))?;
        if index.format_version != VECTOR_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported vector index body version {}",
                index.format_version
            )));
        }
        // `build` never emits a centroid-less index, but a corrupt-yet-CRC-valid
        // object could; guard so `search`'s `clamp(1, centroids.len())` can't panic.
        if index.centroids.is_empty() {
            return Err(Error::Corrupt("vector index has no centroids".into()));
        }
        Ok(index)
    }

    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        probes: Option<usize>,
        metric: Option<DistanceMetric>,
    ) -> Result<Vec<VectorHit>> {
        self.search_with_filter(query, k, probes, metric, None)
    }

    pub fn search_with_filter(
        &self,
        query: &[f32],
        k: usize,
        probes: Option<usize>,
        metric: Option<DistanceMetric>,
        filter: Option<&VectorFilterMask>,
    ) -> Result<Vec<VectorHit>> {
        if k == 0 {
            return Err(Error::InvalidQuery(
                "ANN query k must be greater than zero".into(),
            ));
        }
        validate_query_vector(query, self.dim, "ANN query vector")?;
        let metric = metric.unwrap_or(self.metric);
        let probe_count = probes
            .unwrap_or_else(|| self.centroids.len().min(4))
            .clamp(1, self.centroids.len());

        let centroids = self
            .centroids
            .iter()
            .enumerate()
            .filter(|(idx, _)| filter.is_none_or(|mask| mask.cluster_has_any(*idx)))
            .collect::<Vec<_>>();
        let centroid_vectors = centroids
            .iter()
            .map(|(_, centroid)| centroid.as_slice())
            .collect::<Vec<_>>();
        let mut scores = vec![0.0f32; centroid_vectors.len()];
        score_batch(query, &centroid_vectors, metric, &mut scores)?;
        let mut centroid_scores = centroids
            .into_iter()
            .zip(scores)
            .map(|((idx, _), score)| (idx, score))
            .collect::<Vec<_>>();
        centroid_scores.sort_by(|a, b| compare_scores(a.1, b.1).then_with(|| a.0.cmp(&b.0)));

        let mut hits = Vec::new();
        for (centroid_id, _) in centroid_scores.into_iter().take(probe_count) {
            let Some(posting) = self.postings.get(centroid_id) else {
                return Err(Error::Corrupt("vector posting id out of bounds".into()));
            };
            let entries = posting
                .vectors
                .iter()
                .filter(|entry| {
                    filter.is_none_or(|mask| mask.allows(centroid_id, entry.local_id as usize))
                })
                .collect::<Vec<_>>();
            if entries.is_empty() {
                continue;
            }
            let vectors = entries
                .iter()
                .map(|entry| entry.vector.as_slice())
                .collect::<Vec<_>>();
            let mut scores = vec![0.0f32; vectors.len()];
            score_batch(query, &vectors, metric, &mut scores)?;
            for (entry, score) in entries.into_iter().zip(scores) {
                hits.push(VectorHit {
                    id: entry.id.clone(),
                    version: entry.version,
                    score,
                });
            }
        }
        sort_hits(&mut hits);
        hits.truncate(k);
        Ok(hits)
    }

    /// Per-cluster live-vector counts, indexed by `centroid_id`. Used to size the
    /// trailing-bit trim of filter bitmaps so unused high bits never match.
    fn cluster_row_counts(&self) -> Vec<usize> {
        self.postings
            .iter()
            .map(|posting| posting.vectors.len())
            .collect()
    }

    pub fn row_count(&self) -> usize {
        self.cluster_row_counts().into_iter().sum()
    }
}

/// Bin cluster-assigned entries into postings, stamp each a `local_id`, collect
/// id-sorted addresses, and attach the attribute filter index. The three build
/// paths (full, append, reassigned-delta) share this so they stay identical.
fn assemble_index(
    column: String,
    metric: DistanceMetric,
    dim: usize,
    centroids: Vec<Vec<f32>>,
    assigned: Vec<(u32, VectorEntry)>,
    docs: &BTreeMap<Id, Document>,
) -> Result<VectorIndex> {
    let mut postings = (0..centroids.len())
        .map(|centroid_id| VectorPosting {
            centroid_id: centroid_id as u32,
            vectors: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut addresses = Vec::new();
    for (cluster_id, mut entry) in assigned {
        let Some(posting) = postings.get_mut(cluster_id as usize) else {
            return Err(Error::Corrupt("vector cluster id out of bounds".into()));
        };
        entry.local_id = posting.vectors.len() as u32;
        addresses.push(VectorAddress {
            id: entry.id.clone(),
            cluster_id,
            local_id: entry.local_id,
            version: entry.version,
        });
        posting.vectors.push(entry);
    }
    addresses.sort_by(|a, b| a.id.cmp(&b.id));
    let filter_index = VectorFilterIndex::build(&postings, docs)?;

    Ok(VectorIndex {
        format_version: VECTOR_FORMAT_VERSION,
        column,
        dim,
        metric,
        centroids,
        postings,
        addresses,
        filter_index,
    })
}

pub fn vector_to_f32(vector: &VectorValue) -> Vec<f32> {
    match vector {
        VectorValue::F32(values) => values.clone(),
        VectorValue::F16(values) => values
            .iter()
            .map(|bits| half::f16::from_bits(*bits).to_f32())
            .collect(),
    }
}

pub fn score(query: &[f32], candidate: &[f32], metric: DistanceMetric) -> Result<f32> {
    let candidates = [candidate];
    let mut out = [0.0f32];
    score_batch(query, &candidates, metric, &mut out)?;
    Ok(out[0])
}

pub fn sort_hits(hits: &mut [VectorHit]) {
    hits.sort_by(|a, b| compare_scores(a.score, b.score).then_with(|| a.id.cmp(&b.id)));
}

pub fn recall_at(exact: &[Id], approximate: &[Id], k: usize) -> f64 {
    if k == 0 {
        return 1.0;
    }
    let exact = exact
        .iter()
        .take(k)
        .collect::<std::collections::BTreeSet<_>>();
    let got = approximate
        .iter()
        .take(k)
        .filter(|id| exact.contains(id))
        .count();
    got as f64 / k.min(exact.len()).max(1) as f64
}

fn compare_scores(a: f32, b: f32) -> std::cmp::Ordering {
    b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
}

fn validate_query_vector(vector: &[f32], dim: usize, label: &str) -> Result<()> {
    if vector.len() != dim {
        return Err(Error::InvalidQuery(format!(
            "{label} has dim {}, expected {dim}",
            vector.len()
        )));
    }
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(Error::InvalidQuery(format!(
            "{label} contains a non-finite value"
        )));
    }
    Ok(())
}

fn cluster_count(n: usize) -> usize {
    ((n as f64).sqrt().ceil() as usize)
        .clamp(1, MAX_CLUSTERS)
        .min(n)
}

fn initial_centroids(entries: &[VectorEntry], cluster_count: usize) -> Vec<Vec<f32>> {
    (0..cluster_count)
        .map(|i| entries[i * entries.len() / cluster_count].vector.clone())
        .collect()
}

fn assign_entries(
    entries: &[VectorEntry],
    centroids: &[Vec<f32>],
    metric: DistanceMetric,
    assignments: &mut [usize],
) -> Result<()> {
    for (entry_idx, entry) in entries.iter().enumerate() {
        let mut best = (0usize, f32::NEG_INFINITY);
        for (centroid_idx, centroid) in centroids.iter().enumerate() {
            let score = score(&entry.vector, centroid, metric)?;
            if score > best.1 {
                best = (centroid_idx, score);
            }
        }
        assignments[entry_idx] = best.0;
    }
    Ok(())
}

fn recompute_centroids(
    entries: &[VectorEntry],
    assignments: &[usize],
    centroids: &mut [Vec<f32>],
    metric: DistanceMetric,
) {
    let dim = centroids[0].len();
    let mut sums = vec![vec![0.0f32; dim]; centroids.len()];
    let mut counts = vec![0usize; centroids.len()];

    for (entry, centroid_id) in entries.iter().zip(assignments) {
        counts[*centroid_id] += 1;
        for (sum, value) in sums[*centroid_id].iter_mut().zip(&entry.vector) {
            *sum += *value;
        }
    }

    for (idx, centroid) in centroids.iter_mut().enumerate() {
        if counts[idx] == 0 {
            continue;
        }
        for value in &mut sums[idx] {
            *value /= counts[idx] as f32;
        }
        if metric == DistanceMetric::Cosine {
            normalize(&mut sums[idx]);
        }
        *centroid = sums[idx].clone();
    }
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        return;
    }
    for value in vector {
        *value /= norm;
    }
}
