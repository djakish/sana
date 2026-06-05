mod common;

use std::collections::BTreeMap;

use sana::value::{Document, Id, Value, VectorValue};
use sana::wal::{WAL_MAGIC, WalBatch, WalOp};

fn sample_batch() -> WalBatch {
    let mut doc = Document::new(Id::U64(7));
    doc.vectors
        .insert("embedding".into(), VectorValue::F32(vec![0.1, 0.2, 0.3]));
    doc.attributes
        .insert("title".into(), Value::String("hello".into()));
    doc.attributes.insert("score".into(), Value::Int(42));

    let mut patch_attrs = BTreeMap::new();
    patch_attrs.insert("score".into(), Value::Int(43));

    WalBatch {
        namespace: "docs".into(),
        sequence: 1,
        created_at_ms: 1_700_000_000_000,
        idempotency_key: Some("idem-1".into()),
        operations: vec![
            WalOp::Upsert {
                id: Id::U64(7),
                document: doc,
            },
            WalOp::Patch {
                id: Id::String("abc".into()),
                attributes: patch_attrs,
                vectors: BTreeMap::new(),
            },
            WalOp::Delete { id: Id::U64(9) },
        ],
    }
}

#[test]
fn round_trips() {
    let batch = sample_batch();
    let encoded = batch.encode().unwrap();
    let decoded = WalBatch::decode(&encoded).unwrap();
    assert_eq!(batch, decoded);
}

#[test]
fn envelope_has_magic() {
    let encoded = sample_batch().encode().unwrap();
    assert_eq!(&encoded[0..8], WAL_MAGIC);
}

#[test]
fn rejects_bad_magic() {
    let mut encoded = sample_batch().encode().unwrap();
    encoded[0] = b'X';
    assert!(WalBatch::decode(&encoded).is_err());
}

#[test]
fn detects_body_corruption_via_crc() {
    let mut encoded = sample_batch().encode().unwrap();
    let last = encoded.len() - 1;
    encoded[last] ^= 0xff;
    assert!(WalBatch::decode(&encoded).is_err());
}

#[test]
fn rejects_truncated_frame() {
    let encoded = sample_batch().encode().unwrap();
    assert!(WalBatch::decode(&encoded[..encoded.len() - 1]).is_err());
    assert!(WalBatch::decode(&encoded[..4]).is_err());
}

#[test]
fn golden_format_is_stable() {
    let encoded = sample_batch().encode().unwrap();
    common::assert_golden("wal_batch_v1.bin", &encoded);
}
