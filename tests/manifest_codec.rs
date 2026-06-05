mod common;

use sana::manifest::{
    ManifestPointer, NamespaceManifest, SstMeta, VectorAppendMeta, VectorIndexMeta,
    VectorMaintenanceAction, VectorMaintenancePlan, VectorMaintenanceTask,
    VectorMaintenanceThresholds,
};
use sana::schema::{ColumnSpec, ColumnType, DistanceMetric, ScalarType, Schema, VectorEncoding};
use sana::value::Id;
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
fn manifest_round_trips_doc_sst_metadata() {
    let mut m = sample_manifest();
    m.doc_ssts.push(SstMeta {
        key: "namespaces/docs/index/g/7/doc/flush-12.sst".into(),
        size_bytes: 4096,
        row_count: 2,
        min_id: Some(Id::U64(1)),
        max_id: Some(Id::U64(9)),
    });
    m.attr_ssts.push(SstMeta {
        key: "namespaces/docs/index/g/7/attr/full-12.sst".into(),
        size_bytes: 2048,
        row_count: 8,
        min_id: None,
        max_id: None,
    });
    m.vector_index_generations.insert("embedding".into(), 7);
    m.vector_indexes.insert(
        "embedding".into(),
        VectorIndexMeta {
            key: "namespaces/docs/index/g/7/vector/656d62656464696e67/ivf.bin".into(),
            size_bytes: 8192,
            version_map_key: Some(
                "namespaces/docs/index/g/7/vector/656d62656464696e67/versions.bin".into(),
            ),
            version_map_size_bytes: 1024,
            append_indexes: vec![VectorAppendMeta {
                key: "namespaces/docs/index/g/8/vector/656d62656464696e67/append-8.ivf.bin".into(),
                size_bytes: 512,
                row_count: 1,
                generation: 8,
            }],
            maintenance_plan: Some(VectorMaintenancePlan {
                thresholds: VectorMaintenanceThresholds {
                    min_posting_rows: 128,
                    max_posting_rows: 512,
                    reassign_neighborhood: 64,
                },
                tasks: vec![VectorMaintenanceTask {
                    action: VectorMaintenanceAction::Split,
                    cluster_id: 7,
                    partner_cluster_id: None,
                    neighbor_cluster_ids: vec![8, 9],
                    live_rows: 700,
                    stale_rows: 12,
                    append_rows: 140,
                    total_rows: 712,
                }],
            }),
            row_count: 2,
            centroid_count: 2,
            dim: 768,
            metric: DistanceMetric::Cosine,
        },
    );

    let decoded = NamespaceManifest::decode(&m.encode().unwrap()).unwrap();
    assert_eq!(decoded.doc_ssts, m.doc_ssts);
    assert_eq!(decoded.attr_ssts, m.attr_ssts);
    assert_eq!(decoded.vector_index_generations, m.vector_index_generations);
    assert_eq!(decoded.vector_indexes, m.vector_indexes);
}

#[test]
fn pointer_round_trips() {
    let p = ManifestPointer::new(42);
    let decoded = ManifestPointer::decode(&p.encode().unwrap()).unwrap();
    assert_eq!(p, decoded);
}

#[test]
fn pointer_round_trips_content_body_key() {
    let p = ManifestPointer::for_body(42, "namespaces/docs/manifest/g/42-deadbeef.json");
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
