mod common;

use sana::manifest::{ManifestPointer, NamespaceManifest};
use sana::schema::{ColumnSpec, ColumnType, DistanceMetric, ScalarType, Schema, VectorEncoding};
use sana::wal::WalCursor;

fn sample_manifest() -> NamespaceManifest {
    let mut schema = Schema {
        version: 1,
        ..Default::default()
    };
    schema.columns.insert(
        "embedding".into(),
        ColumnSpec {
            column_type: ColumnType::Vector {
                dim: 768,
                encoding: VectorEncoding::F32,
                metric: DistanceMetric::Cosine,
            },
            filterable: false,
            indexed: true,
        },
    );
    schema.columns.insert(
        "title".into(),
        ColumnSpec {
            column_type: ColumnType::Scalar(ScalarType::String),
            filterable: true,
            indexed: true,
        },
    );

    let mut m = NamespaceManifest::new("docs", 1_700_000_000_000);
    m.generation = 3;
    m.schema = schema;
    m.wal_commit_cursor = Some(WalCursor::new(0, 12));
    m.indexed_cursor = Some(WalCursor::new(0, 9));
    m.approx_row_count = 1000;
    m.approx_logical_bytes = 4_000_000;
    m.updated_at_ms = 1_700_000_500_000;
    m
}

#[test]
fn manifest_round_trips() {
    let m = sample_manifest();
    let encoded = m.encode().unwrap();
    let decoded = NamespaceManifest::decode(&encoded).unwrap();
    assert_eq!(m, decoded);
}

#[test]
fn manifest_rejects_unknown_format_version() {
    let m = sample_manifest();
    let mut value: serde_json::Value = serde_json::from_slice(&m.encode().unwrap()).unwrap();
    value["format_version"] = serde_json::json!(999);
    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(NamespaceManifest::decode(&bytes).is_err());
}

#[test]
fn pointer_round_trips() {
    let p = ManifestPointer::new(42);
    let decoded = ManifestPointer::decode(&p.encode().unwrap()).unwrap();
    assert_eq!(p, decoded);
}

#[test]
fn golden_manifest_json_is_stable() {
    let encoded = sample_manifest().encode().unwrap();
    common::assert_golden("manifest_v1.json", &encoded);
}

#[test]
fn golden_pointer_json_is_stable() {
    let encoded = ManifestPointer::new(42).encode().unwrap();
    common::assert_golden("manifest_pointer_v1.json", &encoded);
}
