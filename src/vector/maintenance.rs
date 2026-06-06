//! SPFresh-style incremental maintenance over the immutable IVF index: LIRE
//! split/merge planning, bounded reassignment deltas, and local cluster
//! rebuilds. Liveness is judged against the column's `VectorVersionMap`.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result};
use crate::manifest::{
    VectorMaintenanceAction, VectorMaintenancePlan, VectorMaintenanceTask,
    VectorMaintenanceThresholds,
};
use crate::value::{Document, Id};

use super::{
    VectorEntry, VectorIndex, VectorVersionMap, assemble_index, compare_scores, score,
    validate_query_vector,
};

const DEFAULT_REASSIGN_NEIGHBORHOOD: usize = 64;

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

impl VectorIndex {
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
        let mut split_clusters = BTreeSet::new();
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
            let mut merged_clusters = BTreeSet::new();
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
        for assigned in &entries {
            validate_query_vector(&assigned.entry.vector, self.dim, "assigned vector")?;
        }
        let assigned = entries
            .into_iter()
            .map(|assigned| (assigned.cluster_id, assigned.entry))
            .collect();
        assemble_index(
            self.column.clone(),
            self.metric,
            self.dim,
            self.centroids.clone(),
            assigned,
            docs,
        )
    }
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
