//! Immutable IVF vector index for Stage 4 ANN v0.
//!
//! The implementation is intentionally small and deterministic: build a
//! full-snapshot IVF index per vector column during index publication, store it
//! as one immutable object, then probe centroids and exact-rerank vectors in the
//! selected postings at query time.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::attr;
use crate::error::{Error, Result};
use crate::frame;
use crate::manifest::{
    VectorMaintenanceAction, VectorMaintenancePlan, VectorMaintenanceTask,
    VectorMaintenanceThresholds,
};
use crate::schema::DistanceMetric;
use crate::value::{Document, Id, Value, VectorValue};

const VECTOR_MAGIC: &[u8; 8] = b"SANAVEC1";
const VERSION_MAP_MAGIC: &[u8; 8] = b"SANAVM1!";
const VECTOR_FORMAT_VERSION: u32 = 1;
const VERSION_MAP_FORMAT_VERSION: u32 = 1;
const KMEANS_ITERS: usize = 8;
const MAX_CLUSTERS: usize = 16;
const DEFAULT_REASSIGN_NEIGHBORHOOD: usize = 64;

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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VectorFilterIndex {
    pub columns: BTreeMap<String, VectorFilterColumn>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VectorFilterColumn {
    pub values: Vec<VectorFilterValue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorFilterValue {
    pub value: Value,
    pub clusters: Vec<u32>,
    pub rows: Vec<VectorFilterRows>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorFilterRows {
    pub cluster_id: u32,
    pub words: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VectorHit {
    pub id: Id,
    pub version: u64,
    pub score: f32,
}

pub trait DistanceKernel {
    fn l2_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()>;
    fn dot_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()>;
    fn cosine_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()>;
}

pub struct ScalarDistanceKernel;

impl DistanceKernel for ScalarDistanceKernel {
    fn l2_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        for (candidate, score) in candidates.iter().zip(out) {
            *score = -query
                .iter()
                .zip(*candidate)
                .map(|(a, b)| {
                    let d = a - b;
                    d * d
                })
                .sum::<f32>();
        }
        Ok(())
    }

    fn dot_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        for (candidate, score) in candidates.iter().zip(out) {
            *score = query.iter().zip(*candidate).map(|(a, b)| a * b).sum();
        }
        Ok(())
    }

    fn cosine_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        let q_norm = squared_norm(query).sqrt();
        if q_norm == 0.0 {
            return Err(Error::InvalidQuery(
                "cosine query and candidate vectors must be non-zero".into(),
            ));
        }
        for (candidate, score) in candidates.iter().zip(out) {
            let c_norm = squared_norm(candidate).sqrt();
            if c_norm == 0.0 {
                return Err(Error::InvalidQuery(
                    "cosine query and candidate vectors must be non-zero".into(),
                ));
            }
            let dot: f32 = query.iter().zip(*candidate).map(|(a, b)| a * b).sum();
            *score = dot / (q_norm * c_norm);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorReassignment {
    pub id: Id,
    pub from_cluster_id: u32,
    pub to_cluster_id: u32,
    pub previous_version: u64,
    pub new_version: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VectorReassignmentDelta {
    pub index: VectorIndex,
    pub version_map: VectorVersionMap,
    pub reassignments: Vec<VectorReassignment>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VectorLocalRebuildDelta {
    pub index: VectorIndex,
    pub version_map: VectorVersionMap,
    pub rebuilt_ids: Vec<Id>,
    pub rebuilt_cluster_ids: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorVersionMap {
    pub format_version: u32,
    pub column: String,
    pub versions: BTreeMap<Id, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorFilterMask {
    row_counts: Vec<usize>,
    rows: Vec<Vec<u64>>,
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
        Ok(frame::encode(
            VERSION_MAP_MAGIC,
            VERSION_MAP_FORMAT_VERSION,
            &body,
        ))
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

        let mut postings = (0..cluster_count)
            .map(|centroid_id| VectorPosting {
                centroid_id: centroid_id as u32,
                vectors: Vec::new(),
            })
            .collect::<Vec<_>>();
        let mut addresses = Vec::new();
        for (mut entry, centroid_id) in entries.into_iter().zip(assignments) {
            entry.local_id = postings[centroid_id].vectors.len() as u32;
            addresses.push(VectorAddress {
                id: entry.id.clone(),
                cluster_id: centroid_id as u32,
                local_id: entry.local_id,
                version: entry.version,
            });
            postings[centroid_id].vectors.push(entry);
        }
        addresses.sort_by(|a, b| a.id.cmp(&b.id));
        let filter_index = VectorFilterIndex::build(&postings, docs)?;

        Ok(Some(Self {
            format_version: VECTOR_FORMAT_VERSION,
            column,
            dim,
            metric,
            centroids,
            postings,
            addresses,
            filter_index,
        }))
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

        let mut postings = (0..centroids.len())
            .map(|centroid_id| VectorPosting {
                centroid_id: centroid_id as u32,
                vectors: Vec::new(),
            })
            .collect::<Vec<_>>();
        let mut addresses = Vec::new();
        for (mut entry, centroid_id) in entries.into_iter().zip(assignments) {
            entry.local_id = postings[centroid_id].vectors.len() as u32;
            addresses.push(VectorAddress {
                id: entry.id.clone(),
                cluster_id: centroid_id as u32,
                local_id: entry.local_id,
                version: entry.version,
            });
            postings[centroid_id].vectors.push(entry);
        }
        addresses.sort_by(|a, b| a.id.cmp(&b.id));
        let filter_index = VectorFilterIndex::build(&postings, docs)?;

        Ok(Some(Self {
            format_version: VECTOR_FORMAT_VERSION,
            column: column.into(),
            dim,
            metric,
            centroids,
            postings,
            addresses,
            filter_index,
        }))
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let body = postcard::to_allocvec(self).map_err(|e| Error::Codec(e.to_string()))?;
        Ok(frame::encode(VECTOR_MAGIC, VECTOR_FORMAT_VERSION, &body))
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

    pub fn maintenance_thresholds(&self) -> VectorMaintenanceThresholds {
        let centroid_count = self.centroids.len().max(1);
        let target_rows = self.row_count().div_ceil(centroid_count).max(1);
        let min_posting_rows = target_rows.div_ceil(2).max(1);
        let max_posting_rows = (target_rows * 2).max(min_posting_rows + 1);
        VectorMaintenanceThresholds {
            min_posting_rows: min_posting_rows as u64,
            max_posting_rows: max_posting_rows as u64,
            reassign_neighborhood: DEFAULT_REASSIGN_NEIGHBORHOOD,
        }
    }

    pub fn plan_maintenance(
        &self,
        append_indexes: &[VectorIndex],
        version_map: Option<&VectorVersionMap>,
        thresholds: VectorMaintenanceThresholds,
    ) -> Result<VectorMaintenancePlan> {
        if let Some(version_map) = version_map
            && version_map.column != self.column
        {
            return Err(Error::Corrupt(format!(
                "vector version map column '{}' does not match index column '{}'",
                version_map.column, self.column
            )));
        }

        let mut stats = (0..self.centroids.len())
            .map(|cluster_id| VectorPostingMaintenanceStats {
                cluster_id: cluster_id as u32,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        accumulate_maintenance_stats(self, self, false, version_map, &mut stats)?;

        for append in append_indexes {
            accumulate_maintenance_stats(self, append, true, version_map, &mut stats)?;
        }

        let mut tasks = Vec::new();
        let mut split_clusters = std::collections::BTreeSet::new();
        for stat in &stats {
            if stat.live_rows > thresholds.max_posting_rows {
                split_clusters.insert(stat.cluster_id);
                tasks.push(VectorMaintenanceTask {
                    action: VectorMaintenanceAction::Split,
                    cluster_id: stat.cluster_id,
                    partner_cluster_id: None,
                    neighbor_cluster_ids: self.nearest_cluster_ids(
                        stat.cluster_id as usize,
                        thresholds.reassign_neighborhood,
                    )?,
                    live_rows: stat.live_rows,
                    stale_rows: stat.stale_rows,
                    append_rows: stat.append_rows,
                    total_rows: stat.total_rows,
                });
            }
        }

        if self.centroids.len() > 1 {
            let mut merged_clusters = std::collections::BTreeSet::new();
            for stat in &stats {
                if stat.live_rows >= thresholds.min_posting_rows
                    || split_clusters.contains(&stat.cluster_id)
                    || merged_clusters.contains(&stat.cluster_id)
                {
                    continue;
                }

                let neighbors = self.nearest_cluster_ids(
                    stat.cluster_id as usize,
                    thresholds.reassign_neighborhood,
                )?;
                let Some(partner) = neighbors
                    .iter()
                    .copied()
                    .find(|cluster_id| !split_clusters.contains(cluster_id))
                else {
                    continue;
                };
                merged_clusters.insert(stat.cluster_id);
                merged_clusters.insert(partner);
                tasks.push(VectorMaintenanceTask {
                    action: VectorMaintenanceAction::Merge,
                    cluster_id: stat.cluster_id,
                    partner_cluster_id: Some(partner),
                    neighbor_cluster_ids: neighbors,
                    live_rows: stat.live_rows,
                    stale_rows: stat.stale_rows,
                    append_rows: stat.append_rows,
                    total_rows: stat.total_rows,
                });
            }
        }

        Ok(VectorMaintenancePlan { thresholds, tasks })
    }

    pub fn build_reassignment_delta(
        &self,
        append_indexes: &[VectorIndex],
        version_map: &VectorVersionMap,
        task: &VectorMaintenanceTask,
        new_version: u64,
        docs: &BTreeMap<Id, Document>,
    ) -> Result<Option<VectorReassignmentDelta>> {
        if version_map.column != self.column {
            return Err(Error::Corrupt(format!(
                "vector version map column '{}' does not match index column '{}'",
                version_map.column, self.column
            )));
        }

        let cluster_ids = self.task_cluster_neighborhood(task)?;
        let mut candidates = BTreeMap::new();
        collect_live_reassignment_candidates(
            self,
            self,
            &cluster_ids,
            version_map,
            &mut candidates,
        )?;
        for append in append_indexes {
            collect_live_reassignment_candidates(
                self,
                append,
                &cluster_ids,
                version_map,
                &mut candidates,
            )?;
        }

        let mut reassigned_entries = Vec::new();
        let mut reassignments = Vec::new();
        for (id, candidate) in candidates {
            let to_cluster_id = self.nearest_cluster_in(&candidate.entry.vector, &cluster_ids)?;
            if to_cluster_id == candidate.cluster_id {
                continue;
            }
            if new_version <= candidate.entry.version {
                return Err(Error::Corrupt(
                    "vector reassignment version must be newer than the live copy".into(),
                ));
            }

            reassignments.push(VectorReassignment {
                id: id.clone(),
                from_cluster_id: candidate.cluster_id,
                to_cluster_id,
                previous_version: candidate.entry.version,
                new_version,
            });
            reassigned_entries.push(AssignedVectorEntry {
                cluster_id: to_cluster_id,
                entry: VectorEntry {
                    id,
                    vector: candidate.entry.vector,
                    local_id: 0,
                    version: new_version,
                },
            });
        }

        if reassigned_entries.is_empty() {
            return Ok(None);
        }

        let mut next_version_map = version_map.clone();
        for reassignment in &reassignments {
            next_version_map
                .versions
                .insert(reassignment.id.clone(), new_version);
        }

        Ok(Some(VectorReassignmentDelta {
            index: self.build_assigned_delta(reassigned_entries, docs)?,
            version_map: next_version_map,
            reassignments,
        }))
    }

    pub fn build_local_rebuild_delta(
        &self,
        append_indexes: &[VectorIndex],
        version_map: &VectorVersionMap,
        task: &VectorMaintenanceTask,
        new_version: u64,
        docs: &BTreeMap<Id, Document>,
    ) -> Result<Option<VectorLocalRebuildDelta>> {
        if version_map.column != self.column {
            return Err(Error::Corrupt(format!(
                "vector version map column '{}' does not match index column '{}'",
                version_map.column, self.column
            )));
        }

        let cluster_ids = self.task_cluster_neighborhood(task)?;
        let mut candidates = BTreeMap::new();
        collect_live_reassignment_candidates(
            self,
            self,
            &cluster_ids,
            version_map,
            &mut candidates,
        )?;
        for append in append_indexes {
            collect_live_reassignment_candidates(
                self,
                append,
                &cluster_ids,
                version_map,
                &mut candidates,
            )?;
        }

        let mut entries = Vec::new();
        let mut rebuilt_ids = Vec::new();
        let mut next_version_map = version_map.clone();
        for (id, candidate) in candidates {
            if new_version <= candidate.entry.version {
                return Err(Error::Corrupt(
                    "vector local rebuild version must be newer than the live copy".into(),
                ));
            }
            next_version_map.versions.insert(id.clone(), new_version);
            rebuilt_ids.push(id.clone());
            entries.push(VectorEntry {
                id,
                vector: candidate.entry.vector,
                local_id: 0,
                version: new_version,
            });
        }

        let target_cluster_count = match task.action {
            VectorMaintenanceAction::Split => 2,
            VectorMaintenanceAction::Merge => 1,
        };
        let Some(index) = VectorIndex::build_with_cluster_count(
            self.column.clone(),
            self.metric,
            self.dim,
            entries,
            docs,
            |_| target_cluster_count,
        )?
        else {
            return Ok(None);
        };
        let rebuilt_cluster_ids = cluster_ids.into_iter().collect();
        Ok(Some(VectorLocalRebuildDelta {
            index,
            version_map: next_version_map,
            rebuilt_ids,
            rebuilt_cluster_ids,
        }))
    }

    fn task_cluster_neighborhood(&self, task: &VectorMaintenanceTask) -> Result<BTreeSet<u32>> {
        let mut cluster_ids = BTreeSet::new();
        cluster_ids.insert(task.cluster_id);
        if let Some(partner_cluster_id) = task.partner_cluster_id {
            cluster_ids.insert(partner_cluster_id);
        }
        cluster_ids.extend(task.neighbor_cluster_ids.iter().copied());
        for cluster_id in &cluster_ids {
            if (*cluster_id as usize) >= self.centroids.len() {
                return Err(Error::Corrupt(format!(
                    "vector maintenance task references missing cluster {cluster_id}"
                )));
            }
        }
        Ok(cluster_ids)
    }

    fn nearest_cluster_in(&self, vector: &[f32], cluster_ids: &BTreeSet<u32>) -> Result<u32> {
        validate_query_vector(vector, self.dim, "reassignment vector")?;
        let mut best: Option<(u32, f32)> = None;
        for cluster_id in cluster_ids {
            let centroid = self
                .centroids
                .get(*cluster_id as usize)
                .ok_or_else(|| Error::Corrupt("vector cluster id out of bounds".into()))?;
            let candidate = (*cluster_id, score(vector, centroid, self.metric)?);
            if best.is_none_or(|best| {
                compare_scores(candidate.1, best.1)
                    .then_with(|| candidate.0.cmp(&best.0))
                    .is_lt()
            }) {
                best = Some(candidate);
            }
        }
        best.map(|(cluster_id, _)| cluster_id)
            .ok_or_else(|| Error::Corrupt("empty vector reassignment neighborhood".into()))
    }

    fn nearest_cluster_for_vector(&self, vector: &[f32]) -> Result<u32> {
        validate_query_vector(vector, self.dim, "maintenance vector")?;
        let mut best: Option<(u32, f32)> = None;
        for (cluster_id, centroid) in self.centroids.iter().enumerate() {
            let candidate = (cluster_id as u32, score(vector, centroid, self.metric)?);
            if best.is_none_or(|best| {
                compare_scores(candidate.1, best.1)
                    .then_with(|| candidate.0.cmp(&best.0))
                    .is_lt()
            }) {
                best = Some(candidate);
            }
        }
        best.map(|(cluster_id, _)| cluster_id)
            .ok_or_else(|| Error::Corrupt("vector index has no centroids".into()))
    }

    fn nearest_cluster_ids(&self, cluster_id: usize, limit: usize) -> Result<Vec<u32>> {
        let Some(centroid) = self.centroids.get(cluster_id) else {
            return Err(Error::Corrupt("vector cluster id out of bounds".into()));
        };
        let mut scored = self
            .centroids
            .iter()
            .enumerate()
            .filter(|(idx, _)| *idx != cluster_id)
            .map(|(idx, other)| Ok((idx as u32, score(centroid, other, self.metric)?)))
            .collect::<Result<Vec<_>>>()?;
        scored.sort_by(|a, b| compare_scores(a.1, b.1).then_with(|| a.0.cmp(&b.0)));
        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(cluster_id, _)| cluster_id)
            .collect())
    }

    fn build_assigned_delta(
        &self,
        entries: Vec<AssignedVectorEntry>,
        docs: &BTreeMap<Id, Document>,
    ) -> Result<VectorIndex> {
        let mut postings = (0..self.centroids.len())
            .map(|centroid_id| VectorPosting {
                centroid_id: centroid_id as u32,
                vectors: Vec::new(),
            })
            .collect::<Vec<_>>();
        let mut addresses = Vec::new();
        for assigned in entries {
            let cluster_id = assigned.cluster_id as usize;
            let Some(posting) = postings.get_mut(cluster_id) else {
                return Err(Error::Corrupt(
                    "assigned vector cluster id out of bounds".into(),
                ));
            };
            let mut entry = assigned.entry;
            validate_query_vector(&entry.vector, self.dim, "assigned vector")?;
            entry.local_id = posting.vectors.len() as u32;
            addresses.push(VectorAddress {
                id: entry.id.clone(),
                cluster_id: assigned.cluster_id,
                local_id: entry.local_id,
                version: entry.version,
            });
            posting.vectors.push(entry);
        }
        addresses.sort_by(|a, b| a.id.cmp(&b.id));
        let filter_index = VectorFilterIndex::build(&postings, docs)?;

        Ok(VectorIndex {
            format_version: VECTOR_FORMAT_VERSION,
            column: self.column.clone(),
            dim: self.dim,
            metric: self.metric,
            centroids: self.centroids.clone(),
            postings,
            addresses,
            filter_index,
        })
    }

    pub fn all_filter_mask(&self) -> VectorFilterMask {
        VectorFilterMask::all(self.cluster_row_counts())
    }

    pub fn empty_filter_mask(&self) -> VectorFilterMask {
        VectorFilterMask::empty(self.cluster_row_counts())
    }

    pub fn filter_mask_by_value<F>(&self, column: &str, mut matches: F) -> Option<VectorFilterMask>
    where
        F: FnMut(&Value) -> bool,
    {
        let row_counts = self.cluster_row_counts();
        let Some(column) = self.filter_index.columns.get(column) else {
            return Some(VectorFilterMask::empty(row_counts));
        };

        let mut mask = VectorFilterMask::empty(row_counts);
        for value in &column.values {
            if !matches(&value.value) {
                continue;
            }
            mask.union_value(value);
        }
        Some(mask)
    }
}

struct AssignedVectorEntry {
    cluster_id: u32,
    entry: VectorEntry,
}

struct ReassignmentCandidate {
    cluster_id: u32,
    entry: VectorEntry,
}

#[derive(Default)]
struct VectorPostingMaintenanceStats {
    cluster_id: u32,
    live_rows: u64,
    stale_rows: u64,
    append_rows: u64,
    total_rows: u64,
}

fn accumulate_maintenance_stats(
    base: &VectorIndex,
    index: &VectorIndex,
    is_append: bool,
    version_map: Option<&VectorVersionMap>,
    stats: &mut [VectorPostingMaintenanceStats],
) -> Result<()> {
    if index.column != base.column || index.dim != base.dim || index.metric != base.metric {
        return Err(Error::Corrupt(format!(
            "vector segment for '{}' does not match base index",
            base.column
        )));
    }
    let same_topology = index.centroids == base.centroids;
    for posting in &index.postings {
        for entry in &posting.vectors {
            let cluster_id = if same_topology {
                posting.centroid_id
            } else {
                base.nearest_cluster_for_vector(&entry.vector)?
            } as usize;
            let Some(stat) = stats.get_mut(cluster_id) else {
                return Err(Error::Corrupt("vector posting id out of bounds".into()));
            };
            stat.total_rows += 1;
            if is_append {
                stat.append_rows += 1;
            }
            if version_map.is_none_or(|versions| versions.is_live(&entry.id, entry.version)) {
                stat.live_rows += 1;
            } else {
                stat.stale_rows += 1;
            }
        }
    }
    Ok(())
}

fn collect_live_reassignment_candidates(
    base: &VectorIndex,
    index: &VectorIndex,
    cluster_ids: &BTreeSet<u32>,
    version_map: &VectorVersionMap,
    out: &mut BTreeMap<Id, ReassignmentCandidate>,
) -> Result<()> {
    if index.column != base.column || index.dim != base.dim || index.metric != base.metric {
        return Err(Error::Corrupt(format!(
            "vector segment for '{}' does not match base index",
            base.column
        )));
    }
    let same_topology = index.centroids == base.centroids;
    for posting in &index.postings {
        for entry in &posting.vectors {
            if !version_map.is_live(&entry.id, entry.version) {
                continue;
            }
            let cluster_id = if same_topology {
                posting.centroid_id
            } else {
                base.nearest_cluster_for_vector(&entry.vector)?
            };
            if !cluster_ids.contains(&cluster_id) {
                continue;
            }
            out.entry(entry.id.clone())
                .or_insert_with(|| ReassignmentCandidate {
                    cluster_id,
                    entry: entry.clone(),
                });
        }
    }
    Ok(())
}

impl VectorFilterIndex {
    fn build(postings: &[VectorPosting], docs: &BTreeMap<Id, Document>) -> Result<Self> {
        let row_counts = postings
            .iter()
            .map(|posting| posting.vectors.len())
            .collect::<Vec<_>>();
        let mut builders: BTreeMap<String, BTreeMap<Vec<u8>, VectorFilterValueBuilder>> =
            BTreeMap::new();

        for posting in postings {
            let cluster_id = posting.centroid_id as usize;
            let Some(row_count) = row_counts.get(cluster_id).copied() else {
                return Err(Error::Corrupt(
                    "vector filter cluster id out of bounds".into(),
                ));
            };
            for entry in &posting.vectors {
                let Some(doc) = docs.get(&entry.id) else {
                    continue;
                };
                for (column, value) in &doc.attributes {
                    for scalar in attr::indexable_values(value)? {
                        let Some(key) = attr::scalar_key(scalar)? else {
                            continue;
                        };
                        builders
                            .entry(column.clone())
                            .or_default()
                            .entry(key)
                            .or_insert_with(|| {
                                VectorFilterValueBuilder::new(scalar.clone(), &row_counts)
                            })
                            .set(cluster_id, entry.local_id as usize, row_count);
                    }
                }
            }
        }

        let columns = builders
            .into_iter()
            .map(|(column, values)| {
                let values = values
                    .into_values()
                    .map(VectorFilterValueBuilder::finish)
                    .collect();
                (column, VectorFilterColumn { values })
            })
            .collect();
        Ok(Self { columns })
    }
}

struct VectorFilterValueBuilder {
    value: Value,
    rows: Vec<Vec<u64>>,
}

impl VectorFilterValueBuilder {
    fn new(value: Value, row_counts: &[usize]) -> Self {
        Self {
            value,
            rows: row_counts
                .iter()
                .map(|count| vec![0u64; words_for_rows(*count)])
                .collect(),
        }
    }

    fn set(&mut self, cluster_id: usize, local_id: usize, row_count: usize) {
        if local_id >= row_count {
            return;
        }
        let word = local_id / 64;
        let bit = local_id % 64;
        self.rows[cluster_id][word] |= 1u64 << bit;
    }

    fn finish(self) -> VectorFilterValue {
        let mut clusters = Vec::new();
        let mut rows = Vec::new();
        for (cluster_id, words) in self.rows.into_iter().enumerate() {
            if words.iter().all(|word| *word == 0) {
                continue;
            }
            clusters.push(cluster_id as u32);
            rows.push(VectorFilterRows {
                cluster_id: cluster_id as u32,
                words,
            });
        }
        VectorFilterValue {
            value: self.value,
            clusters,
            rows,
        }
    }
}

impl VectorFilterMask {
    fn empty(row_counts: Vec<usize>) -> Self {
        let rows = row_counts
            .iter()
            .map(|count| vec![0u64; words_for_rows(*count)])
            .collect();
        Self { row_counts, rows }
    }

    fn all(row_counts: Vec<usize>) -> Self {
        let mut mask = Self::empty(row_counts);
        for cluster_id in 0..mask.rows.len() {
            for word in &mut mask.rows[cluster_id] {
                *word = u64::MAX;
            }
            mask.trim_cluster(cluster_id);
        }
        mask
    }

    pub fn and(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for (cluster, rhs) in out.rows.iter_mut().zip(&other.rows) {
            for (lhs, rhs) in cluster.iter_mut().zip(rhs) {
                *lhs &= *rhs;
            }
        }
        out
    }

    pub fn or(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for (cluster, rhs) in out.rows.iter_mut().zip(&other.rows) {
            for (lhs, rhs) in cluster.iter_mut().zip(rhs) {
                *lhs |= *rhs;
            }
        }
        out
    }

    pub fn not(&self) -> Self {
        let mut out = self.clone();
        for cluster_id in 0..out.rows.len() {
            for word in &mut out.rows[cluster_id] {
                *word = !*word;
            }
            out.trim_cluster(cluster_id);
        }
        out
    }

    pub fn cluster_has_any(&self, cluster_id: usize) -> bool {
        self.rows
            .get(cluster_id)
            .is_some_and(|words| words.iter().any(|word| *word != 0))
    }

    pub fn allows(&self, cluster_id: usize, local_id: usize) -> bool {
        if self
            .row_counts
            .get(cluster_id)
            .is_none_or(|count| local_id >= *count)
        {
            return false;
        }
        let word = local_id / 64;
        let bit = local_id % 64;
        self.rows
            .get(cluster_id)
            .and_then(|words| words.get(word))
            .is_some_and(|word| (word & (1u64 << bit)) != 0)
    }

    fn union_value(&mut self, value: &VectorFilterValue) {
        for rows in &value.rows {
            let cluster_id = rows.cluster_id as usize;
            let Some(dst) = self.rows.get_mut(cluster_id) else {
                continue;
            };
            for (lhs, rhs) in dst.iter_mut().zip(&rows.words) {
                *lhs |= *rhs;
            }
        }
    }

    fn trim_cluster(&mut self, cluster_id: usize) {
        let Some(row_count) = self.row_counts.get(cluster_id).copied() else {
            return;
        };
        let extra = row_count % 64;
        if extra == 0 {
            return;
        }
        if let Some(last) = self.rows[cluster_id].last_mut() {
            *last &= (1u64 << extra) - 1;
        }
    }
}

fn words_for_rows(row_count: usize) -> usize {
    row_count.div_ceil(64)
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

pub fn score_batch(
    query: &[f32],
    candidates: &[&[f32]],
    metric: DistanceMetric,
    out: &mut [f32],
) -> Result<()> {
    match metric {
        DistanceMetric::L2 => ScalarDistanceKernel::l2_f32_batch(query, candidates, out),
        DistanceMetric::Dot => ScalarDistanceKernel::dot_f32_batch(query, candidates, out),
        DistanceMetric::Cosine => ScalarDistanceKernel::cosine_f32_batch(query, candidates, out),
    }
}

pub fn score(query: &[f32], candidate: &[f32], metric: DistanceMetric) -> Result<f32> {
    let candidates = [candidate];
    let mut out = [0.0f32];
    score_batch(query, &candidates, metric, &mut out)?;
    Ok(out[0])
}

fn validate_batch(query: &[f32], candidates: &[&[f32]], out: &[f32]) -> Result<()> {
    if candidates.len() != out.len() {
        return Err(Error::InvalidQuery(format!(
            "score output has len {}, expected {}",
            out.len(),
            candidates.len()
        )));
    }
    if candidates
        .iter()
        .any(|candidate| candidate.len() != query.len())
    {
        return Err(Error::InvalidQuery(
            "query and candidate vectors must have matching dimensions".into(),
        ));
    }
    if query.iter().any(|v| !v.is_finite())
        || candidates
            .iter()
            .flat_map(|candidate| candidate.iter())
            .any(|v| !v.is_finite())
    {
        return Err(Error::InvalidQuery(
            "query and candidate vectors must contain only finite values".into(),
        ));
    }
    Ok(())
}

fn squared_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|v| v * v).sum()
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
