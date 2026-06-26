//! One end-to-end tour of the library API: create a namespace, write
//! documents with attributes / a vector / text, index them, and run the four
//! query shapes — filtered, exact kNN, ANN, and BM25 — plus a hybrid
//! multi-query against one consistent snapshot.
//!
//!   cargo run --example usage
//!
//! Everything here also works over S3: build the store with
//! `S3ObjectStore::from_env(S3Config::from_location("s3://bucket")?)` instead
//! of `FsObjectStore`. The HTTP service (`sana serve`) exposes these same
//! calls as routes; see docs/guide.md.
#![allow(clippy::float_cmp, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use sana::indexer;
use sana::query::{Aggregate, ApproxVectorQuery, ExactVectorQuery, MultiQuery, TextQuery};
use sana::{Document, FilterExpr, FsObjectStore, Id, Namespace, ObjectStore, Query};

#[tokio::main]
async fn main() -> sana::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));

    // A namespace is the unit of isolation: its own WAL, manifest, indexes.
    let ns = Namespace::create(store, "library").await?;

    // Write a few books. Every write is durable in object storage when the
    // call returns; the schema (types per column) is inferred and enforced.
    let books: [(u64, &str, &str, f64, [f32; 2]); 4] = [
        (1, "The Left Hand of Darkness", "scifi", 4.7, [0.9, 0.1]),
        (2, "A Wizard of Earthsea", "fantasy", 4.5, [0.8, 0.3]),
        (3, "The Dispossessed", "scifi", 4.8, [0.95, 0.05]),
        (4, "Piranesi", "fantasy", 4.2, [0.2, 0.9]),
    ];
    for (id, title, genre, rating, embedding) in books {
        // `From` conversions and the chainable builders keep this terse; the
        // schema (types per column) is still inferred and enforced on write.
        ns.upsert(
            Document::new(id)
                .attr("title", title)
                .attr("genre", genre)
                .attr("rating", rating)
                .vector("embedding", embedding.to_vec()),
        )
        .await?;
    }

    // Fold the WAL into immutable SSTs and build the attribute, full-text,
    // and vector (IVF + RaBitQ) indexes. `sana serve` does this in the
    // background; a library embedder calls it directly.
    indexer::flush(&ns).await?;

    // 1. Filtered query with an aggregate: scifi books, count them.
    let result = ns
        .query(Query {
            filter: Some(FilterExpr::eq("genre", "scifi")),
            aggregates: vec![Aggregate::Count],
            ..Query::all()
        })
        .await?;
    println!(
        "scifi: {} rows, aggregates {:?}",
        result.rows.len(),
        result.aggregates
    );

    // 2. ANN vector search (IVF probe + RaBitQ-estimated L2 + exact rerank).
    let result = ns
        .query(Query {
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: vec![1.0, 0.0],
                k: 2,
                probes: None,
                metric: None,
            }),
            ..Query::all()
        })
        .await?;
    for row in &result.rows {
        println!("ann hit {:?} score {:?}", row.id, row.score);
    }

    // 3. Full-text search, BM25-ranked.
    let result = ns
        .query(Query {
            text: Some(TextQuery {
                column: "title".into(),
                query: "wizard darkness".into(),
                k: 3,
                params: Default::default(),
            }),
            ..Query::all()
        })
        .await?;
    for row in &result.rows {
        println!("text hit {:?} score {:?}", row.id, row.score);
    }

    // 4. Hybrid: one consistent snapshot, several rankings; fuse client-side.
    let result = ns
        .multi_query(MultiQuery {
            queries: vec![
                Query {
                    exact_vector: Some(ExactVectorQuery {
                        column: "embedding".into(),
                        vector: vec![1.0, 0.0],
                        k: 2,
                        metric: None,
                    }),
                    ..Query::all()
                },
                Query {
                    text: Some(TextQuery {
                        column: "title".into(),
                        query: "earthsea".into(),
                        k: 2,
                        params: Default::default(),
                    }),
                    ..Query::all()
                },
            ],
        })
        .await?;
    println!(
        "hybrid: {} vector hits, {} text hits",
        result.results[0].rows.len(),
        result.results[1].rows.len()
    );

    // Point lookup is strongly consistent (reads through the WAL overlay).
    let book = ns.lookup(&Id::U64(4)).await?.expect("book 4 exists");
    println!("lookup 4 -> {:?}", book.attributes["title"]);
    Ok(())
}
