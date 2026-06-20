#![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

use sana::doc::{DocRecord, decode_id, encode_id};
use sana::value::{Document, Id, Value};

#[test]
fn id_round_trips_all_variants() {
    let ids = [
        Id::U64(0),
        Id::U64(u64::MAX),
        Id::Uuid([7u8; 16]),
        Id::String("hello".into()),
        Id::String(String::new()),
    ];
    for id in ids {
        assert_eq!(decode_id(&encode_id(&id)).unwrap(), id);
    }
}

#[test]
fn encoded_order_matches_id_order() {
    // A deliberately shuffled set spanning all variants.
    let mut ids = [
        Id::String("apple".into()),
        Id::U64(5),
        Id::U64(100),
        Id::Uuid([0u8; 16]),
        Id::U64(2),
        Id::String("banana".into()),
        Id::Uuid([255u8; 16]),
    ];
    ids.sort();

    let mut encoded: Vec<Vec<u8>> = ids.iter().map(encode_id).collect();
    let by_id = encoded.clone();
    encoded.sort();

    // Sorting by Id and sorting by encoded bytes must agree.
    assert_eq!(by_id, encoded);
}

#[test]
fn record_round_trips() {
    let mut doc = Document::new(Id::U64(1));
    doc.attributes
        .insert("title".into(), Value::String("x".into()));
    let present = DocRecord::Present(doc);
    assert_eq!(
        DocRecord::decode(&present.encode().unwrap()).unwrap(),
        present
    );

    let deleted = DocRecord::Deleted;
    assert_eq!(
        DocRecord::decode(&deleted.encode().unwrap()).unwrap(),
        deleted
    );
}
