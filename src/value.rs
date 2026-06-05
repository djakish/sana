use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A document primary key. The architecture allows `u64`, UUID, or short
/// string keys; all three round-trip through the WAL and storage layers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Id {
    U64(u64),
    Uuid([u8; 16]),
    String(String),
}

/// A scalar or array attribute value used for filtering, ordering, aggregation,
/// and return. Vectors are modeled separately via [`VectorValue`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
}

/// Compare two scalar [`Value`]s with numeric coercion, returning `None` for
/// incomparable inputs (mismatched non-numeric types, `NaN`, `Null`, `Array`).
///
/// This is the single source of truth for scalar comparison: the query-path
/// recheck, attribute-index range scans, and order-by all call it, so they can
/// never disagree on cross-type cases like `Int(5)` vs `Float(5.0)`. Integers
/// compare exactly (not via `f64`) so large `i64` values keep full precision.
pub fn compare_scalars(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// Scalar equality with the same numeric coercion as [`compare_scalars`]
/// (`Int(5) == Float(5.0)`, `+0.0 == -0.0`). Falls back to structural equality
/// for inputs the comparator cannot order (e.g. `Null`/`Null`).
pub fn scalar_eq(a: &Value, b: &Value) -> bool {
    match compare_scalars(a, b) {
        Some(ordering) => ordering == Ordering::Equal,
        None => a == b,
    }
}

/// A vector column value. `F16` stores raw IEEE half-precision bits, decoded to
/// `f32` via the `half` crate when a kernel needs float values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VectorValue {
    F32(Vec<f32>),
    F16(Vec<u16>),
}

impl VectorValue {
    pub fn dim(&self) -> usize {
        match self {
            VectorValue::F32(v) => v.len(),
            VectorValue::F16(v) => v.len(),
        }
    }
}

/// A fully materialized document: its key, vector columns, and attributes.
/// Maps are `BTreeMap` so serialization is deterministic (golden-testable).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: Id,
    #[serde(default)]
    pub vectors: BTreeMap<String, VectorValue>,
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
}

impl Document {
    pub fn new(id: Id) -> Self {
        Self {
            id,
            vectors: BTreeMap::new(),
            attributes: BTreeMap::new(),
        }
    }
}
