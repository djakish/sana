//! Hybrid retrieval: one consistent snapshot, a vector ranking and a BM25
//! ranking, fused client-side with Reciprocal Rank Fusion (RRF). `multi_query`
//! is the engine half — it batches both reads against one captured manifest and
//! WAL snapshot — while fusion is the client half, because the executor
//! deliberately does not mix BM25 and vector score scales.
//!
//!   cargo run --example hybrid
#![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::Arc;

use sana::indexer;
use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::query::{ApproxVectorQuery, MultiQuery, Query, QueryResult, TextQuery};
use sana::value::{Document, Id, Value, VectorValue};

#[tokio::main]
async fn main() -> sana::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
    let ns = Namespace::create(store, "library").await?;

    // (title, embedding). The vector query leans toward the first dimension; the
    // text query "deep ocean" leans lexical. Different documents win each signal,
    // so fusion has something to do.
    let docs = [
        (1, "The Deep Range", [0.95, 0.05]),
        (2, "Ocean of Stars", [0.10, 0.95]),
        (3, "Deep Ocean Currents", [0.20, 0.90]),
        (4, "A Fire Upon the Deep", [0.90, 0.20]),
        (5, "Shallow Waters", [0.30, 0.70]),
    ];
    for (id, title, embedding) in docs {
        let mut doc = Document::new(Id::U64(id));
        doc.attributes
            .insert("title".into(), Value::String(title.into()));
        doc.vectors
            .insert("embedding".into(), VectorValue::F32(embedding.to_vec()));
        ns.upsert(doc).await?;
    }
    indexer::flush(&ns).await?;

    let result = ns
        .multi_query(MultiQuery {
            queries: vec![
                Query {
                    approx_vector: Some(ApproxVectorQuery {
                        column: "embedding".into(),
                        vector: vec![1.0, 0.0],
                        k: 5,
                        probes: None,
                        metric: None,
                    }),
                    ..Query::all()
                },
                Query {
                    text: Some(TextQuery {
                        column: "title".into(),
                        query: "deep ocean".into(),
                        k: 5,
                        params: Default::default(),
                    }),
                    ..Query::all()
                },
            ],
        })
        .await?;

    let vector_rank = &result.results[0];
    let text_rank = &result.results[1];
    print_ranking("vector (nearest [1,0])", vector_rank);
    print_ranking("text (\"deep ocean\")", text_rank);

    let fused = reciprocal_rank_fusion(&[vector_rank, text_rank], 60.0);
    println!("\nRRF fused:");
    for (rank, (id, score)) in fused.iter().enumerate() {
        println!("  {}. id {id:?}  rrf={score:.4}", rank + 1);
    }
    Ok(())
}

/// Reciprocal Rank Fusion: each list contributes `1 / (k + rank)` to a document
/// with one-based ranks (the canonical `k = 60`). It is scale-free, so it fuses
/// a BM25 ranking and a vector ranking without normalizing their incomparable
/// scores.
fn reciprocal_rank_fusion(rankings: &[&QueryResult], k: f64) -> Vec<(Id, f64)> {
    let mut scores: HashMap<Id, f64> = HashMap::new();
    for ranking in rankings {
        for (rank, row) in ranking.rows.iter().enumerate() {
            *scores.entry(row.id.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    let mut fused: Vec<(Id, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    fused
}

fn print_ranking(label: &str, result: &QueryResult) {
    println!("{label}:");
    for (rank, row) in result.rows.iter().enumerate() {
        let title = match row.document.attributes.get("title") {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        println!(
            "  {}. id {:?}  {title:?}  score={:?}",
            rank + 1,
            row.id,
            row.score
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sana::query::QueryRow;

    #[test]
    fn reciprocal_rank_fusion_uses_one_based_ranks() {
        let ranking = QueryResult {
            rows: vec![row(1), row(2)],
            aggregates: Vec::new(),
        };
        let fused = reciprocal_rank_fusion(&[&ranking], 60.0);

        assert_eq!(fused[0].0, Id::U64(1));
        assert_eq!(fused[1].0, Id::U64(2));
        assert!((fused[0].1 - (1.0 / 61.0)).abs() < f64::EPSILON);
        assert!((fused[1].1 - (1.0 / 62.0)).abs() < f64::EPSILON);
    }

    fn row(id: u64) -> QueryRow {
        QueryRow {
            id: Id::U64(id),
            document: Document::new(Id::U64(id)),
            score: None,
        }
    }
}
