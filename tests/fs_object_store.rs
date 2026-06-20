#![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

use bytes::Bytes;
use sana::error::Error;
use sana::object_store::{FsObjectStore, ObjectStore, version_of};
use std::sync::Arc;
use tempfile::tempdir;

fn store() -> (tempfile::TempDir, FsObjectStore) {
    let dir = tempdir().unwrap();
    let store = FsObjectStore::new(dir.path());
    (dir, store)
}

#[tokio::test]
async fn put_then_get_round_trips_with_version() {
    let (_dir, store) = store();
    let key = "namespaces/docs/manifest/current";
    let body = Bytes::from_static(b"hello world");

    let put_version = store.put(key, body.clone()).await.unwrap();
    let got = store.get(key).await.unwrap();

    assert_eq!(got.bytes, body);
    assert_eq!(got.version, put_version);
    assert_eq!(got.version, version_of(&body));
}

#[tokio::test]
async fn get_missing_is_not_found() {
    let (_dir, store) = store();
    let err = store.get("nope").await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
}

#[tokio::test]
async fn put_if_absent_rejects_existing() {
    let (_dir, store) = store();
    let key = "a/b/c";
    store
        .put_if_absent(key, Bytes::from_static(b"1"))
        .await
        .unwrap();
    let err = store
        .put_if_absent(key, Bytes::from_static(b"2"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)));
    // original content is untouched
    assert_eq!(
        store.get(key).await.unwrap().bytes,
        Bytes::from_static(b"1")
    );
}

#[tokio::test]
async fn cas_succeeds_on_matching_version_and_advances() {
    let (_dir, store) = store();
    let key = "ptr";
    let v0 = store.put(key, Bytes::from_static(b"v0")).await.unwrap();

    let v1 = store
        .compare_and_set(key, v0.clone(), Bytes::from_static(b"v1"))
        .await
        .unwrap();
    assert_ne!(v0, v1);
    assert_eq!(
        store.get(key).await.unwrap().bytes,
        Bytes::from_static(b"v1")
    );
}

#[tokio::test]
async fn cas_fails_on_stale_version() {
    let (_dir, store) = store();
    let key = "ptr";
    let v0 = store.put(key, Bytes::from_static(b"v0")).await.unwrap();
    store
        .compare_and_set(key, v0.clone(), Bytes::from_static(b"v1"))
        .await
        .unwrap();

    // Reusing the stale v0 must fail and leave content unchanged.
    let err = store
        .compare_and_set(key, v0, Bytes::from_static(b"v2"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::CasMismatch { .. }));
    assert_eq!(
        store.get(key).await.unwrap().bytes,
        Bytes::from_static(b"v1")
    );
}

#[tokio::test]
async fn independent_handles_share_one_cas_lock() {
    let dir = tempdir().unwrap();
    let initial = FsObjectStore::new(dir.path());
    let expected = initial.put("ptr", Bytes::from_static(b"v0")).await.unwrap();

    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for value in 0..16u8 {
        let store = FsObjectStore::new(dir.path());
        let expected = expected.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .compare_and_set("ptr", expected, Bytes::from(vec![value]))
                .await
        }));
    }

    let mut successes = 0;
    let mut mismatches = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => successes += 1,
            Err(Error::CasMismatch { .. }) => mismatches += 1,
            Err(error) => panic!("unexpected CAS result: {error}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(mismatches, 15);
}

#[tokio::test]
async fn cas_on_absent_key_reports_no_actual() {
    let (_dir, store) = store();
    let err = store
        .compare_and_set("missing", version_of(b"whatever"), Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    match err {
        Error::CasMismatch { actual, .. } => assert!(actual.is_none()),
        other => panic!("expected CasMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn get_range_reads_subslice() {
    let (_dir, store) = store();
    let key = "blob";
    store
        .put(key, Bytes::from_static(b"0123456789"))
        .await
        .unwrap();

    assert_eq!(
        store.get_range(key, 2..5).await.unwrap(),
        Bytes::from_static(b"234")
    );
    assert_eq!(
        store.get_range(key, 0..10).await.unwrap(),
        Bytes::from_static(b"0123456789")
    );
    let err = store.get_range(key, 8..12).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRange { .. }));
}

#[tokio::test]
async fn list_returns_sorted_prefixed_keys() {
    let (_dir, store) = store();
    store.put("ns/a/1", Bytes::from_static(b"x")).await.unwrap();
    store
        .put("ns/a/2", Bytes::from_static(b"yy"))
        .await
        .unwrap();
    store.put("ns/b/1", Bytes::from_static(b"z")).await.unwrap();

    let listed = store.list("ns/a/").await.unwrap();
    let keys: Vec<_> = listed.iter().map(|m| m.key.as_str()).collect();
    assert_eq!(keys, vec!["ns/a/1", "ns/a/2"]);
    assert_eq!(listed[0].size, 1);
    assert_eq!(listed[1].size, 2);
}

#[tokio::test]
async fn delete_is_idempotent() {
    let (_dir, store) = store();
    let key = "gone";
    store.put(key, Bytes::from_static(b"x")).await.unwrap();
    store.delete(key).await.unwrap();
    store.delete(key).await.unwrap(); // second delete is a no-op
    assert!(matches!(
        store.get(key).await.unwrap_err(),
        Error::NotFound(_)
    ));
}

#[tokio::test]
async fn rejects_path_traversal_keys() {
    let (_dir, store) = store();
    let err = store
        .put("../escape", Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)));
}
