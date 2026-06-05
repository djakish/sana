mod common;

use bytes::Bytes;
use sana::sst::{SstReader, SstWriter};

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
    assert_eq!(reader.get(b"user/00100").unwrap().as_deref(), Some(&b"value-100"[..]));
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

#[test]
fn golden_format_is_stable() {
    // Fixed params + fixed data => stable bytes.
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = (0..40u32)
        .map(|i| (format!("k{i:04}").into_bytes(), format!("v{i}").into_bytes()))
        .collect();
    common::assert_golden("sst_v1.bin", &build(&pairs, 48));
}
