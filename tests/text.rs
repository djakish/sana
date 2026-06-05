use std::collections::BTreeMap;

use bytes::Bytes;
use sana::sst::SstReader;
use sana::text::{self, Bm25Params, TokenizerConfig};
use sana::value::{Document, Id, Value};

fn text_doc(id: u64, body: &str) -> Document {
    let mut doc = Document::new(Id::U64(id));
    doc.attributes
        .insert("body".into(), Value::String(body.to_string()));
    doc
}

#[test]
fn tokenizer_lowercases_and_splits_on_non_words() {
    assert_eq!(
        text::tokenize("Rust, BM25-search v2!", TokenizerConfig::default()),
        vec!["rust", "bm25", "search", "v2"]
    );
}

#[test]
fn analysis_counts_terms_deterministically() {
    let stats = text::analyze_text("Rust rust database", TokenizerConfig::default());
    assert_eq!(stats.doc_len, 3);
    assert_eq!(stats.terms[0].term, "database");
    assert_eq!(stats.terms[0].frequency, 1);
    assert_eq!(stats.terms[1].term, "rust");
    assert_eq!(stats.terms[1].frequency, 2);
}

#[test]
fn bm25_rewards_higher_tf_and_shorter_documents() {
    let params = Bm25Params::default();
    let one_hit = text::bm25_term_score(1, 4, 4.0, 10, 2, params);
    let two_hits = text::bm25_term_score(2, 4, 4.0, 10, 2, params);
    let long_doc = text::bm25_term_score(1, 12, 4.0, 10, 2, params);

    assert!(two_hits > one_hit);
    assert!(one_hit > long_doc);
}

#[test]
fn text_sst_round_trips_bm25_postings() {
    let docs = BTreeMap::from([
        (Id::U64(1), text_doc(1, "rust rust database")),
        (Id::U64(2), text_doc(2, "rust database storage engine")),
        (Id::U64(3), text_doc(3, "python database")),
    ]);
    let built = text::build_text_sst(&docs).unwrap().unwrap();
    let reader = SstReader::open(Bytes::from(built.bytes)).unwrap();

    let hits = text::search_sst(&reader, "body", "rust database", Bm25Params::default()).unwrap();
    let ids: Vec<Id> = hits.into_iter().map(|hit| hit.id).collect();

    assert_eq!(ids, vec![Id::U64(1), Id::U64(2), Id::U64(3)]);
}

#[test]
fn text_sst_splits_large_terms_into_fixed_blocks() {
    let docs: BTreeMap<Id, Document> = (0..300u64)
        .map(|id| {
            (
                Id::U64(id),
                text_doc(id, &format!("rust database shard {id}")),
            )
        })
        .collect();
    let built = text::build_text_sst(&docs).unwrap().unwrap();
    let reader = SstReader::open(Bytes::from(built.bytes)).unwrap();

    let stats = text::term_stats(&reader, "body", "rust").unwrap().unwrap();
    assert_eq!(stats.doc_freq, 300);
    assert_eq!(stats.block_count, 2);
    assert!(stats.max_score > 0.0);

    let hits = text::search_sst(&reader, "body", "rust", Bm25Params::default()).unwrap();
    assert_eq!(hits.len(), 300);
}

#[test]
fn maxscore_top_k_matches_exhaustive_bm25() {
    let docs = BTreeMap::from([
        (Id::U64(1), text_doc(1, "rust rust database")),
        (Id::U64(2), text_doc(2, "rust database storage engine")),
        (Id::U64(3), text_doc(3, "database internals")),
        (Id::U64(4), text_doc(4, "python storage")),
        (Id::U64(5), text_doc(5, "rust")),
    ]);
    let built = text::build_text_sst(&docs).unwrap().unwrap();
    let reader = SstReader::open(Bytes::from(built.bytes)).unwrap();

    let exhaustive = text::search_sst(&reader, "body", "rust database", Bm25Params::default())
        .unwrap()
        .into_iter()
        .take(3)
        .map(|hit| hit.id)
        .collect::<Vec<_>>();
    let maxscore =
        text::search_sst_top_k(&reader, "body", "rust database", 3, Bm25Params::default())
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>();

    assert_eq!(maxscore, exhaustive);
}

#[test]
fn maxscore_skips_blocks_that_cannot_enter_top_k() {
    let docs: BTreeMap<Id, Document> = (0..300u64)
        .map(|id| {
            let body = if id < 3 {
                "common ".repeat(20)
            } else {
                "common".to_string()
            };
            (Id::U64(id), text_doc(id, body.trim()))
        })
        .collect();
    let built = text::build_text_sst(&docs).unwrap().unwrap();
    let reader = SstReader::open(Bytes::from(built.bytes)).unwrap();

    let outcome =
        text::search_sst_top_k_with_stats(&reader, "body", "common", 3, Bm25Params::default())
            .unwrap();
    let ids = outcome
        .hits
        .iter()
        .map(|hit| hit.id.clone())
        .collect::<Vec<_>>();

    assert_eq!(ids, vec![Id::U64(0), Id::U64(1), Id::U64(2)]);
    assert_eq!(outcome.stats.blocks_read, 1);
    assert_eq!(outcome.stats.blocks_skipped, 1);
    assert_eq!(outcome.stats.score_batches, 4);
    assert_eq!(outcome.stats.postings_scored, 256);
}
