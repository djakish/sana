use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScalarType {
    Bool,
    Int,
    Float,
    String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VectorEncoding {
    F32,
    F16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistanceMetric {
    L2,
    Cosine,
    Dot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ColumnType {
    Scalar(ScalarType),
    Array(ScalarType),
    Vector {
        dim: usize,
        encoding: VectorEncoding,
        metric: DistanceMetric,
    },
    FullText,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnSpec {
    pub column_type: ColumnType,
    #[serde(default)]
    pub filterable: bool,
    #[serde(default)]
    pub indexed: bool,
}

/// A namespace schema. `version` increments on schema evolution. Stage 0 only
/// defines the types; inference and validation arrive in Stage 1/3.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Schema {
    #[serde(default)]
    pub columns: BTreeMap<String, ColumnSpec>,
    #[serde(default)]
    pub version: u64,
}
