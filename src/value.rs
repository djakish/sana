//! `Id`, `Value`, and `VectorValue` serialize two ways, chosen by the format's
//! `is_human_readable()`. JSON (the HTTP API, the manifest) gets plain scalars —
//! `4.5`, `"fantasy"`, `[0.1, 0.2]` — typed by the JSON token and the schema.
//! postcard (the WAL and SSTs) keeps the tagged enum encoding, because it is not
//! self-describing and cannot round-trip a tag-less scalar; the on-disk bytes
//! are therefore unchanged. There is no compatibility with the old tagged JSON.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A document primary key. On the JSON wire it is a bare number or string; a
/// string in canonical hyphenated UUID form is read as [`Id::Uuid`] (so it
/// round-trips and uses the 16-byte key encoding), any other string as
/// [`Id::String`]. A 36-char hyphenated-hex string therefore cannot be a
/// `String` id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Id {
    U64(u64),
    Uuid([u8; 16]),
    String(String),
}

/// A scalar or array attribute value used for filtering, ordering, aggregation,
/// and return. Vectors are modeled separately via [`VectorValue`].
#[derive(Clone, Debug, PartialEq)]
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
/// `f32` via the `half` crate when a kernel needs float values. On the JSON wire
/// a vector is always a plain array of floats; the storage encoding is a schema
/// property, not a per-value tag.
#[derive(Clone, Debug, PartialEq)]
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

fn uuid_to_hyphenated(bytes: &[u8; 16]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Parse exactly the canonical 8-4-4-4-12 hex form; any other shape is `None`
/// and is treated as a plain string id.
fn parse_uuid_hyphenated(s: &str) -> Option<[u8; 16]> {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    if bytes.get(8).copied() != Some(b'-')
        || bytes.get(13).copied() != Some(b'-')
        || bytes.get(18).copied() != Some(b'-')
        || bytes.get(23).copied() != Some(b'-')
    {
        return None;
    }
    let mut out = [0u8; 16];
    let mut out_index = 0usize;
    let mut high: Option<u8> = None;
    for (i, &c) in bytes.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            continue;
        }
        let nibble = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        match high {
            None => high = Some(nibble),
            Some(h) => {
                let slot = out.get_mut(out_index)?;
                *slot = (h << 4) | nibble;
                out_index += 1;
                high = None;
            }
        }
    }
    (out_index == 16 && high.is_none()).then_some(out)
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            match self {
                Id::U64(v) => serializer.serialize_u64(*v),
                Id::Uuid(bytes) => serializer.serialize_str(&uuid_to_hyphenated(bytes)),
                Id::String(s) => serializer.serialize_str(s),
            }
        } else {
            match self {
                Id::U64(v) => serializer.serialize_newtype_variant("Id", 0, "U64", v),
                Id::Uuid(bytes) => serializer.serialize_newtype_variant("Id", 1, "Uuid", bytes),
                Id::String(s) => serializer.serialize_newtype_variant("Id", 2, "String", s),
            }
        }
    }
}

struct IdVisitor;

impl<'de> Visitor<'de> for IdVisitor {
    type Value = Id;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a non-negative integer, a UUID string, or a string id")
    }

    fn visit_u64<E>(self, v: u64) -> Result<Id, E> {
        Ok(Id::U64(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Id, E> {
        u64::try_from(v)
            .map(Id::U64)
            .map_err(|error| E::custom(format!("integer id must be non-negative: {error}")))
    }

    fn visit_str<E>(self, v: &str) -> Result<Id, E> {
        Ok(parse_uuid_hyphenated(v)
            .map(Id::Uuid)
            .unwrap_or_else(|| Id::String(v.to_string())))
    }

    fn visit_string<E>(self, v: String) -> Result<Id, E> {
        Ok(match parse_uuid_hyphenated(&v) {
            Some(bytes) => Id::Uuid(bytes),
            None => Id::String(v),
        })
    }
}

#[derive(Deserialize)]
enum IdRepr {
    U64(u64),
    Uuid([u8; 16]),
    String(String),
}

impl From<IdRepr> for Id {
    fn from(repr: IdRepr) -> Self {
        match repr {
            IdRepr::U64(v) => Id::U64(v),
            IdRepr::Uuid(b) => Id::Uuid(b),
            IdRepr::String(s) => Id::String(s),
        }
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            deserializer.deserialize_any(IdVisitor)
        } else {
            IdRepr::deserialize(deserializer).map(Id::from)
        }
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            match self {
                Value::Null => serializer.serialize_none(),
                Value::Bool(b) => serializer.serialize_bool(*b),
                Value::Int(i) => serializer.serialize_i64(*i),
                Value::Float(f) => serializer.serialize_f64(*f),
                Value::String(s) => serializer.serialize_str(s),
                Value::Array(a) => a.serialize(serializer),
            }
        } else {
            match self {
                Value::Null => serializer.serialize_unit_variant("Value", 0, "Null"),
                Value::Bool(b) => serializer.serialize_newtype_variant("Value", 1, "Bool", b),
                Value::Int(i) => serializer.serialize_newtype_variant("Value", 2, "Int", i),
                Value::Float(f) => serializer.serialize_newtype_variant("Value", 3, "Float", f),
                Value::String(s) => serializer.serialize_newtype_variant("Value", 4, "String", s),
                Value::Array(a) => serializer.serialize_newtype_variant("Value", 5, "Array", a),
            }
        }
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON null, bool, number, string, or array")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Int(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        i64::try_from(v).map(Value::Int).map_err(|error| {
            E::custom(format!(
                "integer attribute exceeds i64::MAX; store wider integers as strings: {v} ({error})"
            ))
        })
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(Value::Float(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }
}

#[derive(Deserialize)]
enum ValueRepr {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
}

impl From<ValueRepr> for Value {
    fn from(repr: ValueRepr) -> Self {
        match repr {
            ValueRepr::Null => Value::Null,
            ValueRepr::Bool(b) => Value::Bool(b),
            ValueRepr::Int(i) => Value::Int(i),
            ValueRepr::Float(f) => Value::Float(f),
            ValueRepr::String(s) => Value::String(s),
            ValueRepr::Array(a) => Value::Array(a),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            deserializer.deserialize_any(ValueVisitor)
        } else {
            ValueRepr::deserialize(deserializer).map(Value::from)
        }
    }
}

impl Serialize for VectorValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            match self {
                VectorValue::F32(v) => v.serialize(serializer),
                VectorValue::F16(v) => {
                    let floats: Vec<f32> = v
                        .iter()
                        .map(|bits| half::f16::from_bits(*bits).to_f32())
                        .collect();
                    floats.serialize(serializer)
                }
            }
        } else {
            match self {
                VectorValue::F32(v) => {
                    serializer.serialize_newtype_variant("VectorValue", 0, "F32", v)
                }
                VectorValue::F16(v) => {
                    serializer.serialize_newtype_variant("VectorValue", 1, "F16", v)
                }
            }
        }
    }
}

#[derive(Deserialize)]
enum VectorValueRepr {
    F32(Vec<f32>),
    F16(Vec<u16>),
}

impl From<VectorValueRepr> for VectorValue {
    fn from(repr: VectorValueRepr) -> Self {
        match repr {
            VectorValueRepr::F32(v) => VectorValue::F32(v),
            VectorValueRepr::F16(v) => VectorValue::F16(v),
        }
    }
}

impl<'de> Deserialize<'de> for VectorValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            Vec::<f32>::deserialize(deserializer).map(VectorValue::F32)
        } else {
            VectorValueRepr::deserialize(deserializer).map(VectorValue::from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_json_is_plain() {
        assert_eq!(serde_json::to_string(&Value::Null).unwrap(), "null");
        assert_eq!(serde_json::to_string(&Value::Bool(true)).unwrap(), "true");
        assert_eq!(serde_json::to_string(&Value::Int(7)).unwrap(), "7");
        assert_eq!(serde_json::to_string(&Value::Float(4.5)).unwrap(), "4.5");
        assert_eq!(
            serde_json::to_string(&Value::String("hi".into())).unwrap(),
            "\"hi\""
        );
        assert_eq!(
            serde_json::to_string(&Value::Array(vec![
                Value::Int(1),
                Value::String("a".into())
            ]))
            .unwrap(),
            "[1,\"a\"]"
        );
    }

    #[test]
    fn value_json_round_trips_and_keeps_int_float_distinction() {
        assert_eq!(serde_json::from_str::<Value>("4").unwrap(), Value::Int(4));
        assert_eq!(
            serde_json::from_str::<Value>("4.0").unwrap(),
            Value::Float(4.0)
        );
        assert_eq!(
            serde_json::from_str::<Value>("\"x\"").unwrap(),
            Value::String("x".into())
        );
        assert_eq!(
            serde_json::from_str::<Value>("true").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(serde_json::from_str::<Value>("null").unwrap(), Value::Null);
        assert_eq!(
            serde_json::from_str::<Value>("[1, \"a\", false]").unwrap(),
            Value::Array(vec![
                Value::Int(1),
                Value::String("a".into()),
                Value::Bool(false)
            ])
        );
    }

    #[test]
    fn value_json_rejects_unsigned_integers_outside_i64_range() {
        assert_eq!(
            serde_json::from_str::<Value>("9007199254740993").unwrap(),
            Value::Int(9_007_199_254_740_993)
        );
        assert_eq!(
            serde_json::from_str::<Value>("9223372036854775807").unwrap(),
            Value::Int(i64::MAX)
        );
        assert!(serde_json::from_str::<Value>("9223372036854775808").is_err());
        assert!(serde_json::from_str::<Value>("18446744073709551615").is_err());
    }

    #[test]
    fn id_json_is_plain_and_round_trips() {
        assert_eq!(serde_json::to_string(&Id::U64(1)).unwrap(), "1");
        assert_eq!(
            serde_json::to_string(&Id::String("doc-1".into())).unwrap(),
            "\"doc-1\""
        );
        assert_eq!(serde_json::from_str::<Id>("1").unwrap(), Id::U64(1));
        assert_eq!(
            serde_json::from_str::<Id>("\"doc-1\"").unwrap(),
            Id::String("doc-1".into())
        );
    }

    #[test]
    fn id_uuid_string_round_trips_losslessly() {
        let uuid = Id::Uuid([
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ]);
        let json = serde_json::to_string(&uuid).unwrap();
        assert_eq!(json, "\"550e8400-e29b-41d4-a716-446655440000\"");
        assert_eq!(serde_json::from_str::<Id>(&json).unwrap(), uuid);
        assert_eq!(
            serde_json::from_str::<Id>("\"not-a-uuid\"").unwrap(),
            Id::String("not-a-uuid".into())
        );
    }

    #[test]
    fn vector_json_is_plain_float_array() {
        assert_eq!(
            serde_json::to_string(&VectorValue::F32(vec![1.0, 2.5])).unwrap(),
            "[1.0,2.5]"
        );
        assert_eq!(
            serde_json::from_str::<VectorValue>("[1, 2.5]").unwrap(),
            VectorValue::F32(vec![1.0, 2.5])
        );
        let one = half::f16::from_f32(1.0).to_bits();
        assert_eq!(
            serde_json::to_string(&VectorValue::F16(vec![one])).unwrap(),
            "[1.0]"
        );
    }

    #[test]
    fn binary_branch_round_trips_through_postcard() {
        for value in [
            Value::Null,
            Value::Bool(true),
            Value::Int(-9),
            Value::Float(3.5),
            Value::String("hi".into()),
            Value::Array(vec![Value::Int(1), Value::Array(vec![Value::Bool(false)])]),
        ] {
            let bytes = postcard::to_allocvec(&value).unwrap();
            assert_eq!(postcard::from_bytes::<Value>(&bytes).unwrap(), value);
        }
        for id in [Id::U64(7), Id::Uuid([3; 16]), Id::String("k".into())] {
            let bytes = postcard::to_allocvec(&id).unwrap();
            assert_eq!(postcard::from_bytes::<Id>(&bytes).unwrap(), id);
        }
        for vector in [
            VectorValue::F32(vec![1.0, 2.0]),
            VectorValue::F16(vec![1, 2]),
        ] {
            let bytes = postcard::to_allocvec(&vector).unwrap();
            assert_eq!(postcard::from_bytes::<VectorValue>(&bytes).unwrap(), vector);
        }
    }

    #[test]
    fn binary_branch_keeps_the_tagged_discriminant() {
        assert_eq!(postcard::to_allocvec(&Value::Null).unwrap(), vec![0]);
        assert_eq!(
            postcard::to_allocvec(&Value::Bool(true)).unwrap(),
            vec![1, 1]
        );
        assert_eq!(postcard::to_allocvec(&Id::U64(1)).unwrap(), vec![0, 1]);
    }
}
