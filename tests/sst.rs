mod common;

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use sana::object_store::{FsObjectStore, GetResult, ObjectMeta, ObjectStore, ObjectVersion};
use sana::sst::{SstReader, SstWriter, ranged_get};

/// Many keys with shared prefixes and a tiny block target, to force multiple
/// blocks and exercise prefix compression + the block index.
fn sample_pairs() -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs = Vec::new();
    for i in 0..200u32 {
        let key = format!("user/{i:05}").into_bytes();
        let value = format!("value-{i}").into_bytes();
        pairs.push((key, value));
    }
    pairs
}

fn build(pairs: &[(Vec<u8>, Vec<u8>)], block_target: usize) -> Vec<u8> {
    let mut w = SstWriter::with_params(block_target, 8);
    for (k, v) in pairs {
        w.add(k, v).unwrap();
    }
    w.finish()
}

#[test]
fn point_get_hits_and_misses() {
    let pairs = sample_pairs();
    let reader = SstReader::open(Bytes::from(build(&pairs, 64))).unwrap();

    for (k, v) in &pairs {
        assert_eq!(reader.get(k).unwrap().as_deref(), Some(v.as_slice()));
    }
    assert_eq!(reader.get(b"user/99999").unwrap(), None); // past the end
    assert_eq!(reader.get(b"aaa").unwrap(), None); // before the start
    assert_eq!(reader.get(b"user/00000x").unwrap(), None); // between keys
}

#[test]
fn entries_are_sorted_and_complete() {
    let pairs = sample_pairs();
    let reader = SstReader::open(Bytes::from(build(&pairs, 64))).unwrap();
    let got: Vec<(Vec<u8>, Vec<u8>)> = reader
        .entries()
        .unwrap()
        .into_iter()
        .map(|(k, v)| (k, v.to_vec()))
        .collect();
    assert_eq!(got, pairs);
}

#[test]
fn single_block_round_trips() {
    let pairs = sample_pairs();
    let reader = SstReader::open(Bytes::from(build(&pairs, 1 << 20))).unwrap();
    assert_eq!(reader.entries().unwrap().len(), pairs.len());
    assert_eq!(
        reader.get(b"user/00100").unwrap().as_deref(),
        Some(&b"value-100"[..])
    );
}

#[test]
fn empty_sst_is_valid() {
    let reader = SstReader::open(Bytes::from(SstWriter::new().finish())).unwrap();
    assert!(reader.entries().unwrap().is_empty());
    assert_eq!(reader.get(b"anything").unwrap(), None);
}

#[test]
fn rejects_unsorted_keys() {
    let mut w = SstWriter::new();
    w.add(b"b", b"1").unwrap();
    assert!(w.add(b"a", b"2").is_err());
    assert!(w.add(b"b", b"3").is_err()); // equal is also rejected
}

#[test]
fn detects_corruption() {
    let mut bytes = build(&sample_pairs(), 64);
    bytes[10] ^= 0xff; // flip a byte inside the first data block
    let reader = SstReader::open(Bytes::from(bytes)).unwrap();
    // Corruption in a data block surfaces when that block is read.
    let hit_error = reader.get(b"user/00000").is_err() || reader.entries().is_err();
    assert!(hit_error);
}

#[test]
fn rejects_bad_magic() {
    let mut bytes = build(&sample_pairs(), 64);
    let n = bytes.len();
    bytes[n - 1] ^= 0xff; // corrupt the trailing magic
    assert!(SstReader::open(Bytes::from(bytes)).is_err());
}

/// Test-only `ObjectStore` decorator that counts read calls and bytes returned,
/// so a test can assert that a point lookup transfers far less than the object.
struct ByteCountingStore {
    inner: Arc<dyn ObjectStore>,
    reads: AtomicUsize,
    bytes: AtomicU64,
}

impl ByteCountingStore {
    fn new(inner: Arc<dyn ObjectStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            reads: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
        })
    }

    fn reset(&self) {
        self.reads.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl ObjectStore for ByteCountingStore {
    async fn get(&self, key: &str) -> sana::Result<GetResult> {
        let r = self.inner.get(key).await?;
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(r.bytes.len() as u64, Ordering::Relaxed);
        Ok(r)
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> sana::Result<Bytes> {
        let b = self.inner.get_range(key, range).await?;
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(b.len() as u64, Ordering::Relaxed);
        Ok(b)
    }

    async fn put(&self, key: &str, bytes: Bytes) -> sana::Result<ObjectVersion> {
        self.inner.put(key, bytes).await
    }

    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> sana::Result<ObjectVersion> {
        self.inner.put_if_absent(key, bytes).await
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> sana::Result<ObjectVersion> {
        self.inner.compare_and_set(key, expected, bytes).await
    }

    async fn list(&self, prefix: &str) -> sana::Result<Vec<ObjectMeta>> {
        self.inner.list(prefix).await
    }

    async fn delete(&self, key: &str) -> sana::Result<()> {
        self.inner.delete(key).await
    }
}

/// `ranged_get` must agree with the whole-object reader on every hit and miss,
/// while reading only a small fraction of a multi-block object.
#[tokio::test]
async fn ranged_get_matches_reader_and_reads_few_bytes() {
    // Fat values so the data region dominates and the index stays small.
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..300u32)
        .map(|i| (format!("doc/{i:06}").into_bytes(), vec![b'x'; 256]))
        .collect();
    let sst = build(&pairs, 4096); // default-ish target => many blocks
    let size = sst.len() as u64;

    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
    fs.put("doc.sst", Bytes::from(sst.clone())).await.unwrap();
    let store = ByteCountingStore::new(fs);
    let whole = SstReader::open(Bytes::from(sst)).unwrap();

    // Every present key resolves to the same value as the whole-object reader.
    for (k, v) in &pairs {
        let got = ranged_get(store.as_ref(), "doc.sst", size, k)
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(v.as_slice()));
        assert_eq!(got.as_deref(), whole.get(k).unwrap().as_deref());
    }
    // Misses before, after, and between keys all return None.
    for miss in [&b"aaa"[..], &b"doc/999999"[..], &b"doc/000000x"[..]] {
        assert_eq!(
            ranged_get(store.as_ref(), "doc.sst", size, miss)
                .await
                .unwrap(),
            whole.get(miss).unwrap(),
        );
    }

    // A single point lookup reads only footer + index + one block: at most three
    // small requests and a small fraction of the object.
    store.reset();
    let hit = ranged_get(store.as_ref(), "doc.sst", size, b"doc/000150")
        .await
        .unwrap();
    assert_eq!(hit.as_deref(), Some(&[b'x'; 256][..]));
    assert!(
        store.reads.load(Ordering::Relaxed) <= 3,
        "expected <=3 reads, got {}",
        store.reads.load(Ordering::Relaxed)
    );
    let read = store.bytes.load(Ordering::Relaxed);
    assert!(
        read < size / 4,
        "point lookup read {read} of {size} bytes (expected < a quarter)"
    );
}

/// A corrupt `size` (footer not where claimed) must error, not panic.
#[tokio::test]
async fn ranged_get_rejects_bad_size() {
    let sst = build(&sample_pairs(), 64);
    let size = sst.len() as u64;
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
    fs.put("doc.sst", Bytes::from(sst)).await.unwrap();

    // Claiming a too-large size makes the footer range run past EOF.
    assert!(
        ranged_get(fs.as_ref(), "doc.sst", size + 64, b"user/00000")
            .await
            .is_err()
    );
    // A size below the footer length is rejected up front.
    assert!(
        ranged_get(fs.as_ref(), "doc.sst", 8, b"user/00000")
            .await
            .is_err()
    );
}

#[test]
fn golden_format_is_stable() {
    // Fixed params + fixed data => stable bytes.
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..40u32)
        .map(|i| {
            (
                format!("k{i:04}").into_bytes(),
                format!("v{i}").into_bytes(),
            )
        })
        .collect();
    common::assert_golden("sst_v1.bin", &build(&pairs, 48));
}
