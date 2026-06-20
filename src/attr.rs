//! Attribute-family SST encoding.
//!
//! Stage 3 keeps this intentionally simple: a flush writes one full-snapshot
//! postings SST for the indexed live documents. Keys are
//! `column + encoded_value`, values are sorted `Id` postings. The query path
//! uses these postings for candidate generation, then re-checks materialized
//! documents with the WAL overlay applied for correctness.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::sst::{SstReader, SstWriter};
use crate::value::{Document, Id, Value, compare_scalars};

const KEY_KIND_ALL_DOCS: u8 = 0;
const KEY_KIND_ATTR: u8 = 1;

const VALUE_BOOL: u8 = 1;
const VALUE_INT: u8 = 2;
const VALUE_FLOAT: u8 = 3;
const VALUE_STRING: u8 = 4;

#[derive(Clone, Debug)]
pub struct BuiltAttrSst {
    pub bytes: Vec<u8>,
    pub entry_count: u64,
}

#[derive(Serialize, Deserialize)]
struct PostingList {
    ids: Vec<Id>,
}

impl PostingList {
    fn encode(ids: Vec<Id>) -> Result<Vec<u8>> {
        postcard::to_allocvec(&Self { ids }).map_err(|e| Error::Codec(e.to_string()))
    }

    fn decode(bytes: &[u8]) -> Result<BTreeSet<Id>> {
        let list: Self = postcard::from_bytes(bytes).map_err(|e| Error::Codec(e.to_string()))?;
        Ok(list.ids.into_iter().collect())
    }
}

pub fn build_attr_sst(docs: &BTreeMap<Id, Document>) -> Result<Option<BuiltAttrSst>> {
    if docs.is_empty() {
        return Ok(None);
    }

    let mut postings: BTreeMap<Vec<u8>, BTreeSet<Id>> = BTreeMap::new();
    postings.insert(all_docs_key(), docs.keys().cloned().collect());

    for (id, doc) in docs {
        for (column, value) in &doc.attributes {
            for indexed in indexable_values(value)? {
                postings
                    .entry(attr_key(column, indexed)?)
                    .or_default()
                    .insert(id.clone());
            }
        }
    }

    let mut writer = SstWriter::new();
    let mut entry_count = 0u64;
    for (key, ids) in postings {
        let ids = ids.into_iter().collect();
        writer.add(&key, &PostingList::encode(ids)?)?;
        entry_count += 1;
    }

    Ok(Some(BuiltAttrSst {
        bytes: writer.finish()?,
        entry_count,
    }))
}

/// Merge several attribute postings SSTs into one, unioning the id postings for
/// each key (the all-docs key included). Used by size-tiered attribute
/// compaction to bound read fan-out. Stale entries are *retained* — a value-keyed
/// posting cannot know an id's current value — so this only reduces the number of
/// runs, not their staleness; a full rebuild from live documents (`compact`)
/// removes stale entries. Returns `None` if every input is empty.
pub fn merge_attr_ssts(readers: &[SstReader]) -> Result<Option<BuiltAttrSst>> {
    let mut postings: BTreeMap<Vec<u8>, BTreeSet<Id>> = BTreeMap::new();
    for reader in readers {
        for (key, bytes) in reader.entries()? {
            postings
                .entry(key)
                .or_default()
                .extend(PostingList::decode(&bytes)?);
        }
    }
    if postings.is_empty() {
        return Ok(None);
    }

    let mut writer = SstWriter::new();
    let mut entry_count = 0u64;
    for (key, ids) in postings {
        writer.add(&key, &PostingList::encode(ids.into_iter().collect())?)?;
        entry_count += 1;
    }
    Ok(Some(BuiltAttrSst {
        bytes: writer.finish()?,
        entry_count,
    }))
}

pub fn all_ids(reader: &SstReader) -> Result<BTreeSet<Id>> {
    match reader.get(&all_docs_key())? {
        Some(bytes) => PostingList::decode(&bytes),
        None => Ok(BTreeSet::new()),
    }
}

pub fn ids_for_eq(reader: &SstReader, column: &str, value: &Value) -> Result<Option<BTreeSet<Id>>> {
    // Candidate generation must be a superset of what the query-path recheck
    // (`scalar_eq`) accepts. That recheck coerces numerically — Int(5) == Float(5.0),
    // and +0.0 == -0.0 — but our keys are type-tagged exact bytes (VALUE_INT vs
    // VALUE_FLOAT, distinct zero bits), so a point lookup would miss those
    // cross-type matches and silently drop rows. A numeric Eq is therefore an
    // inclusive degenerate range, decoded and numerically compared like any other
    // range. Bool/String have no cross-type neighbours, so they keep the exact
    // point lookup.
    if matches!(value, Value::Int(_) | Value::Float(_)) {
        let bound = AttrBound {
            value,
            inclusive: true,
        };
        return ids_for_range(reader, column, Some(bound), Some(bound));
    }
    let Some(key) = maybe_attr_key(column, value)? else {
        return Ok(None);
    };
    match reader.get(&key)? {
        Some(bytes) => Ok(Some(PostingList::decode(&bytes)?)),
        None => Ok(Some(BTreeSet::new())),
    }
}

pub fn ids_for_range(
    reader: &SstReader,
    column: &str,
    lower: Option<AttrBound<'_>>,
    upper: Option<AttrBound<'_>>,
) -> Result<Option<BTreeSet<Id>>> {
    if lower.is_some_and(|bound| !can_encode_scalar(bound.value))
        || upper.is_some_and(|bound| !can_encode_scalar(bound.value))
    {
        return Ok(None);
    }

    let prefix = attr_column_prefix(column);
    let mut out = BTreeSet::new();
    for (key, bytes) in reader.entries()? {
        if !key.starts_with(&prefix) {
            continue;
        }
        let value_bytes = key
            .get(prefix.len()..)
            .ok_or_else(|| Error::Corrupt("attribute key prefix out of bounds".into()))?;
        let Some(value) = decode_value_from_key(value_bytes)? else {
            continue;
        };
        if !range_bound_matches(&value, lower, upper) {
            continue;
        }
        out.extend(PostingList::decode(&bytes)?);
    }
    Ok(Some(out))
}

#[derive(Clone, Copy)]
pub struct AttrBound<'a> {
    pub value: &'a Value,
    pub inclusive: bool,
}

fn all_docs_key() -> Vec<u8> {
    vec![KEY_KIND_ALL_DOCS]
}

fn maybe_attr_key(column: &str, value: &Value) -> Result<Option<Vec<u8>>> {
    if !can_encode_scalar(value) {
        return Ok(None);
    }
    Ok(Some(attr_key(column, value)?))
}

fn attr_key(column: &str, value: &Value) -> Result<Vec<u8>> {
    let mut key = attr_column_prefix(column);
    encode_scalar_value(value, &mut key)?;
    Ok(key)
}

fn attr_column_prefix(column: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + 4 + column.len() + 16);
    key.push(KEY_KIND_ATTR);
    key.extend_from_slice(&(column.len() as u32).to_be_bytes());
    key.extend_from_slice(column.as_bytes());
    key
}

pub(crate) fn indexable_values(value: &Value) -> Result<Vec<&Value>> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_) => Ok(vec![value]),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                if can_encode_scalar(value) {
                    Ok(value)
                } else {
                    Err(Error::InvalidSchema(
                        "array attributes must contain indexable scalar values".into(),
                    ))
                }
            })
            .collect(),
    }
}

fn can_encode_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_)
    )
}

pub(crate) fn scalar_key(value: &Value) -> Result<Option<Vec<u8>>> {
    if !can_encode_scalar(value) {
        return Ok(None);
    }
    let mut key = Vec::new();
    encode_scalar_value(value, &mut key)?;
    Ok(Some(key))
}

fn encode_scalar_value(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Bool(v) => {
            out.push(VALUE_BOOL);
            out.push(u8::from(*v));
        }
        Value::Int(v) => {
            out.push(VALUE_INT);
            let ordered = (*v as u64) ^ (1u64 << 63);
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Float(v) => {
            if !v.is_finite() {
                return Err(Error::InvalidSchema(
                    "attribute index cannot encode non-finite float".into(),
                ));
            }
            out.push(VALUE_FLOAT);
            let bits = v.to_bits();
            let ordered = if bits & (1u64 << 63) != 0 {
                !bits
            } else {
                bits ^ (1u64 << 63)
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::String(v) => {
            out.push(VALUE_STRING);
            encode_ordered_string(v, out);
        }
        Value::Null | Value::Array(_) => {
            return Err(Error::InvalidSchema(
                "attribute index can only encode scalar values".into(),
            ));
        }
    }
    Ok(())
}

fn encode_ordered_string(value: &str, out: &mut Vec<u8>) {
    for b in value.as_bytes() {
        if *b == 0 {
            out.extend_from_slice(&[0, 0xff]);
        } else {
            out.push(*b);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

fn decode_value_from_key(bytes: &[u8]) -> Result<Option<Value>> {
    match bytes.first().copied() {
        Some(VALUE_BOOL) => Ok(bytes.get(1).map(|v| Value::Bool(*v != 0))),
        Some(VALUE_INT) => {
            let raw = bytes
                .get(1..9)
                .ok_or_else(|| Error::Corrupt("attribute int key truncated".into()))?;
            let ordered = u64::from_be_bytes(raw.try_into().expect("slice is a fixed-size window"));
            Ok(Some(Value::Int((ordered ^ (1u64 << 63)) as i64)))
        }
        Some(VALUE_FLOAT) => {
            let raw = bytes
                .get(1..9)
                .ok_or_else(|| Error::Corrupt("attribute float key truncated".into()))?;
            let ordered = u64::from_be_bytes(raw.try_into().expect("slice is a fixed-size window"));
            let bits = if ordered & (1u64 << 63) != 0 {
                ordered ^ (1u64 << 63)
            } else {
                !ordered
            };
            Ok(Some(Value::Float(f64::from_bits(bits))))
        }
        Some(VALUE_STRING) => {
            let value_bytes = bytes
                .get(1..)
                .ok_or_else(|| Error::Corrupt("attribute string key out of bounds".into()))?;
            Ok(Some(Value::String(decode_ordered_string(value_bytes)?)))
        }
        None => Ok(None),
        Some(_) => Err(Error::Corrupt("unknown attribute value tag".into())),
    }
}

fn decode_ordered_string(bytes: &[u8]) -> Result<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let b = *bytes
            .get(pos)
            .ok_or_else(|| Error::Corrupt("attribute string cursor out of bounds".into()))?;
        pos += 1;
        if b != 0 {
            out.push(b);
            continue;
        }
        let marker = *bytes
            .get(pos)
            .ok_or_else(|| Error::Corrupt("attribute string terminator truncated".into()))?;
        pos += 1;
        match marker {
            0 => {
                return String::from_utf8(out).map_err(|error| {
                    Error::Corrupt(format!("attribute string key not utf-8: {error}"))
                });
            }
            0xff => out.push(0),
            _ => return Err(Error::Corrupt("bad attribute string escape".into())),
        }
    }
    Err(Error::Corrupt("attribute string missing terminator".into()))
}

fn range_bound_matches(
    value: &Value,
    lower: Option<AttrBound<'_>>,
    upper: Option<AttrBound<'_>>,
) -> bool {
    use std::cmp::Ordering;
    if let Some(bound) = lower {
        let Some(ord) = compare_scalars(value, bound.value) else {
            return false;
        };
        if ord == Ordering::Less || (!bound.inclusive && ord == Ordering::Equal) {
            return false;
        }
    }
    if let Some(bound) = upper {
        let Some(ord) = compare_scalars(value, bound.value) else {
            return false;
        };
        if ord == Ordering::Greater || (!bound.inclusive && ord == Ordering::Equal) {
            return false;
        }
    }
    true
}
