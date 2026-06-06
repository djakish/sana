use std::collections::BTreeMap;
use std::sync::Arc;

use sana::error::Error;
use sana::manifest::ManifestPointer;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::schema::{ColumnSpec, ColumnType, DistanceMetric, ScalarType, VectorEncoding};
use sana::value::{Document, Id, Value, VectorValue};
use sana::wal::WalOp;
use tempfile::TempDir;

fn store(dir: &TempDir) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir.path()))
}

fn doc_with_schema(id: u64) -> Document {
    let mut doc = Document::new(Id::U64(id));
    doc.attributes
        .insert("title".into(), Value::String("alpha".into()));
    doc.attributes.insert("score".into(), Value::Int(10));
    doc.vectors
        .insert("embedding".into(), VectorValue::F32(vec![1.0, 2.0, 3.0]));
    doc
}

#[tokio::test]
async fn upsert_infers_schema_and_persists_manifest_pointer_body() {
    let dir = tempfile::tempdir().unwrap();
    let object_store = store(&dir);
    let ns = Namespace::create(object_store.clone(), "docs")
        .await
        .unwrap();

    ns.upsert(doc_with_schema(1)).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.schema.version, 1);
    assert_eq!(
        manifest.schema.columns["title"],
        ColumnSpec {
            column_type: ColumnType::Scalar(ScalarType::String),
            filterable: true,
            indexed: true,
        }
    );
    assert_eq!(
        manifest.schema.columns["score"],
        ColumnSpec {
            column_type: ColumnType::Scalar(ScalarType::Int),
            filterable: true,
            indexed: true,
        }
    );
    assert_eq!(
        manifest.schema.columns["embedding"],
        ColumnSpec {
            column_type: ColumnType::Vector {
                dim: 3,
                encoding: VectorEncoding::F32,
                metric: DistanceMetric::Cosine,
            },
            filterable: false,
            indexed: true,
        }
    );

    let ptr = object_store
        .get("namespaces/docs/manifest/current")
        .await
        .unwrap();
    let pointer = ManifestPointer::decode(&ptr.bytes).unwrap();
    assert_eq!(pointer.generation, manifest.generation);
    assert!(
        pointer
            .body_key
            .as_deref()
            .unwrap()
            .contains("/manifest/g/1-")
    );

    let reopened = Namespace::open(store(&dir), "docs").await.unwrap();
    assert_eq!(
        reopened.load_manifest().await.unwrap().schema,
        manifest.schema
    );
}

#[tokio::test]
async fn matching_writes_do_not_bump_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    ns.upsert(doc_with_schema(1)).await.unwrap();
    let v1 = ns.load_manifest().await.unwrap().schema.version;
    ns.upsert(doc_with_schema(2)).await.unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.schema.version, v1);
    assert_eq!(manifest.schema.columns.len(), 3);
}

#[tokio::test]
async fn type_mismatch_is_rejected_without_advancing_wal() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with_schema(1)).await.unwrap();
    let before = ns.commit_cursor().await.unwrap();

    let mut bad = Document::new(Id::U64(2));
    bad.attributes
        .insert("title".into(), Value::String("beta".into()));
    bad.attributes
        .insert("score".into(), Value::String("high".into()));

    let err = ns.upsert(bad).await.unwrap_err();
    assert!(matches!(err, Error::InvalidSchema(_)));
    assert_eq!(ns.commit_cursor().await.unwrap(), before);
    assert_eq!(ns.replay().await.unwrap().len(), 1);
}

#[tokio::test]
async fn ids_columns_and_vectors_enforce_schema_limits_before_wal_commit() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    for id in [Id::String(String::new()), Id::String("x".repeat(65))] {
        let error = ns.upsert(Document::new(id)).await.unwrap_err();
        assert!(matches!(error, Error::InvalidWrite(_)));
    }

    for name in ["$reserved".to_string(), "x".repeat(129)] {
        let mut document = Document::new(Id::U64(1));
        document.attributes.insert(name, Value::Int(1));
        let error = ns.upsert(document).await.unwrap_err();
        assert!(matches!(error, Error::InvalidSchema(_)));
    }

    let mut oversized = Document::new(Id::U64(1));
    oversized.vectors.insert(
        "embedding".into(),
        VectorValue::F32(vec![0.0; 10_753]),
    );
    let error = ns.upsert(oversized).await.unwrap_err();
    assert!(matches!(error, Error::InvalidSchema(_)));

    let mut too_many_vectors = Document::new(Id::U64(1));
    for name in ["a", "b", "c"] {
        too_many_vectors
            .vectors
            .insert(name.into(), VectorValue::F32(vec![0.0]));
    }
    let error = ns.upsert(too_many_vectors).await.unwrap_err();
    assert!(matches!(error, Error::InvalidSchema(_)));

    assert_eq!(ns.commit_cursor().await.unwrap().seq, 0);
    assert!(ns.load_manifest().await.unwrap().schema.columns.is_empty());
}

#[tokio::test]
async fn patch_infers_new_columns_but_null_only_clears() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with_schema(1)).await.unwrap();

    let mut attrs = BTreeMap::new();
    attrs.insert("tag".into(), Value::String("fresh".into()));
    attrs.insert("missing".into(), Value::Null);
    ns.append(
        vec![WalOp::Patch {
            id: Id::U64(1),
            attributes: attrs,
            vectors: BTreeMap::new(),
        }],
        None,
    )
    .await
    .unwrap();

    let manifest = ns.load_manifest().await.unwrap();
    assert_eq!(manifest.schema.version, 2);
    assert!(manifest.schema.columns.contains_key("tag"));
    assert!(!manifest.schema.columns.contains_key("missing"));

    let doc = ns.lookup(&Id::U64(1)).await.unwrap().unwrap();
    assert_eq!(doc.attributes["tag"], Value::String("fresh".into()));
    assert!(!doc.attributes.contains_key("missing"));
}

#[tokio::test]
async fn vector_dimension_mismatch_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();
    ns.upsert(doc_with_schema(1)).await.unwrap();
    let before = ns.commit_cursor().await.unwrap();

    let mut bad = Document::new(Id::U64(2));
    bad.attributes
        .insert("title".into(), Value::String("beta".into()));
    bad.attributes.insert("score".into(), Value::Int(20));
    bad.vectors
        .insert("embedding".into(), VectorValue::F32(vec![1.0, 2.0]));

    let err = ns.upsert(bad).await.unwrap_err();
    assert!(matches!(err, Error::InvalidSchema(_)));
    assert_eq!(ns.commit_cursor().await.unwrap(), before);
}

#[tokio::test]
async fn array_columns_are_homogeneous_but_can_later_be_empty() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    let mut first = Document::new(Id::U64(1));
    first.attributes.insert(
        "tags".into(),
        Value::Array(vec![Value::String("a".into()), Value::String("b".into())]),
    );
    ns.upsert(first).await.unwrap();

    let mut second = Document::new(Id::U64(2));
    second
        .attributes
        .insert("tags".into(), Value::Array(Vec::new()));
    ns.upsert(second).await.unwrap();

    let mut bad = Document::new(Id::U64(3));
    bad.attributes.insert(
        "tags".into(),
        Value::Array(vec![Value::String("a".into()), Value::Int(1)]),
    );
    assert!(matches!(
        ns.upsert(bad).await.unwrap_err(),
        Error::InvalidSchema(_)
    ));
}

#[test]
fn full_text_schema_accepts_strings_and_string_arrays() {
    let mut schema = sana::schema::Schema {
        columns: BTreeMap::from([(
            "body".to_string(),
            ColumnSpec {
                column_type: ColumnType::FullText,
                filterable: false,
                indexed: true,
            },
        )]),
        version: 1,
    };

    let mut ok = Document::new(Id::U64(1));
    ok.attributes
        .insert("body".into(), Value::String("rust database".into()));
    assert!(
        !schema
            .infer_and_validate_ops(&[WalOp::Upsert {
                id: Id::U64(1),
                document: ok,
            }])
            .unwrap()
    );

    let mut ok_array = Document::new(Id::U64(2));
    ok_array.attributes.insert(
        "body".into(),
        Value::Array(vec![
            Value::String("rust".into()),
            Value::String("search".into()),
        ]),
    );
    assert!(
        !schema
            .infer_and_validate_ops(&[WalOp::Upsert {
                id: Id::U64(2),
                document: ok_array,
            }])
            .unwrap()
    );

    let mut bad = Document::new(Id::U64(3));
    bad.attributes.insert("body".into(), Value::Int(10));
    assert!(matches!(
        schema.infer_and_validate_ops(&[WalOp::Upsert {
            id: Id::U64(3),
            document: bad,
        }]),
        Err(Error::InvalidSchema(_))
    ));
}

#[tokio::test]
async fn empty_batches_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ns = Namespace::create(store(&dir), "docs").await.unwrap();

    let err = ns.append(Vec::new(), None).await.unwrap_err();
    assert!(matches!(err, Error::InvalidWrite(_)));
    assert_eq!(ns.commit_cursor().await.unwrap().seq, 0);
    assert_eq!(ns.load_manifest().await.unwrap().generation, 0);
}
