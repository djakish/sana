//! Stage 3 logical query execution over a strong namespace snapshot.
//!
//! This executor is deliberately correct before it is clever: it scans the
//! materialized document snapshot from the LSM + WAL overlay, evaluates filters,
//! computes aggregations, orders rows, and performs exact vector kNN. Attribute
//! SSTs can later replace the candidate-generation step without changing these
//! logical request/response types.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::attr::{self, AttrBound};
use crate::doc::encode_id;
use crate::error::{Error, Result};
use crate::manifest::{NamespaceManifest, VectorIndexMeta};
use crate::namespace::{Namespace, op_id};
use crate::schema::{ColumnType, DistanceMetric};
use crate::value::{Document, Id, Value, compare_scalars, scalar_eq};
use crate::vector::{self, VectorFilterMask, VectorIndex, VectorVersionMap};

const DEFAULT_RECALL_NUM: usize = 25;
const DEFAULT_RECALL_TOP_K: usize = 10;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Query {
    #[serde(default)]
    pub filter: Option<FilterExpr>,
    #[serde(default)]
    pub order_by: Option<OrderBy>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub aggregates: Vec<Aggregate>,
    #[serde(default)]
    pub exact_vector: Option<ExactVectorQuery>,
    #[serde(default)]
    pub approx_vector: Option<ApproxVectorQuery>,
}

impl Query {
    pub fn all() -> Self {
        Self {
            filter: None,
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FilterExpr {
    Eq {
        column: String,
        value: Value,
    },
    Range {
        column: String,
        lower: Option<RangeBound>,
        upper: Option<RangeBound>,
    },
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RangeBound {
    Included(Value),
    Excluded(Value),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBy {
    pub target: OrderTarget,
    pub direction: SortDirection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderTarget {
    Id,
    Attribute(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Aggregate {
    Count,
    Sum { column: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExactVectorQuery {
    pub column: String,
    pub vector: Vec<f32>,
    pub k: usize,
    #[serde(default)]
    pub metric: Option<DistanceMetric>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApproxVectorQuery {
    pub column: String,
    pub vector: Vec<f32>,
    pub k: usize,
    #[serde(default)]
    pub probes: Option<usize>,
    #[serde(default)]
    pub metric: Option<DistanceMetric>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub rows: Vec<QueryRow>,
    pub aggregates: Vec<AggregateResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallRequest {
    #[serde(default = "default_recall_num")]
    pub num: usize,
    #[serde(default = "default_recall_top_k")]
    pub top_k: usize,
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub probes: Option<usize>,
    #[serde(default)]
    pub metric: Option<DistanceMetric>,
    #[serde(default)]
    pub filter: Option<FilterExpr>,
}

impl Default for RecallRequest {
    fn default() -> Self {
        Self {
            num: DEFAULT_RECALL_NUM,
            top_k: DEFAULT_RECALL_TOP_K,
            column: None,
            probes: None,
            metric: None,
            filter: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallResult {
    pub column: String,
    pub requested: usize,
    pub sampled: usize,
    pub top_k: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probes: Option<usize>,
    pub avg_recall: f64,
    pub avg_exhaustive_count: f64,
    pub avg_ann_count: f64,
    pub samples: Vec<RecallSample>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallSample {
    pub query_id: Id,
    pub recall: f64,
    pub exhaustive_count: usize,
    pub ann_count: usize,
    pub exhaustive_ids: Vec<Id>,
    pub ann_ids: Vec<Id>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryRow {
    pub id: Id,
    pub document: Document,
    /// Higher is better. L2 uses negative squared distance, cosine uses cosine
    /// similarity, and dot uses the raw dot product.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AggregateResult {
    Count(u64),
    Sum {
        column: String,
        value_count: u64,
        total: f64,
    },
}

fn default_recall_num() -> usize {
    DEFAULT_RECALL_NUM
}

fn default_recall_top_k() -> usize {
    DEFAULT_RECALL_TOP_K
}

pub async fn execute(ns: &Namespace, query: Query) -> Result<QueryResult> {
    let manifest = ns.load_manifest().await?;
    if query.exact_vector.is_some() && query.approx_vector.is_some() {
        return Err(Error::InvalidQuery(
            "query cannot specify both exact_vector and approx_vector".into(),
        ));
    }

    if let Some(ann_query) = &query.approx_vector
        && query.order_by.is_none()
        && query.aggregates.is_empty()
    {
        return execute_ann_vector(ns, &manifest, ann_query, query.filter.as_ref(), query.limit)
            .await;
    }

    let mut candidates = materialize_candidates(ns, &manifest, query.filter.as_ref()).await?;

    let aggregates = compute_aggregates(&query.aggregates, &candidates)?;

    if let Some(vector_query) = &query.exact_vector {
        finish_exact_vector(&mut candidates, &manifest, vector_query)?;
    } else if let Some(vector_query) = &query.approx_vector {
        // Forced off the ANN fast path by an order_by/aggregate; score exactly.
        let exact_query = ExactVectorQuery {
            column: vector_query.column.clone(),
            vector: vector_query.vector.clone(),
            k: vector_query.k,
            metric: vector_query.metric,
        };
        finish_exact_vector(&mut candidates, &manifest, &exact_query)?;
    } else if let Some(order_by) = &query.order_by {
        sort_rows(&mut candidates, order_by)?;
    } else {
        candidates.sort_by(|a, b| a.id.cmp(&b.id));
    }

    if let Some(limit) = query.limit {
        candidates.truncate(limit);
    }

    Ok(QueryResult {
        rows: candidates,
        aggregates,
    })
}

pub async fn recall(ns: &Namespace, request: RecallRequest) -> Result<RecallResult> {
    if request.num == 0 {
        return Err(Error::InvalidQuery(
            "recall num must be greater than zero".into(),
        ));
    }
    if request.top_k == 0 {
        return Err(Error::InvalidQuery(
            "recall top_k must be greater than zero".into(),
        ));
    }
    let manifest = ns.load_manifest().await?;
    let column = match request.column.clone() {
        Some(column) => column,
        None => manifest
            .vector_indexes
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| {
                Error::InvalidQuery(
                    "recall requires a published vector index; run flush or compact first".into(),
                )
            })?,
    };
    let Some(index_meta) = manifest.vector_indexes.get(&column) else {
        return Err(Error::InvalidQuery(format!(
            "recall requires a published vector index for '{column}'; run flush or compact first"
        )));
    };

    let (dim, default_metric) = vector_column_schema(&manifest, &column)?;
    let metric = request.metric.unwrap_or(default_metric);
    if let Some(filter) = &request.filter {
        let index = VectorIndex::decode(&ns.store().get(&index_meta.key).await?.bytes)?;
        if native_filter_mask(&index, filter)?.is_none() {
            return Err(Error::InvalidQuery(
                "filtered recall requires a natively supported scalar filter".into(),
            ));
        }
    }

    let mut candidates = recall_candidates(ns, &column, dim, request.filter.as_ref()).await?;
    if candidates.is_empty() {
        return Err(Error::InvalidQuery(format!(
            "recall column '{column}' has no stored vectors"
        )));
    }

    candidates.sort_by_key(|candidate| stable_sample_key(&candidate.id, &column));
    candidates.truncate(request.num.min(candidates.len()));

    let mut samples = Vec::with_capacity(candidates.len());
    let mut recall_sum = 0.0;
    let mut exhaustive_count_sum = 0usize;
    let mut ann_count_sum = 0usize;

    for candidate in candidates {
        // The exact and ANN queries for a sample are independent read-only
        // executions; run them concurrently rather than back to back.
        let exact_query = Query {
            filter: request.filter.clone(),
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: Some(ExactVectorQuery {
                column: column.clone(),
                vector: candidate.vector.clone(),
                k: request.top_k,
                metric: Some(metric),
            }),
            approx_vector: None,
        };
        let ann_query = Query {
            filter: request.filter.clone(),
            order_by: None,
            limit: None,
            aggregates: Vec::new(),
            exact_vector: None,
            approx_vector: Some(ApproxVectorQuery {
                column: column.clone(),
                vector: candidate.vector,
                k: request.top_k,
                probes: request.probes,
                metric: Some(metric),
            }),
        };
        let (exact, ann) = tokio::join!(execute(ns, exact_query), execute(ns, ann_query));
        let (exact, ann) = (exact?, ann?);

        let exhaustive_ids = exact.rows.into_iter().map(|row| row.id).collect::<Vec<_>>();
        let ann_ids = ann.rows.into_iter().map(|row| row.id).collect::<Vec<_>>();
        let sample_recall = vector::recall_at(&exhaustive_ids, &ann_ids, request.top_k);

        recall_sum += sample_recall;
        exhaustive_count_sum += exhaustive_ids.len();
        ann_count_sum += ann_ids.len();
        samples.push(RecallSample {
            query_id: candidate.id,
            recall: sample_recall,
            exhaustive_count: exhaustive_ids.len(),
            ann_count: ann_ids.len(),
            exhaustive_ids,
            ann_ids,
        });
    }

    let sampled = samples.len();
    Ok(RecallResult {
        column,
        requested: request.num,
        sampled,
        top_k: request.top_k,
        probes: request.probes,
        avg_recall: recall_sum / sampled as f64,
        avg_exhaustive_count: exhaustive_count_sum as f64 / sampled as f64,
        avg_ann_count: ann_count_sum as f64 / sampled as f64,
        samples,
    })
}

async fn execute_ann_vector(
    ns: &Namespace,
    manifest: &NamespaceManifest,
    query: &ApproxVectorQuery,
    filter: Option<&FilterExpr>,
    limit: Option<usize>,
) -> Result<QueryResult> {
    let exact_query = ExactVectorQuery {
        column: query.column.clone(),
        vector: query.vector.clone(),
        k: query.k,
        metric: query.metric,
    };
    let (dim, default_metric) = vector_schema(manifest, &exact_query)?;
    if query.vector.len() != dim {
        return Err(Error::InvalidQuery(format!(
            "query vector for '{}' has dim {}, expected {dim}",
            query.column,
            query.vector.len()
        )));
    }
    let metric = query.metric.unwrap_or(default_metric);

    let Some(meta) = manifest.vector_indexes.get(&query.column) else {
        return exact_vector_fallback(ns, manifest, filter, &exact_query, metric, dim, limit).await;
    };

    let index_bytes = ns.store().get(&meta.key).await?.bytes;
    let index = VectorIndex::decode(&index_bytes)?;
    let version_map = load_vector_version_map(ns, meta).await?;
    let native_filter = match filter {
        Some(filter) => match native_filter_mask(&index, filter)? {
            Some(mask) => Some(mask),
            None => {
                return exact_vector_fallback(
                    ns,
                    manifest,
                    Some(filter),
                    &exact_query,
                    metric,
                    dim,
                    limit,
                )
                .await;
            }
        },
        None => None,
    };
    let mut ann_hits = index.search_with_filter(
        &query.vector,
        usize::MAX,
        query.probes,
        Some(metric),
        native_filter.as_ref(),
    )?;
    // The append-delta object keys are known up front, so prefetch them
    // concurrently (mirroring `read_overlay_ops`) rather than serializing a
    // round trip per delta; decode + search stays sequential and in order.
    for bytes in fetch_append_indexes(ns, meta).await? {
        let append_index = VectorIndex::decode(&bytes)?;
        let append_filter = match filter {
            Some(filter) => match native_filter_mask(&append_index, filter)? {
                Some(mask) => Some(mask),
                None => {
                    return exact_vector_fallback(
                        ns,
                        manifest,
                        Some(filter),
                        &exact_query,
                        metric,
                        dim,
                        limit,
                    )
                    .await;
                }
            },
            None => None,
        };
        ann_hits.extend(append_index.search_with_filter(
            &query.vector,
            usize::MAX,
            query.probes,
            Some(metric),
            append_filter.as_ref(),
        )?);
    }

    let commit = ns.commit_cursor().await?;
    let overlay = ns.read_overlay_ops(manifest.indexed_cursor, commit).await?;
    let touched: BTreeSet<Id> = overlay.iter().map(|op| op_id(op).clone()).collect();

    // The documents we need: live ANN hits (not superseded by the overlay) plus
    // every overlay-touched id, resolved in one SST pass rather than per id.
    let live_hits: Vec<_> = ann_hits
        .into_iter()
        .filter(|hit| !touched.contains(&hit.id))
        .filter(|hit| {
            version_map
                .as_ref()
                .is_none_or(|versions| versions.is_live(&hit.id, hit.version))
        })
        .collect();
    let mut needed: BTreeSet<Id> = live_hits.iter().map(|hit| hit.id.clone()).collect();
    needed.extend(touched.iter().cloned());
    let resolved = ns.resolve_ids(manifest, &overlay, &needed).await?;

    let mut rows = Vec::new();
    for hit in live_hits {
        let Some(document) = resolved.get(&hit.id) else {
            continue;
        };
        if !matches_filter(filter, document)? {
            continue;
        }
        rows.push(QueryRow {
            id: hit.id,
            document: document.clone(),
            score: Some(hit.score),
        });
    }

    for id in touched {
        let Some(document) = resolved.get(&id) else {
            continue;
        };
        if !matches_filter(filter, document)? {
            continue;
        }
        let Some(vector) = document.vectors.get(&query.column) else {
            continue;
        };
        let values = vector::vector_to_f32(vector);
        if values.len() != dim {
            return Err(Error::Corrupt(format!(
                "stored vector '{}' for {:?} has dim {}, expected {dim}",
                query.column,
                id,
                values.len()
            )));
        }
        rows.push(QueryRow {
            id,
            document: document.clone(),
            score: Some(vector::score(&query.vector, &values, metric)?),
        });
    }

    rows.sort_by(compare_score_rows);
    rows.truncate(keep_count(query.k, limit));
    Ok(QueryResult {
        rows,
        aggregates: Vec::new(),
    })
}

/// Fetch every append-delta index object for a vector column concurrently,
/// returning their bytes in `append_indexes` order. The keys are all known from
/// the manifest, so the GETs need not be serialized.
async fn fetch_append_indexes(ns: &Namespace, meta: &VectorIndexMeta) -> Result<Vec<bytes::Bytes>> {
    if meta.append_indexes.is_empty() {
        return Ok(Vec::new());
    }
    let mut set = tokio::task::JoinSet::new();
    for (idx, append_meta) in meta.append_indexes.iter().enumerate() {
        let store = ns.store().clone();
        let key = append_meta.key.clone();
        set.spawn(async move {
            Ok::<(usize, bytes::Bytes), Error>((idx, store.get(&key).await?.bytes))
        });
    }

    let mut slots: Vec<Option<bytes::Bytes>> =
        (0..meta.append_indexes.len()).map(|_| None).collect();
    while let Some(res) = set.join_next().await {
        let (idx, bytes) =
            res.map_err(|e| Error::Corrupt(format!("append index join error: {e}")))??;
        slots[idx] = Some(bytes);
    }
    Ok(slots
        .into_iter()
        .map(|slot| slot.expect("every append slot is filled exactly once"))
        .collect())
}

async fn load_vector_version_map(
    ns: &Namespace,
    meta: &VectorIndexMeta,
) -> Result<Option<VectorVersionMap>> {
    let Some(key) = &meta.version_map_key else {
        return Ok(None);
    };
    Ok(Some(VectorVersionMap::decode(
        &ns.store().get(key).await?.bytes,
    )?))
}

async fn exact_vector_fallback(
    ns: &Namespace,
    manifest: &NamespaceManifest,
    filter: Option<&FilterExpr>,
    exact_query: &ExactVectorQuery,
    metric: DistanceMetric,
    dim: usize,
    limit: Option<usize>,
) -> Result<QueryResult> {
    let mut candidates = materialize_candidates(ns, manifest, filter).await?;
    score_vectors(&mut candidates, exact_query, metric, dim)?;
    candidates.retain(|row| row.score.is_some());
    candidates.sort_by(compare_score_rows);
    candidates.truncate(keep_count(exact_query.k, limit));
    Ok(QueryResult {
        rows: candidates,
        aggregates: Vec::new(),
    })
}

/// Rows to keep for a vector query: the top `k`, further capped by an explicit
/// `limit` when the caller asked for fewer than `k`.
fn keep_count(k: usize, limit: Option<usize>) -> usize {
    limit.map_or(k, |limit| limit.min(k))
}

/// Score, prune, sort, and truncate `candidates` for an exact (rerank) vector
/// query. Shared by the exact arm and the order_by/aggregate-forced approx arm.
fn finish_exact_vector(
    candidates: &mut Vec<QueryRow>,
    manifest: &NamespaceManifest,
    vector_query: &ExactVectorQuery,
) -> Result<()> {
    let (dim, default_metric) = vector_schema(manifest, vector_query)?;
    if vector_query.vector.len() != dim {
        return Err(Error::InvalidQuery(format!(
            "query vector for '{}' has dim {}, expected {dim}",
            vector_query.column,
            vector_query.vector.len()
        )));
    }
    let metric = vector_query.metric.unwrap_or(default_metric);
    score_vectors(candidates, vector_query, metric, dim)?;
    candidates.retain(|row| row.score.is_some());
    candidates.sort_by(compare_score_rows);
    candidates.truncate(vector_query.k);
    Ok(())
}

async fn materialize_candidates(
    ns: &Namespace,
    manifest: &NamespaceManifest,
    filter: Option<&FilterExpr>,
) -> Result<Vec<QueryRow>> {
    if let Some(filter) = filter
        && let Some(mut ids) = indexed_filter_candidate_ids(ns, manifest, filter).await?
    {
        let commit = ns.commit_cursor().await?;
        let overlay = ns.read_overlay_ops(manifest.indexed_cursor, commit).await?;
        for op in &overlay {
            ids.insert(op_id(op).clone());
        }

        // Resolve every candidate in one pass (each doc SST read once) instead of
        // one `ns.lookup` per id, which would re-read the manifest, cursor, SSTs,
        // and overlay for each candidate.
        let resolved = ns.resolve_ids(manifest, &overlay, &ids).await?;
        let mut rows = Vec::new();
        for id in ids {
            let Some(document) = resolved.get(&id) else {
                continue;
            };
            if filter_matches(filter, document)? {
                rows.push(QueryRow {
                    id,
                    document: document.clone(),
                    score: None,
                });
            }
        }
        return Ok(rows);
    }

    let docs = ns.replay().await?;
    let mut rows = Vec::new();
    for (id, document) in docs {
        if matches_filter(filter, &document)? {
            rows.push(QueryRow {
                id,
                document,
                score: None,
            });
        }
    }
    Ok(rows)
}

async fn indexed_filter_candidate_ids(
    ns: &Namespace,
    manifest: &NamespaceManifest,
    filter: &FilterExpr,
) -> Result<Option<BTreeSet<Id>>> {
    let Some(meta) = manifest.attr_ssts.first() else {
        return Ok(None);
    };
    let reader = ns.load_sst(&meta.key).await?;
    candidate_ids_from_filter(&reader, filter)
}

fn candidate_ids_from_filter(
    reader: &crate::sst::SstReader,
    filter: &FilterExpr,
) -> Result<Option<BTreeSet<Id>>> {
    match filter {
        FilterExpr::Eq { column, value } => attr::ids_for_eq(reader, column, value),
        FilterExpr::Range {
            column,
            lower,
            upper,
        } => attr::ids_for_range(
            reader,
            column,
            lower.as_ref().map(range_bound_to_attr_bound),
            upper.as_ref().map(range_bound_to_attr_bound),
        ),
        FilterExpr::And(filters) => {
            let mut out: Option<BTreeSet<Id>> = None;
            for filter in filters {
                let Some(ids) = candidate_ids_from_filter(reader, filter)? else {
                    return Ok(None);
                };
                out = Some(match out {
                    Some(existing) => existing.intersection(&ids).cloned().collect(),
                    None => ids,
                });
            }
            Ok(Some(match out {
                Some(ids) => ids,
                None => attr::all_ids(reader)?,
            }))
        }
        FilterExpr::Or(filters) => {
            let mut out = BTreeSet::new();
            for filter in filters {
                let Some(ids) = candidate_ids_from_filter(reader, filter)? else {
                    return Ok(None);
                };
                out.extend(ids);
            }
            Ok(Some(out))
        }
        FilterExpr::Not(filter) => {
            let Some(ids) = candidate_ids_from_filter(reader, filter)? else {
                return Ok(None);
            };
            let all = attr::all_ids(reader)?;
            Ok(Some(all.difference(&ids).cloned().collect()))
        }
    }
}

fn range_bound_to_attr_bound(bound: &RangeBound) -> AttrBound<'_> {
    match bound {
        RangeBound::Included(value) => AttrBound {
            value,
            inclusive: true,
        },
        RangeBound::Excluded(value) => AttrBound {
            value,
            inclusive: false,
        },
    }
}

fn matches_filter(filter: Option<&FilterExpr>, document: &Document) -> Result<bool> {
    match filter {
        Some(filter) => filter_matches(filter, document),
        None => Ok(true),
    }
}

fn filter_matches(filter: &FilterExpr, document: &Document) -> Result<bool> {
    match filter {
        FilterExpr::Eq { column, value } => Ok(document
            .attributes
            .get(column)
            .is_some_and(|actual| eq_filter_value(actual, value))),
        FilterExpr::Range {
            column,
            lower,
            upper,
        } => Ok(document
            .attributes
            .get(column)
            .is_some_and(|actual| range_filter_value(actual, lower.as_ref(), upper.as_ref()))),
        FilterExpr::And(filters) => {
            for filter in filters {
                if !filter_matches(filter, document)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        FilterExpr::Or(filters) => {
            for filter in filters {
                if filter_matches(filter, document)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        FilterExpr::Not(filter) => Ok(!filter_matches(filter, document)?),
    }
}

fn eq_filter_value(actual: &Value, expected: &Value) -> bool {
    match actual {
        Value::Array(values) => {
            if matches!(expected, Value::Array(_)) {
                actual == expected
            } else {
                values.iter().any(|value| scalar_eq(value, expected))
            }
        }
        _ => scalar_eq(actual, expected),
    }
}

fn range_filter_value(
    actual: &Value,
    lower: Option<&RangeBound>,
    upper: Option<&RangeBound>,
) -> bool {
    match actual {
        Value::Array(values) => values
            .iter()
            .any(|value| scalar_in_range(value, lower, upper)),
        _ => scalar_in_range(actual, lower, upper),
    }
}

fn scalar_in_range(value: &Value, lower: Option<&RangeBound>, upper: Option<&RangeBound>) -> bool {
    if let Some(lower) = lower {
        let Some(ord) = compare_scalars(value, lower.value()) else {
            return false;
        };
        match lower {
            RangeBound::Included(_) if ord == Ordering::Less => return false,
            RangeBound::Excluded(_) if ord != Ordering::Greater => return false,
            _ => {}
        }
    }
    if let Some(upper) = upper {
        let Some(ord) = compare_scalars(value, upper.value()) else {
            return false;
        };
        match upper {
            RangeBound::Included(_) if ord == Ordering::Greater => return false,
            RangeBound::Excluded(_) if ord != Ordering::Less => return false,
            _ => {}
        }
    }
    true
}

impl RangeBound {
    fn value(&self) -> &Value {
        match self {
            RangeBound::Included(value) | RangeBound::Excluded(value) => value,
        }
    }
}

fn compute_aggregates(aggregates: &[Aggregate], rows: &[QueryRow]) -> Result<Vec<AggregateResult>> {
    let mut out = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        match aggregate {
            Aggregate::Count => out.push(AggregateResult::Count(rows.len() as u64)),
            Aggregate::Sum { column } => {
                let mut total = 0.0f64;
                let mut value_count = 0u64;
                for row in rows {
                    let Some(value) = row.document.attributes.get(column) else {
                        continue;
                    };
                    match numeric_value(value) {
                        Some(v) => {
                            total += v;
                            value_count += 1;
                        }
                        None => {
                            return Err(Error::InvalidQuery(format!(
                                "cannot sum non-numeric column '{column}'"
                            )));
                        }
                    }
                }
                out.push(AggregateResult::Sum {
                    column: column.clone(),
                    value_count,
                    total,
                });
            }
        }
    }
    Ok(out)
}

fn sort_rows(rows: &mut [QueryRow], order_by: &OrderBy) -> Result<()> {
    match &order_by.target {
        OrderTarget::Id => {
            rows.sort_by(|a, b| apply_direction(a.id.cmp(&b.id), order_by.direction));
            Ok(())
        }
        OrderTarget::Attribute(column) => {
            for row in rows.iter() {
                if let Some(value) = row.document.attributes.get(column)
                    && !is_sortable_value(value)
                {
                    return Err(Error::InvalidQuery(format!(
                        "cannot order by non-scalar column '{column}'"
                    )));
                }
            }
            rows.sort_by(|a, b| {
                let av = a.document.attributes.get(column);
                let bv = b.document.attributes.get(column);
                compare_optional_values(av, bv, order_by.direction).then_with(|| a.id.cmp(&b.id))
            });
            Ok(())
        }
    }
}

fn compare_optional_values(
    a: Option<&Value>,
    b: Option<&Value>,
    direction: SortDirection,
) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => {
            apply_direction(compare_scalars(a, b).unwrap_or(Ordering::Equal), direction)
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn apply_direction(ordering: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Asc => ordering,
        SortDirection::Desc => ordering.reverse(),
    }
}

fn is_sortable_value(value: &Value) -> bool {
    matches!(
        value,
        Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_)
    )
}

fn vector_schema(
    manifest: &NamespaceManifest,
    vector_query: &ExactVectorQuery,
) -> Result<(usize, DistanceMetric)> {
    if vector_query.k == 0 {
        return Err(Error::InvalidQuery(
            "exact vector query k must be greater than zero".into(),
        ));
    }
    if vector_query.vector.is_empty() {
        return Err(Error::InvalidQuery(
            "exact vector query vector cannot be empty".into(),
        ));
    }
    if vector_query.vector.iter().any(|v| !v.is_finite()) {
        return Err(Error::InvalidQuery(
            "exact vector query contains a non-finite value".into(),
        ));
    }
    vector_column_schema(manifest, &vector_query.column)
}

fn vector_column_schema(
    manifest: &NamespaceManifest,
    column: &str,
) -> Result<(usize, DistanceMetric)> {
    let spec = manifest
        .schema
        .columns
        .get(column)
        .ok_or_else(|| Error::InvalidQuery(format!("unknown vector column '{column}'")))?;
    match &spec.column_type {
        ColumnType::Vector { dim, metric, .. } => Ok((*dim, *metric)),
        other => Err(Error::InvalidQuery(format!(
            "column '{}' is not a vector column: {:?}",
            column, other
        ))),
    }
}

struct RecallCandidate {
    id: Id,
    vector: Vec<f32>,
}

async fn recall_candidates(
    ns: &Namespace,
    column: &str,
    dim: usize,
    filter: Option<&FilterExpr>,
) -> Result<Vec<RecallCandidate>> {
    let docs = ns.replay().await?;
    let mut out = Vec::new();
    for (id, document) in docs {
        if !matches_filter(filter, &document)? {
            continue;
        }
        let Some(vector) = document.vectors.get(column) else {
            continue;
        };
        let values = vector::vector_to_f32(vector);
        if values.len() != dim {
            return Err(Error::Corrupt(format!(
                "stored vector '{column}' for {:?} has dim {}, expected {dim}",
                id,
                values.len()
            )));
        }
        out.push(RecallCandidate { id, vector: values });
    }
    Ok(out)
}

fn native_filter_mask(
    index: &VectorIndex,
    filter: &FilterExpr,
) -> Result<Option<VectorFilterMask>> {
    match filter {
        FilterExpr::Eq { column, value } => {
            if matches!(value, Value::Array(_)) {
                return Ok(None);
            }
            Ok(index.filter_mask_by_value(column, |actual| eq_filter_value(actual, value)))
        }
        FilterExpr::Range {
            column,
            lower,
            upper,
        } => {
            if lower
                .as_ref()
                .is_some_and(|bound| !is_sortable_value(bound.value()))
                || upper
                    .as_ref()
                    .is_some_and(|bound| !is_sortable_value(bound.value()))
            {
                return Ok(None);
            }
            Ok(index.filter_mask_by_value(column, |actual| {
                range_filter_value(actual, lower.as_ref(), upper.as_ref())
            }))
        }
        FilterExpr::And(filters) => {
            let mut out = index.all_filter_mask();
            for filter in filters {
                let Some(mask) = native_filter_mask(index, filter)? else {
                    return Ok(None);
                };
                out = out.and(&mask);
            }
            Ok(Some(out))
        }
        FilterExpr::Or(filters) => {
            let mut out = index.empty_filter_mask();
            for filter in filters {
                let Some(mask) = native_filter_mask(index, filter)? else {
                    return Ok(None);
                };
                out = out.or(&mask);
            }
            Ok(Some(out))
        }
        FilterExpr::Not(filter) => {
            let Some(mask) = native_filter_mask(index, filter)? else {
                return Ok(None);
            };
            Ok(Some(mask.not()))
        }
    }
}

fn stable_sample_key(id: &Id, column: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in column.as_bytes().iter().chain(encode_id(id).iter()) {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    splitmix64(hash)
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn score_vectors(
    rows: &mut [QueryRow],
    vector_query: &ExactVectorQuery,
    metric: DistanceMetric,
    dim: usize,
) -> Result<()> {
    for row in rows {
        let Some(vector) = row.document.vectors.get(&vector_query.column) else {
            continue;
        };
        let values = vector::vector_to_f32(vector);
        if values.len() != dim {
            return Err(Error::Corrupt(format!(
                "stored vector '{}' for {:?} has dim {}, expected {dim}",
                vector_query.column,
                row.id,
                values.len()
            )));
        }
        row.score = Some(vector::score(&vector_query.vector, &values, metric)?);
    }
    Ok(())
}

fn compare_score_rows(a: &QueryRow, b: &QueryRow) -> Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.id.cmp(&b.id))
}

fn numeric_value(value: &Value) -> Option<f64> {
    match value {
        Value::Int(v) => Some(*v as f64),
        Value::Float(v) if v.is_finite() => Some(*v),
        _ => None,
    }
}
