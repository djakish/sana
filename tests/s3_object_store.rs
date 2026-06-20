//! Behavioral conformance for the S3 backend against a real S3-compatible
//! server, mirroring the filesystem-store contract tests.
//!
//! Gated on the environment so `cargo test` stays hermetic: every test is a
//! no-op unless `SANA_S3_TEST_ENDPOINT` is set. To run locally:
//!
//! ```sh
//! docker run -d --rm --name sana-minio -p 9000:9000 \
//!   -e MINIO_ROOT_USER=sana -e MINIO_ROOT_PASSWORD=sana-secret \
//!   minio/minio server /data
//! SANA_S3_TEST_ENDPOINT=http://127.0.0.1:9000 \
//!   AWS_ACCESS_KEY_ID=sana AWS_SECRET_ACCESS_KEY=sana-secret \
//!   cargo test --test s3_object_store
//! ```
#![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use rusty_s3::actions::CreateBucket;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use sana::error::Error;
use sana::namespace::Namespace;
use sana::object_store::{ObjectStore, S3Config, S3ObjectStore, version_of};
use sana::query::Query;
use sana::value::{Document, Id, Value};
use sana::{indexer, metrics};

const TEST_BUCKET: &str = "sana-conformance";

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Build a store rooted at a unique prefix per test, or `None` (skip) when no
/// test endpoint is configured.
async fn test_store(label: &str) -> Option<S3ObjectStore> {
    let endpoint = match std::env::var("SANA_S3_TEST_ENDPOINT") {
        Ok(endpoint) => endpoint,
        Err(_) => {
            eprintln!("skipping S3 conformance test: SANA_S3_TEST_ENDPOINT is not set");
            return None;
        }
    };
    let credentials = Credentials::from_env().expect("AWS credentials in the environment");
    ensure_bucket(&endpoint, &credentials).await;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let unique = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let config = S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: TEST_BUCKET.into(),
        key_prefix: format!("{label}-{nanos}-{unique}/"),
        path_style: true,
    };
    Some(S3ObjectStore::new(config, credentials).unwrap())
}

async fn ensure_bucket(endpoint: &str, credentials: &Credentials) {
    let bucket = Bucket::new(
        endpoint.parse().unwrap(),
        UrlStyle::Path,
        TEST_BUCKET,
        "us-east-1",
    )
    .unwrap();
    let action: CreateBucket = bucket.create_bucket(credentials);
    let url = action.sign(Duration::from_secs(60));
    let response = reqwest::Client::new().put(url).send().await.unwrap();
    // 200 created, 409 already owned: both leave the bucket usable.
    assert!(
        response.status().is_success() || response.status() == 409,
        "bucket setup failed: {}",
        response.status()
    );
}

#[tokio::test]
async fn put_get_delete_round_trip_with_etag_versions() {
    let Some(store) = test_store("roundtrip").await else {
        return;
    };
    let version = store
        .put("a/b", Bytes::from_static(b"hello"))
        .await
        .unwrap();
    let got = store.get("a/b").await.unwrap();
    assert_eq!(got.bytes, Bytes::from_static(b"hello"));
    assert_eq!(got.version, version);

    store.delete("a/b").await.unwrap();
    store.delete("a/b").await.unwrap(); // idempotent
    assert!(matches!(store.get("a/b").await, Err(Error::NotFound(_))));
    assert!(matches!(
        store.get("missing").await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn put_if_absent_enforces_existence() {
    let Some(store) = test_store("absent").await else {
        return;
    };
    store
        .put_if_absent("k", Bytes::from_static(b"first"))
        .await
        .unwrap();
    assert!(matches!(
        store
            .put_if_absent("k", Bytes::from_static(b"second"))
            .await,
        Err(Error::AlreadyExists(_))
    ));
    assert_eq!(store.get("k").await.unwrap().bytes, "first");
}

#[tokio::test]
async fn compare_and_set_is_enforced_by_the_server() {
    let Some(store) = test_store("cas").await else {
        return;
    };
    let v0 = store
        .put_if_absent("k", Bytes::from_static(b"v0"))
        .await
        .unwrap();
    let v1 = store
        .compare_and_set("k", v0.clone(), Bytes::from_static(b"v1"))
        .await
        .unwrap();
    assert_ne!(v0, v1);

    // Stale token loses; content-hash tokens from other backends lose too.
    assert!(matches!(
        store
            .compare_and_set("k", v0, Bytes::from_static(b"v2"))
            .await,
        Err(Error::CasMismatch { .. })
    ));
    assert!(matches!(
        store
            .compare_and_set("k", version_of(b"v1"), Bytes::from_static(b"v2"))
            .await,
        Err(Error::CasMismatch { .. })
    ));
    // CAS on a missing key is a mismatch, not a create.
    assert!(matches!(
        store
            .compare_and_set("missing", v1, Bytes::from_static(b"v2"))
            .await,
        Err(Error::CasMismatch { .. })
    ));
    assert_eq!(store.get("k").await.unwrap().bytes, "v1");
}

#[tokio::test]
#[allow(clippy::reversed_empty_ranges)] // start > end is the case under test
async fn ranged_gets_match_filesystem_semantics() {
    let Some(store) = test_store("range").await else {
        return;
    };
    store
        .put("k", Bytes::from_static(b"0123456789"))
        .await
        .unwrap();

    assert_eq!(store.get_range("k", 2..6).await.unwrap(), "2345");
    assert_eq!(store.get_range("k", 0..10).await.unwrap(), "0123456789");
    assert_eq!(store.get_range("k", 4..4).await.unwrap(), "");
    assert!(matches!(
        store.get_range("k", 8..12).await,
        Err(Error::InvalidRange { size: 10, .. })
    ));
    assert!(matches!(
        store.get_range("k", 6..2).await,
        Err(Error::InvalidRange { .. })
    ));
    assert!(matches!(
        store.get_range("missing", 0..1).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn list_returns_prefixed_keys_relative_to_the_root() {
    let Some(store) = test_store("list").await else {
        return;
    };
    for key in ["ns/a/1", "ns/a/2", "ns/b/1"] {
        store.put(key, Bytes::from_static(b"x")).await.unwrap();
    }
    let mut keys: Vec<String> = store
        .list("ns/a/")
        .await
        .unwrap()
        .into_iter()
        .map(|meta| meta.key)
        .collect();
    keys.sort();
    assert_eq!(keys, ["ns/a/1", "ns/a/2"]);
    let all = store.list("ns/").await.unwrap();
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|meta| meta.size == 1));
}

/// The real prize: the whole engine running against S3 — durable writes,
/// indexing, and an indexed query, with CAS enforced by the store.
#[tokio::test]
async fn namespace_lifecycle_runs_over_s3() {
    let Some(store) = test_store("engine").await else {
        return;
    };
    let store: Arc<dyn ObjectStore> = Arc::new(store);
    let registry = metrics::Metrics::shared();
    let ns = Namespace::create(store.clone(), "docs")
        .await
        .unwrap()
        .with_metrics(registry.clone());

    let mut doc = Document::new(Id::U64(1));
    doc.attributes
        .insert("title".into(), Value::String("hello s3".into()));
    ns.upsert(doc).await.unwrap();

    assert!(indexer::flush(&ns).await.unwrap());
    let result = ns.query(Query::all()).await.unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].id, Id::U64(1));
    assert_eq!(
        ns.lookup(&Id::U64(1)).await.unwrap().unwrap().attributes["title"],
        Value::String("hello s3".into())
    );
}
