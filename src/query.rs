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
use crate::error::{Error, Result};
use crate::namespace::{Namespace, op_id};
use crate::schema::{ColumnType, DistanceMetric};
use crate::value::{Document, Id, Value, VectorValue};
use crate::vector::{self, VectorIndex};

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

pub async fn execute(ns: &Namespace, query: Query) -> Result<QueryResult> {
    let manifest = ns.load_manifest().await?;
    if query.exact_vector.is_some() && query.approx_vector.is_some() {
        return Err(Error::InvalidQuery(
            "query cannot specify both exact_vector and approx_vector".into(),
        ));
    }

    if let Some(ann_query) = &query.approx_vector
        && query.filter.is_none()
        && query.order_by.is_none()
        && query.aggregates.is_empty()
    {
        return execute_ann_vector(ns, &manifest, ann_query).await;
    }

    let mut candidates = materialize_candidates(ns, &manifest, query.filter.as_ref()).await?;

    let aggregates = compute_aggregates(&query.aggregates, &candidates)?;

    if let Some(vector_query) = &query.exact_vector {
        let (dim, default_metric) = vector_schema(&manifest, vector_query)?;
        if vector_query.vector.len() != dim {
            return Err(Error::InvalidQuery(format!(
                "query vector for '{}' has dim {}, expected {dim}",
                vector_query.column,
                vector_query.vector.len()
            )));
        }
        let metric = vector_query.metric.unwrap_or(default_metric);
        score_vectors(&mut candidates, vector_query, metric, dim)?;
        candidates.retain(|row| row.score.is_some());
        candidates.sort_by(compare_score_rows);
        candidates.truncate(vector_query.k);
    } else if let Some(vector_query) = &query.approx_vector {
        let exact_query = ExactVectorQuery {
            column: vector_query.column.clone(),
            vector: vector_query.vector.clone(),
            k: vector_query.k,
            metric: vector_query.metric,
        };
        let (dim, default_metric) = vector_schema(&manifest, &exact_query)?;
        if exact_query.vector.len() != dim {
            return Err(Error::InvalidQuery(format!(
                "query vector for '{}' has dim {}, expected {dim}",
                exact_query.column,
                exact_query.vector.len()
            )));
        }
        let metric = exact_query.metric.unwrap_or(default_metric);
        score_vectors(&mut candidates, &exact_query, metric, dim)?;
        candidates.retain(|row| row.score.is_some());
        candidates.sort_by(compare_score_rows);
        candidates.truncate(exact_query.k);
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

async fn execute_ann_vector(
    ns: &Namespace,
    manifest: &crate::manifest::NamespaceManifest,
    query: &ApproxVectorQuery,
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
        let mut candidates = materialize_candidates(ns, manifest, None).await?;
        score_vectors(&mut candidates, &exact_query, metric, dim)?;
        candidates.retain(|row| row.score.is_some());
        candidates.sort_by(compare_score_rows);
        candidates.truncate(query.k);
        return Ok(QueryResult {
            rows: candidates,
            aggregates: Vec::new(),
        });
    };

    let index_bytes = ns.store().get(&meta.key).await?.bytes;
    let index = VectorIndex::decode(&index_bytes)?;
    let ann_hits = index.search(&query.vector, query.k, query.probes, Some(metric))?;

    let commit = ns.commit_cursor().await?;
    let mut touched = BTreeSet::new();
    for op in ns.read_overlay_ops(manifest.indexed_cursor, commit).await? {
        touched.insert(op_id(&op).clone());
    }

    let mut rows = Vec::new();
    for hit in ann_hits {
        if touched.contains(&hit.id) {
            continue;
        }
        if let Some(document) = ns.lookup(&hit.id).await? {
            rows.push(QueryRow {
                id: hit.id,
                document,
                score: Some(hit.score),
            });
        }
    }

    for id in touched {
        let Some(document) = ns.lookup(&id).await? else {
            continue;
        };
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
            document,
            score: Some(vector::score(&query.vector, &values, metric)?),
        });
    }

    rows.sort_by(compare_score_rows);
    rows.truncate(query.k);
    Ok(QueryResult {
        rows,
        aggregates: Vec::new(),
    })
}

async fn materialize_candidates(
    ns: &Namespace,
    manifest: &crate::manifest::NamespaceManifest,
    filter: Option<&FilterExpr>,
) -> Result<Vec<QueryRow>> {
    if let Some(filter) = filter
        && let Some(mut ids) = indexed_filter_candidate_ids(ns, manifest, filter).await?
    {
        let commit = ns.commit_cursor().await?;
        for op in ns.read_overlay_ops(manifest.indexed_cursor, commit).await? {
            ids.insert(op_id(&op).clone());
        }

        let mut rows = Vec::new();
        for id in ids {
            let Some(document) = ns.lookup(&id).await? else {
                continue;
            };
            if filter_matches(filter, &document)? {
                rows.push(QueryRow {
                    id,
                    document,
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
    manifest: &crate::manifest::NamespaceManifest,
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

fn scalar_eq(actual: &Value, expected: &Value) -> bool {
    if let (Some(a), Some(b)) = (numeric_value(actual), numeric_value(expected)) {
        return a == b;
    }
    actual == expected
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
        let Some(ord) = compare_bound_value(value, lower.value()) else {
            return false;
        };
        match lower {
            RangeBound::Included(_) if ord == Ordering::Less => return false,
            RangeBound::Excluded(_) if ord != Ordering::Greater => return false,
            _ => {}
        }
    }
    if let Some(upper) = upper {
        let Some(ord) = compare_bound_value(value, upper.value()) else {
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
        (Some(a), Some(b)) => apply_direction(
            compare_bound_value(a, b).unwrap_or(Ordering::Equal),
            direction,
        ),
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
    manifest: &crate::manifest::NamespaceManifest,
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
    let spec = manifest
        .schema
        .columns
        .get(&vector_query.column)
        .ok_or_else(|| {
            Error::InvalidQuery(format!("unknown vector column '{}'", vector_query.column))
        })?;
    match &spec.column_type {
        ColumnType::Vector { dim, metric, .. } => Ok((*dim, *metric)),
        other => Err(Error::InvalidQuery(format!(
            "column '{}' is not a vector column: {:?}",
            vector_query.column, other
        ))),
    }
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
        let values = vector_to_f32(vector);
        if values.len() != dim {
            return Err(Error::Corrupt(format!(
                "stored vector '{}' for {:?} has dim {}, expected {dim}",
                vector_query.column,
                row.id,
                values.len()
            )));
        }
        row.score = Some(vector_score(&vector_query.vector, &values, metric)?);
    }
    Ok(())
}

fn vector_to_f32(vector: &VectorValue) -> Vec<f32> {
    vector::vector_to_f32(vector)
}

fn vector_score(query: &[f32], candidate: &[f32], metric: DistanceMetric) -> Result<f32> {
    vector::score(query, candidate, metric)
}

fn compare_score_rows(a: &QueryRow, b: &QueryRow) -> Ordering {
    b.score
        .partial_cmp(&a.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.id.cmp(&b.id))
}

fn compare_bound_value(a: &Value, b: &Value) -> Option<Ordering> {
    if let (Some(a), Some(b)) = (numeric_value(a), numeric_value(b)) {
        return a.partial_cmp(&b);
    }
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

fn numeric_value(value: &Value) -> Option<f64> {
    match value {
        Value::Int(v) => Some(*v as f64),
        Value::Float(v) if v.is_finite() => Some(*v),
        _ => None,
    }
}
