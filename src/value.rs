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

/// A vector column value. `F16` stores raw IEEE half-precision bits so the
/// engine stays dependency-free until a half-float kernel is needed.
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
