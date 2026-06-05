//! Immutable IVF vector index for Stage 4 ANN v0.
//!
//! The implementation is intentionally small and deterministic: build a
//! full-snapshot IVF index per vector column during index publication, store it
//! as one immutable object, then probe centroids and exact-rerank vectors in the
//! selected postings at query time.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::DistanceMetric;
use crate::value::{Id, VectorValue};

const VECTOR_MAGIC: &[u8; 8] = b"SANAVEC1";
const VECTOR_FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 4 + 4 + 4;
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    pub id: Id,
    pub score: f32,
}

impl VectorIndex {
    pub fn build(
        column: impl Into<String>,
        metric: DistanceMetric,
        dim: usize,
        mut entries: Vec<VectorEntry>,
    ) -> Result<Option<Self>> {
        if entries.is_empty() {
            return Ok(None);
        }
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        for entry in &entries {
            validate_query_vector(&entry.vector, dim, "indexed vector")?;
        }

        let cluster_count = cluster_count(entries.len());
        let mut centroids = initial_centroids(&entries, cluster_count);
        let mut assignments = vec![0usize; entries.len()];

        for _ in 0..KMEANS_ITERS {
            assign_entries(&entries, &centroids, metric, &mut assignments)?;
            recompute_centroids(&entries, &assignments, &mut centroids, metric);
        }
        assign_entries(&entries, &centroids, metric, &mut assignments)?;

        let mut postings = (0..cluster_count)
            .map(|centroid_id| VectorPosting {
                centroid_id: centroid_id as u32,
                vectors: Vec::new(),
            })
            .collect::<Vec<_>>();
        for (entry, centroid_id) in entries.into_iter().zip(assignments) {
            postings[centroid_id].vectors.push(entry);
        }

        Ok(Some(Self {
            format_version: VECTOR_FORMAT_VERSION,
            column: column.into(),
            dim,
            metric,
            centroids,
            postings,
        }))
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let body = postcard::to_allocvec(self).map_err(|e| Error::Codec(e.to_string()))?;
        let crc = crc32fast::hash(&body);
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(VECTOR_MAGIC);
        out.extend_from_slice(&VECTOR_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Corrupt(
                "vector index frame shorter than header".into(),
            ));
        }
        if &bytes[0..8] != VECTOR_MAGIC {
            return Err(Error::Corrupt("bad vector index magic".into()));
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != VECTOR_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported vector index version {version}"
            )));
        }
        let body_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        let body = bytes
            .get(HEADER_LEN..HEADER_LEN + body_len)
            .ok_or_else(|| Error::Corrupt("vector index body truncated".into()))?;
        if crc32fast::hash(body) != crc {
            return Err(Error::Corrupt("vector index crc mismatch".into()));
        }
        let index: Self = postcard::from_bytes(body).map_err(|e| Error::Codec(e.to_string()))?;
        if index.format_version != VECTOR_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported vector index body version {}",
                index.format_version
            )));
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

        let mut centroid_scores = self
            .centroids
            .iter()
            .enumerate()
            .map(|(idx, centroid)| Ok((idx, score(query, centroid, metric)?)))
            .collect::<Result<Vec<_>>>()?;
        centroid_scores.sort_by(|a, b| compare_scores(a.1, b.1).then_with(|| a.0.cmp(&b.0)));

        let mut hits = Vec::new();
        for (centroid_id, _) in centroid_scores.into_iter().take(probe_count) {
            let Some(posting) = self.postings.get(centroid_id) else {
                return Err(Error::Corrupt("vector posting id out of bounds".into()));
            };
            for entry in &posting.vectors {
                hits.push(VectorHit {
                    id: entry.id.clone(),
                    score: score(query, &entry.vector, metric)?,
                });
            }
        }
        sort_hits(&mut hits);
        hits.truncate(k);
        Ok(hits)
    }

    pub fn row_count(&self) -> usize {
        self.postings
            .iter()
            .map(|posting| posting.vectors.len())
            .sum()
    }
}

pub fn vector_to_f32(vector: &VectorValue) -> Vec<f32> {
    match vector {
        VectorValue::F32(values) => values.clone(),
        VectorValue::F16(values) => values.iter().map(|bits| f16_to_f32(*bits)).collect(),
    }
}

pub fn score(query: &[f32], candidate: &[f32], metric: DistanceMetric) -> Result<f32> {
    match metric {
        DistanceMetric::L2 => Ok(-query
            .iter()
            .zip(candidate)
            .map(|(a, b)| {
                let d = a - b;
                d * d
            })
            .sum::<f32>()),
        DistanceMetric::Dot => Ok(query.iter().zip(candidate).map(|(a, b)| a * b).sum()),
        DistanceMetric::Cosine => {
            let dot: f32 = query.iter().zip(candidate).map(|(a, b)| a * b).sum();
            let q_norm: f32 = query.iter().map(|v| v * v).sum::<f32>().sqrt();
            let c_norm: f32 = candidate.iter().map(|v| v * v).sum::<f32>().sqrt();
            if q_norm == 0.0 || c_norm == 0.0 {
                return Err(Error::InvalidQuery(
                    "cosine query and candidate vectors must be non-zero".into(),
                ));
            }
            Ok(dot / (q_norm * c_norm))
        }
    }
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

fn f16_to_f32(bits: u16) -> f32 {
    let sign = ((bits & 0x8000) as u32) << 16;
    let exp = (bits >> 10) & 0x1f;
    let frac = (bits & 0x03ff) as u32;

    let f32_bits = match exp {
        0 => {
            if frac == 0 {
                sign
            } else {
                let mut frac = frac;
                let mut exp_shift = -14i32;
                while (frac & 0x0400) == 0 {
                    frac <<= 1;
                    exp_shift -= 1;
                }
                frac &= 0x03ff;
                let exp32 = ((exp_shift + 127) as u32) << 23;
                sign | exp32 | (frac << 13)
            }
        }
        0x1f => sign | 0x7f80_0000 | (frac << 13),
        _ => {
            let exp32 = ((exp as u32) + 112) << 23;
            sign | exp32 | (frac << 13)
        }
    };
    f32::from_bits(f32_bits)
}
