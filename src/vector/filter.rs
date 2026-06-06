//! Native vector filtering: a per-value cluster/row bitmap built alongside the
//! IVF index, plus the mask algebra (and/or/not) the query planner composes
//! before probing so attribute predicates prune postings without a doc fetch.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::attr;
use crate::error::{Error, Result};
use crate::value::{Document, Id, Value};

use super::{VectorIndex, VectorPosting};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorFilterMask {
    row_counts: Vec<usize>,
    rows: Vec<Vec<u64>>,
}

impl VectorIndex {
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

impl VectorFilterIndex {
    pub(super) fn build(postings: &[VectorPosting], docs: &BTreeMap<Id, Document>) -> Result<Self> {
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
