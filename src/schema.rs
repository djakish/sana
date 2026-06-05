use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::value::{Document, Value, VectorValue};
use crate::wal::WalOp;

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

impl Schema {
    /// Infer new columns and validate all typed values in an atomic WAL batch.
    ///
    /// Sana starts strict: once a column is inferred, later writes must keep the
    /// same kind. Null is a patch-only clear operation and never creates a
    /// column. The schema version increments once per batch that adds at least
    /// one column.
    pub fn infer_and_validate_ops(&mut self, ops: &[WalOp]) -> Result<bool> {
        let mut changed = false;
        for op in ops {
            match op {
                WalOp::Upsert { id, document } => {
                    if &document.id != id {
                        return Err(Error::InvalidWrite(
                            "upsert id does not match document id".into(),
                        ));
                    }
                    changed |= self.infer_and_validate_document(document)?;
                }
                WalOp::Patch {
                    attributes,
                    vectors,
                    ..
                } => {
                    for (name, value) in attributes {
                        changed |=
                            self.infer_and_validate_attribute(name, value, WriteMode::Patch)?;
                    }
                    for (name, vector) in vectors {
                        changed |= self.infer_and_validate_vector(name, vector)?;
                    }
                }
                WalOp::Delete { .. } => {}
            }
        }

        if changed {
            self.version = self
                .version
                .checked_add(1)
                .ok_or_else(|| Error::InvalidSchema("schema version overflow".into()))?;
        }
        Ok(changed)
    }

    fn infer_and_validate_document(&mut self, document: &Document) -> Result<bool> {
        let mut changed = false;
        for (name, value) in &document.attributes {
            changed |= self.infer_and_validate_attribute(name, value, WriteMode::FullDocument)?;
        }
        for (name, vector) in &document.vectors {
            changed |= self.infer_and_validate_vector(name, vector)?;
        }
        Ok(changed)
    }

    fn infer_and_validate_attribute(
        &mut self,
        name: &str,
        value: &Value,
        mode: WriteMode,
    ) -> Result<bool> {
        validate_column_name(name)?;
        if matches!(value, Value::Null) {
            return match mode {
                WriteMode::Patch => Ok(false),
                WriteMode::FullDocument => Err(Error::InvalidSchema(format!(
                    "attribute '{name}' is null in a full document; omit it or patch null to clear"
                ))),
            };
        }

        if let Some(spec) = self.columns.get(name) {
            validate_attribute_value(name, value, &spec.column_type)?;
            Ok(false)
        } else {
            let inferred = infer_attribute_type(name, value)?;
            self.columns.insert(
                name.to_string(),
                ColumnSpec {
                    column_type: inferred,
                    filterable: true,
                    indexed: true,
                },
            );
            Ok(true)
        }
    }

    fn infer_and_validate_vector(&mut self, name: &str, vector: &VectorValue) -> Result<bool> {
        validate_column_name(name)?;
        let inferred = infer_vector_type(name, vector)?;
        match self.columns.get(name) {
            Some(spec) => {
                if spec.column_type != inferred {
                    return Err(Error::InvalidSchema(format!(
                        "column '{name}' expected {:?}, got {:?}",
                        spec.column_type, inferred
                    )));
                }
                Ok(false)
            }
            None => {
                self.columns.insert(
                    name.to_string(),
                    ColumnSpec {
                        column_type: inferred,
                        filterable: false,
                        indexed: true,
                    },
                );
                Ok(true)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum WriteMode {
    FullDocument,
    Patch,
}

fn validate_column_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidSchema("column name cannot be empty".into()));
    }
    Ok(())
}

fn infer_attribute_type(name: &str, value: &Value) -> Result<ColumnType> {
    match value {
        Value::Null => Err(Error::InvalidSchema(format!(
            "cannot infer type for null attribute '{name}'"
        ))),
        Value::Bool(_) => Ok(ColumnType::Scalar(ScalarType::Bool)),
        Value::Int(_) => Ok(ColumnType::Scalar(ScalarType::Int)),
        Value::Float(v) => {
            if !v.is_finite() {
                return Err(Error::InvalidSchema(format!(
                    "attribute '{name}' contains a non-finite float"
                )));
            }
            Ok(ColumnType::Scalar(ScalarType::Float))
        }
        Value::String(_) => Ok(ColumnType::Scalar(ScalarType::String)),
        Value::Array(values) => {
            let ty = infer_array_scalar_type(name, values)?;
            Ok(ColumnType::Array(ty))
        }
    }
}

fn validate_attribute_value(name: &str, value: &Value, expected: &ColumnType) -> Result<()> {
    match expected {
        ColumnType::Scalar(expected) => {
            let actual = infer_attribute_type(name, value)?;
            if actual == ColumnType::Scalar(*expected) {
                Ok(())
            } else {
                Err(Error::InvalidSchema(format!(
                    "column '{name}' expected {:?}, got {:?}",
                    expected, actual
                )))
            }
        }
        ColumnType::Array(expected) => match value {
            Value::Array(values) => {
                for item in values {
                    let actual = match item {
                        Value::Bool(_) => ScalarType::Bool,
                        Value::Int(_) => ScalarType::Int,
                        Value::Float(v) => {
                            if !v.is_finite() {
                                return Err(Error::InvalidSchema(format!(
                                    "array attribute '{name}' contains a non-finite float"
                                )));
                            }
                            ScalarType::Float
                        }
                        Value::String(_) => ScalarType::String,
                        Value::Null | Value::Array(_) => {
                            return Err(Error::InvalidSchema(format!(
                                "array attribute '{name}' must contain only non-null scalar values"
                            )));
                        }
                    };
                    if actual != *expected {
                        return Err(Error::InvalidSchema(format!(
                            "column '{name}' expected array of {:?}, got array of {:?}",
                            expected, actual
                        )));
                    }
                }
                Ok(())
            }
            _ => {
                let actual = infer_attribute_type(name, value)?;
                Err(Error::InvalidSchema(format!(
                    "column '{name}' expected array of {:?}, got {:?}",
                    expected, actual
                )))
            }
        },
        ColumnType::Vector { .. } | ColumnType::FullText => {
            let actual = infer_attribute_type(name, value)?;
            Err(Error::InvalidSchema(format!(
                "column '{name}' expected {:?}, got {:?}",
                expected, actual
            )))
        }
    }
}

fn infer_array_scalar_type(name: &str, values: &[Value]) -> Result<ScalarType> {
    let mut inferred: Option<ScalarType> = None;
    for value in values {
        let item_ty = match value {
            Value::Bool(_) => ScalarType::Bool,
            Value::Int(_) => ScalarType::Int,
            Value::Float(v) => {
                if !v.is_finite() {
                    return Err(Error::InvalidSchema(format!(
                        "array attribute '{name}' contains a non-finite float"
                    )));
                }
                ScalarType::Float
            }
            Value::String(_) => ScalarType::String,
            Value::Null | Value::Array(_) => {
                return Err(Error::InvalidSchema(format!(
                    "array attribute '{name}' must contain only non-null scalar values"
                )));
            }
        };
        match inferred {
            Some(existing) if existing != item_ty => {
                return Err(Error::InvalidSchema(format!(
                    "array attribute '{name}' mixes {:?} and {:?}",
                    existing, item_ty
                )));
            }
            Some(_) => {}
            None => inferred = Some(item_ty),
        }
    }
    inferred.ok_or_else(|| {
        Error::InvalidSchema(format!(
            "cannot infer element type for empty array attribute '{name}'"
        ))
    })
}

fn infer_vector_type(name: &str, vector: &VectorValue) -> Result<ColumnType> {
    let dim = vector.dim();
    if dim == 0 {
        return Err(Error::InvalidSchema(format!(
            "vector column '{name}' cannot be empty"
        )));
    }
    match vector {
        VectorValue::F32(values) => {
            if values.iter().any(|v| !v.is_finite()) {
                return Err(Error::InvalidSchema(format!(
                    "vector column '{name}' contains a non-finite f32"
                )));
            }
            Ok(ColumnType::Vector {
                dim,
                encoding: VectorEncoding::F32,
                metric: DistanceMetric::Cosine,
            })
        }
        VectorValue::F16(values) => {
            if values
                .iter()
                .any(|bits| !half::f16::from_bits(*bits).is_finite())
            {
                return Err(Error::InvalidSchema(format!(
                    "vector column '{name}' contains a non-finite f16"
                )));
            }
            Ok(ColumnType::Vector {
                dim,
                encoding: VectorEncoding::F16,
                metric: DistanceMetric::Cosine,
            })
        }
    }
}
