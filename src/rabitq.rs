//! RaBitQ: 1-bit residual quantization with an unbiased distance estimator.
//!
//! Faithful to Gao & Long, "RaBitQ: Quantizing High-Dimensional Vectors with a
//! Theoretical Error Bound for Approximate Nearest Neighbor Search" (SIGMOD
//! 2024). For each IVF posting we quantize the *normalized* residual `ō =
//! (o − c)/‖o − c‖` after a random orthonormal rotation `P`, storing one sign
//! bit per (padded) dimension plus two scalars: the residual norm `‖o − c‖`
//! and the correction factor `⟨ō_q, ō'⟩` where `ō' = P·ō` and `ō_q` is the
//! bi-valued code `sign(ō'ᵢ)/√n`.
//!
//! The rotation is what makes the single bit per dimension informative: a plain
//! per-dimension sign flip is recoverable and decorrelates nothing, so the
//! quantization error would not concentrate. We use a fast pseudo-orthonormal
//! rotation — random ±1 diagonal followed by a Walsh–Hadamard transform — which
//! mixes every coordinate in `O(n log n)` and is the standard practical stand-in
//! for the paper's dense random orthogonal matrix.
//!
//! Query time uses the unbiased estimator `⟨ō, q_r⟩ ≈ ⟨ō_q, q'⟩ / ⟨ō_q, ō'⟩`
//! (`q' = P·q_r`), from which the L2 distance follows. Quantization itself only
//! needs the segment's centroids, so it also supports an L2 query override over
//! a segment whose clustering metric is cosine.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::frame;
use crate::schema::DistanceMetric;
use crate::value::Id;
use crate::vector::{VectorFilterMask, VectorHit, VectorIndex, score, sort_hits};

const RABITQ_MAGIC: &[u8; 8] = b"SANARBT1";
const RABITQ_FORMAT_VERSION: u32 = 1;
const DEFAULT_EPSILON: f32 = 1.9;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RabitqIndex {
    pub format_version: u32,
    pub column: String,
    pub dim: usize,
    pub padded_dim: usize,
    pub metric: DistanceMetric,
    pub clusters: Vec<RabitqCluster>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RabitqCluster {
    pub centroid_id: u32,
    pub transform_seed: u64,
    pub codes: Vec<RabitqCode>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RabitqCode {
    pub id: Id,
    pub local_id: u32,
    pub version: u64,
    /// `‖o − c‖`, restoring the magnitude the unit-residual code discards.
    pub residual_norm: f32,
    /// Correction factor `⟨ō_q, ō'⟩ = (Σ|ō'ᵢ|)/√n`; the estimator's denominator.
    pub dot_code: f32,
    /// One sign bit per padded dimension: set iff the rotated unit residual is positive.
    pub code_words: Vec<u64>,
}

/// A query residual rotated into one cluster's transformed space, reusable across
/// every code in that cluster so the `O(n log n)` rotation is paid once per probe.
#[derive(Clone, Debug)]
pub struct RotatedQuery {
    rotated: Vec<f32>,
    residual_norm_sq: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RabitqSearchStats {
    pub estimated: u64,
    pub reranked: u64,
    pub pruned: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RabitqSearchOutcome {
    pub hits: Vec<VectorHit>,
    pub stats: RabitqSearchStats,
}

/// Quantize every posting of `index` into RaBitQ codes, one cluster per posting.
pub fn build_index(index: &VectorIndex) -> Result<RabitqIndex> {
    let padded_dim = padded_dim(index.dim);
    let mut clusters = Vec::with_capacity(index.postings.len());
    for posting in &index.postings {
        let centroid_id = posting.centroid_id as usize;
        let Some(centroid) = index.centroids.get(centroid_id) else {
            return Err(Error::Corrupt("vector posting id out of bounds".into()));
        };
        if centroid.len() != index.dim {
            return Err(Error::Corrupt("RaBitQ centroid dimension mismatch".into()));
        }
        let transform_seed = transform_seed(&index.column, posting.centroid_id);
        let mut codes = Vec::with_capacity(posting.vectors.len());
        for entry in &posting.vectors {
            if entry.vector.len() != index.dim {
                return Err(Error::Corrupt("RaBitQ vector dimension mismatch".into()));
            }
            codes.push(encode(
                &entry.id,
                entry.local_id,
                entry.version,
                &entry.vector,
                centroid,
                transform_seed,
                padded_dim,
            ));
        }
        clusters.push(RabitqCluster {
            centroid_id: posting.centroid_id,
            transform_seed,
            codes,
        });
    }
    Ok(RabitqIndex {
        format_version: RABITQ_FORMAT_VERSION,
        column: index.column.clone(),
        dim: index.dim,
        padded_dim,
        metric: DistanceMetric::L2,
        clusters,
    })
}

impl RabitqIndex {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let body = postcard::to_allocvec(self).map_err(|e| Error::Codec(e.to_string()))?;
        Ok(frame::encode(RABITQ_MAGIC, RABITQ_FORMAT_VERSION, &body))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let body = frame::decode(bytes, RABITQ_MAGIC, RABITQ_FORMAT_VERSION, "RaBitQ index")?;
        let index: Self = postcard::from_bytes(body).map_err(|e| Error::Codec(e.to_string()))?;
        if index.format_version != RABITQ_FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported RaBitQ body version {}",
                index.format_version
            )));
        }
        if index.padded_dim < index.dim || !index.padded_dim.is_power_of_two() {
            return Err(Error::Corrupt("invalid RaBitQ padded dimension".into()));
        }
        if index.metric != DistanceMetric::L2 || index.clusters.is_empty() {
            return Err(Error::Corrupt("invalid RaBitQ index topology".into()));
        }
        let code_words = index.padded_dim.div_ceil(64);
        for (cluster_id, cluster) in index.clusters.iter().enumerate() {
            if cluster.centroid_id as usize != cluster_id {
                return Err(Error::Corrupt(
                    "RaBitQ cluster ids are not contiguous".into(),
                ));
            }
            for code in &cluster.codes {
                if code.code_words.len() != code_words
                    || !code.residual_norm.is_finite()
                    || code.residual_norm < 0.0
                    || !code.dot_code.is_finite()
                    || code.dot_code < 0.0
                    || code.dot_code > 1.0 + 1e-5
                {
                    return Err(Error::Corrupt("invalid RaBitQ code".into()));
                }
            }
        }
        Ok(index)
    }

    /// Estimate every eligible vector in the probed clusters, then exact-rerank
    /// only candidates whose confidence lower bound can enter this segment's
    /// top-k. `accept` excludes stale versions and WAL-shadowed ids before rank
    /// pruning, which is required for local top-k to remain globally safe.
    pub fn search_l2_with_filter(
        &self,
        index: &VectorIndex,
        query: &[f32],
        k: usize,
        probes: Option<usize>,
        filter: Option<&VectorFilterMask>,
        mut accept: impl FnMut(&Id, u64) -> bool,
    ) -> Result<RabitqSearchOutcome> {
        validate_search(self, index, query, k)?;
        let probe_count = probes
            .unwrap_or_else(|| index.centroids.len().min(4))
            .clamp(1, index.centroids.len());

        let mut centroid_distances = index
            .centroids
            .iter()
            .enumerate()
            .filter(|(idx, _)| filter.is_none_or(|mask| mask.cluster_has_any(*idx)))
            .map(|(idx, centroid)| Ok((idx, -score(query, centroid, DistanceMetric::L2)?)))
            .collect::<Result<Vec<_>>>()?;
        centroid_distances.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        let mut candidates = Vec::new();
        for (centroid_id, _) in centroid_distances.into_iter().take(probe_count) {
            let posting = index
                .postings
                .get(centroid_id)
                .ok_or_else(|| Error::Corrupt("vector posting id out of bounds".into()))?;
            let cluster = self
                .clusters
                .get(centroid_id)
                .ok_or_else(|| Error::Corrupt("RaBitQ cluster id out of bounds".into()))?;
            if cluster.centroid_id as usize != centroid_id
                || cluster.codes.len() != posting.vectors.len()
            {
                return Err(Error::Corrupt(
                    "RaBitQ topology does not match vector posting".into(),
                ));
            }
            let centroid = &index.centroids[centroid_id];
            let residual = query
                .iter()
                .zip(centroid)
                .map(|(q, c)| q - c)
                .collect::<Vec<_>>();
            let rotated = cluster.rotate_query(&residual, self.padded_dim);

            for (entry, code) in posting.vectors.iter().zip(&cluster.codes) {
                if code.id != entry.id
                    || code.local_id != entry.local_id
                    || code.version != entry.version
                {
                    return Err(Error::Corrupt(
                        "RaBitQ code does not match vector entry".into(),
                    ));
                }
                if filter.is_some_and(|mask| !mask.allows(centroid_id, entry.local_id as usize))
                    || !accept(&entry.id, entry.version)
                {
                    continue;
                }
                let (estimate, radius) =
                    code.estimate_l2_sq_with_error(&rotated, self.padded_dim, DEFAULT_EPSILON);
                candidates.push(EstimatedCandidate {
                    entry,
                    estimate,
                    lower_bound: (estimate - radius).max(0.0),
                });
            }
        }

        candidates.sort_by(|a, b| {
            a.estimate
                .total_cmp(&b.estimate)
                .then_with(|| a.entry.id.cmp(&b.entry.id))
        });
        let mut hits: Vec<VectorHit> = Vec::with_capacity(k.min(candidates.len()));
        let mut stats = RabitqSearchStats {
            estimated: candidates.len() as u64,
            ..Default::default()
        };
        for candidate in candidates {
            let threshold =
                (hits.len() == k).then(|| -hits.last().expect("full top-k has a tail").score);
            if threshold.is_some_and(|distance| candidate.lower_bound > distance) {
                stats.pruned += 1;
                continue;
            }
            hits.push(VectorHit {
                id: candidate.entry.id.clone(),
                version: candidate.entry.version,
                score: score(query, &candidate.entry.vector, DistanceMetric::L2)?,
            });
            stats.reranked += 1;
            sort_hits(&mut hits);
            hits.truncate(k);
        }
        Ok(RabitqSearchOutcome { hits, stats })
    }
}

struct EstimatedCandidate<'a> {
    entry: &'a crate::vector::VectorEntry,
    estimate: f32,
    lower_bound: f32,
}

impl RabitqCluster {
    /// Rotate a raw query residual `q − c` into this cluster's transformed space.
    pub fn rotate_query(&self, query_residual: &[f32], padded_dim: usize) -> RotatedQuery {
        let residual_norm_sq = query_residual.iter().map(|v| v * v).sum();
        let mut rotated = vec![0.0f32; padded_dim];
        rotated[..query_residual.len()].copy_from_slice(query_residual);
        rotate(&mut rotated, self.transform_seed);
        RotatedQuery {
            rotated,
            residual_norm_sq,
        }
    }
}

impl RabitqCode {
    /// Unbiased RaBitQ estimate of `‖o − q‖²` for the rotated query of this cluster.
    pub fn estimate_l2_sq(&self, query: &RotatedQuery) -> f32 {
        // o == c: the residual code carries no direction; distance is exactly ‖q − c‖.
        if self.residual_norm == 0.0 || self.dot_code == 0.0 {
            return query.residual_norm_sq;
        }
        let code_dot = self.code_dot(&query.rotated);
        // ⟨ō, q_r⟩ ≈ ⟨ō_q, q'⟩ / ⟨ō_q, ō'⟩.
        let inner = code_dot / self.dot_code;
        let est = self.residual_norm * self.residual_norm + query.residual_norm_sq
            - 2.0 * self.residual_norm * inner;
        est.max(0.0)
    }

    /// Return the squared-distance estimate and its high-probability absolute
    /// error radius from Equation 14 of the RaBitQ paper.
    pub fn estimate_l2_sq_with_error(
        &self,
        query: &RotatedQuery,
        padded_dim: usize,
        epsilon: f32,
    ) -> (f32, f32) {
        let estimate = self.estimate_l2_sq(query);
        if self.residual_norm == 0.0
            || self.dot_code == 0.0
            || padded_dim <= 1
            || query.residual_norm_sq == 0.0
        {
            return (estimate, 0.0);
        }
        let dot_sq = self.dot_code * self.dot_code;
        let ratio = ((1.0 - dot_sq).max(0.0) / dot_sq).sqrt();
        let inner_radius =
            query.residual_norm_sq.sqrt() * ratio * epsilon / ((padded_dim - 1) as f32).sqrt();
        (estimate, 2.0 * self.residual_norm * inner_radius)
    }

    /// `⟨ō_q, q'⟩ = (1/√n) Σ sign(ō'ᵢ)·q'ᵢ`, read straight off the sign bits.
    fn code_dot(&self, rotated_query: &[f32]) -> f32 {
        let mut acc = 0.0f32;
        for (dim, &value) in rotated_query.iter().enumerate() {
            let bit = self.code_words[dim / 64] & (1u64 << (dim % 64)) != 0;
            acc += if bit { value } else { -value };
        }
        acc / (rotated_query.len() as f32).sqrt()
    }
}

fn validate_search(
    quantized: &RabitqIndex,
    index: &VectorIndex,
    query: &[f32],
    k: usize,
) -> Result<()> {
    if k == 0 {
        return Err(Error::InvalidQuery(
            "RaBitQ query k must be greater than zero".into(),
        ));
    }
    if quantized.metric != DistanceMetric::L2
        || quantized.column != index.column
        || quantized.dim != index.dim
        || quantized.clusters.len() != index.postings.len()
    {
        return Err(Error::Corrupt(
            "RaBitQ index does not match its L2 vector segment".into(),
        ));
    }
    if query.len() != index.dim || query.iter().any(|value| !value.is_finite()) {
        return Err(Error::InvalidQuery(format!(
            "RaBitQ query vector has invalid dimension or values for dim {}",
            index.dim
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn encode(
    id: &Id,
    local_id: u32,
    version: u64,
    vector: &[f32],
    centroid: &[f32],
    seed: u64,
    padded_dim: usize,
) -> RabitqCode {
    let mut residual_norm_sq = 0.0f32;
    let mut rotated = vec![0.0f32; padded_dim];
    for (dim, (value, centroid_value)) in vector.iter().zip(centroid).enumerate() {
        let residual = value - centroid_value;
        residual_norm_sq += residual * residual;
        rotated[dim] = residual;
    }
    let residual_norm = residual_norm_sq.sqrt();

    let mut code_words = vec![0u64; padded_dim.div_ceil(64)];
    if residual_norm == 0.0 {
        return RabitqCode {
            id: id.clone(),
            local_id,
            version,
            residual_norm: 0.0,
            dot_code: 0.0,
            code_words,
        };
    }

    // Normalize to the unit sphere, then rotate: ō' = P·ō.
    for value in &mut rotated {
        *value /= residual_norm;
    }
    rotate(&mut rotated, seed);

    let mut abs_sum = 0.0f32;
    for (dim, &value) in rotated.iter().enumerate() {
        abs_sum += value.abs();
        if value > 0.0 {
            code_words[dim / 64] |= 1u64 << (dim % 64);
        }
    }
    // ⟨ō_q, ō'⟩ with ō_q entries ±1/√n.
    let dot_code = abs_sum / (padded_dim as f32).sqrt();

    RabitqCode {
        id: id.clone(),
        local_id,
        version,
        residual_norm,
        dot_code,
        code_words,
    }
}

/// Apply the pseudo-orthonormal rotation in place: random ±1 diagonal, then a
/// scaled Walsh–Hadamard transform. `data.len()` must be a power of two.
fn rotate(data: &mut [f32], seed: u64) {
    for (dim, value) in data.iter_mut().enumerate() {
        if splitmix64(seed ^ dim as u64) & 1 == 1 {
            *value = -*value;
        }
    }
    walsh_hadamard(data);
    let scale = 1.0 / (data.len() as f32).sqrt();
    for value in data.iter_mut() {
        *value *= scale;
    }
}

/// In-place fast Walsh–Hadamard transform (unnormalized). `data.len()` must be a
/// power of two; `rotate` applies the `1/√n` orthonormal scaling afterwards.
fn walsh_hadamard(data: &mut [f32]) {
    let n = data.len();
    let mut h = 1;
    while h < n {
        let mut i = 0;
        while i < n {
            for j in i..i + h {
                let x = data[j];
                let y = data[j + h];
                data[j] = x + y;
                data[j + h] = x - y;
            }
            i += 2 * h;
        }
        h *= 2;
    }
}

fn padded_dim(dim: usize) -> usize {
    dim.max(1).next_power_of_two()
}

fn transform_seed(column: &str, centroid_id: u32) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in column.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    splitmix64(hash ^ u64::from(centroid_id))
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}
