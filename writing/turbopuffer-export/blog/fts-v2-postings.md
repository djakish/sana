# Designing inverted indexes in a KV-store on object storage

January 14, 2026•Morgan Gallant (Engineer), Adrien Grand (Engineer)

turbopuffer's [FTS v2](https://turbopuffer.com/blog/fts-v2) brings up to 20x faster full-text search performance through a combination of an [updated query implementation](https://turbopuffer.com/blog/fts-v2-maxscore) and a new inverted index structure.

Our new inverted indexes are 10x smaller than their equivalent index in the previous format and are optimized for efficient, vectorized batch processing at query time.

This post focuses on the new index structure implementation, and our plans to use it for all inverted indexes on turbopuffer going forward.

## Background

turbopuffer builds an inverted index for all attributes that are enabled for [filtering](https://turbopuffer.com/docs/query#filtering) or [full-text search](https://turbopuffer.com/docs/fts). Fundamentally, an inverted index maps a set of unique strings to the set of documents that contain them, called a "posting list".

```
value                posting list
  ┌────────┐           ┌───┬───┬───┬───┬───┐
  │ adrien ├──────────▶│ 1 │ 2 │ 5 │ 7 │ 9 │ // "adrien" appears in docs 1, 2, 5, 7, 9
  └────────┘           └───┴───┴───┴───┴───┘
  ┌────────┐           ┌───┬───┬───┬───┐
  │ morgan ├──────────▶│ 3 │ 4 │ 5 │ 9 │
  └────────┘           └───┴───┴───┴───┘
  ┌────────┐           ┌───┬───┬───┬───┬───┬───┬───┐
  │ nathan ├──────────▶│ 1 │ 2 │ 3 │ 4 │ 6 │ 7 │ 8 │
  └────────┘           └───┴───┴───┴───┴───┴───┴───┘
  ┌────────┐           ┌───┬───┐
  │ simon  ├──────────▶│ 2 │ 9 │
  └────────┘           └───┴───┘
  ┌────────┐           ┌───┬───┬───┐
  │ nikhil ├──────────▶│ 4 │ 6 │ 9 │
  └────────┘           └───┴───┴───┘
  ┌────────┐           ┌───┬───┬───┬───┬───┬───┬───┬───┐
  │ puffy  ├──────────▶│ 1 │ 2 │ 3 │ 4 │ 5 │ 7 │ 8 │ 9 │
  └────────┘           └───┴───┴───┴───┴───┴───┴───┴───┘
```

```
value      posting list
┌────────┐ ┌───┬───┬───┬───┬───┐
│ adrien ├▶│ 1 │ 2 │ 3 │ 4 │ 5 │
└────────┘ └───┴───┴───┴───┴───┘
┌────────┐ ┌───┬───┬───┐
│ morgan ├▶│ 2 │ 4 │ 5 │
└────────┘ └───┴───┴───┘
┌────────┐ ┌───┬───┬───┬───┐
│ nathan ├▶│ 1 │ 4 │ 6 │ 7 │
└────────┘ └───┴───┴───┴───┘
┌────────┐ ┌───┐
│ simon  ├▶│ 2 │
└────────┘ └───┘
┌────────┐ ┌───┬───┬───┐
│ nikhil ├▶│ 4 │ 6 │ 9 │
└────────┘ └───┴───┴───┘
┌────────┐ ┌───┬───┬───┬───┬───┐
│ puffy  ├▶│ 1 │ 2 │ 3 │ 4 │ 5 │
└────────┘ └───┴───┴───┴───┴───┘
```

In its simplest form, each value's posting list contains the IDs of documents in which that value appears, along with an optional weight (used for BM25 ranking in full-text search), sorted in ascending order by document ID.

In code (Rust), this can be represented simply as:

```
type InvertedIndex = HashMap<Term, PostingList>;
type PostingList = Vec<(DocId, Weight)>; // sorted by DocId
```

During query execution, turbopuffer retrieves the posting lists for each relevant term and combines them to get the final set of matching documents.

For example, to evaluate a filter like `["author", "In", ["adrien", "morgan"]]`, turbopuffer unions the posting lists for `"adrien"` and `"morgan"` to get the full set of documents that match either value:

```
value                posting list
  ┌────────┐           ┌───┬───┐       ┌───┬───┬───┐
  │ adrien ├──────────▶│ 1 │ 2 │       │ 5 │ 7 │ 9 │
  └────────┘           └───┴───┘       └───┴───┴───┘
  ┌────────┐                   ┌───┬───┬───┐   ┌───┐
  │ morgan ├──────────▶        │ 3 │ 4 │ 5 │   │ 9 │
  └────────┘                   └───┴───┴───┘   └───┘
  ---------------------------------------------------
  ┌─────────────────┐  ┌───┬───┬───┬───┬───┬───┬───┐
  │ [adrien,morgan] ├─▶│ 1 │ 2 │ 3 │ 4 │ 5 │ 7 │ 9 │
  └─────────────────┘  └───┴───┴───┴───┴───┴───┴───┘
```

```
value      posting list
┌────────┐ ┌───┬───┬───┬───┬───┐
│ adrien ├▶│ 1 │ 2 │ 3 │ 4 │ 5 │
└────────┘ └───┴───┴───┴───┴───┘
┌────────┐     ┌───┐   ┌───┬───┐
│ morgan ├▶    │ 2 │   │ 4 │ 5 │
└────────┘     └───┘   └───┴───┘
--------------------------------
┌────────┐ ┌───┬───┬───┬───┬───┐
│ union  ├▶│ 1 │ 2 │ 3 │ 4 │ 5 │
└────────┘ └───┴───┴───┴───┴───┘
```

Full-text search queries like `["rank_by", "BM25", "adrien morgan"]` are executed similarly. turbopuffer [tokenizes](https://turbopuffer.com/docs/fts#tokenizers) the query string into individual terms (e.g. "adrien" and "morgan"), and retrieves their posting lists. Then, using a [vectorized MAXSCORE algorithm](https://turbopuffer.com/blog/fts-v2-maxscore), turbopuffer efficiently computes the final set of matching documents and ranks them by relevance based on the weights stored in the posting lists.

## Designing a keyspace for inverted indexes on object storage

turbopuffer's storage engine is built on an [LSM-tree](https://en.wikipedia.org/wiki/Log-structured_merge-tree) that persists data to [object storage](https://turbopuffer.com/docs/architecture). Like all LSM-based systems, data is organized as key-value pairs that get periodically compacted together. When designing any data structure on top of an LSM, the critical question is: _how do we map our logical data model to physical KV pairs?_

For inverted indexes, we need to decide how to represent posting lists in our keyspace. There are a few ways we could approach this.

Consider, first, the simplest approach: store each posting list as a single KV pair, keyed by term. Queries would be fast: just one key lookup per term. But this approach breaks down at scale. On sufficiently large datasets, the posting list for a single term could contain millions, perhaps billions, of entries. Every time a document is added or deleted, we'd need to re-serialize and re-upload that entire posting list, even though only a single entry changed. This leads to severe write amplification and makes compaction prohibitively expensive.

At the other extreme, we could store each individual posting as its own KV pair, keyed by `(attribute_value, doc_id)`. Updates become trivial: just write one small KV pair per posting. But this creates different problems. Each KV pair must store its full key, and for millions of postings, all those repeated `attribute_value` prefixes add up quickly. More critically, compaction still becomes prohibitively expensive as our LSM must read, merge-sort, and rewrite every single entry.

The solution is to find a middle ground: **turbopuffer partitions each posting list into blocks**, where each block becomes a single KV pair. This approach amortizes the per-KV overhead across many postings while keeping blocks small enough that updates don't require rewriting massive amounts of data. Our LSM's compaction process naturally groups adjacent blocks into the same physical objects (files in S3), so queries can fetch multiple blocks in a single read operation. However, the boundaries of these blocks must be chosen carefully.

## FTS v1: partition by vector cluster boundary

It is our habit at turbopuffer to first pursue the simplest possible design to meet the requirement, only adding complexity once we've earned the right to do so. turbopuffer was once purely a vector database with a [cluster-based index derived from SPFresh](https://turbopuffer.com/docs/architecture), so our initial inverted index design simply aligned posting list partition boundaries with the documents' existing vector cluster boundaries:

```
tpuf FTS v1 cluster-based partition design
  -------------------------------------------

                         x                              x
                                      x              x  x
                        x  x                        x  x  x         x
                        x   x       x    x        x  x x x      x   x x
  Vector clusters: ◎───────────◎ ◎───────────◎ ◎───────────◎ ◎───────────◎ •••

                   ┌───────────┐ ┌───────────┐ ┌───────────┐ ┌───────────┐
       Partitions: │ 5 postings│ │ 3 postings│ │10 postings│ │4 postings │ •••
                   └───────────┘ └───────────┘ └───────────┘ └───────────┘
```

```
tpuf FTS v1 cluster-based
partition design
-------------------------

     x           x   x
   x  x         x x  x
   x   x          x x x
◎──────────◎  ◎──────────◎
┌──────────┐  ┌──────────┐
│5 postings│  │9 postings│
└──────────┘  └──────────┘

      x
                    x x
  x     x       x   x
◎──────────◎  ◎──────────◎
┌──────────┐  ┌──────────┐
│3 postings│  │4 postings│
└──────────┘  └──────────┘
           •••
```

In code, this could be represented simply as:

```
// A map, sorted by (term, cluster id)
type InvertedIndex = BTreeMap<(Term, ClusterId), PostingListPartition>;
```

This design was simple to implement and provided sub-50ms latency for most queries. Not a bad v1. However, as our customers continued to push namespace size limits (100M+ documents per namespace), we began to observe disappointing full-text search latency, especially for longer full-text search queries containing terms with few postings per cluster.

Here's why: In any natural language corpus, term frequency generally follows a [Zipfian distribution](https://en.wikipedia.org/wiki/Zipf%27s_law); the frequency of a term is inversely proportional to its frequency rank. The most common term will occur 2x more often than the second most common term, 3x more often than the third common term, and so on. In any reasonably sized inverted index, a small number of very common terms will appear in a large number of documents, while the vast majority of terms will each appear in only a few documents.

We confirmed this empirically in our v1 implementation on a dataset of 40 million [MSMARCO documents](https://huggingface.co/datasets/Cohere/msmarco-v2-embed-multilingual-v3). **We found the median per-cluster posting list partition contained just ~1.5 postings, and the p90 partition contained ~11 postings**.

While not a correctness problem, this gives us exactly the issue described in the latter extreme above: too many tiny KV pairs, each carrying metadata overhead that dwarfs the actual posting data, leading to poor compression, bloated indexes, and slow compaction.

## FTS v2: partition by fixed-size blocks

To address poor compression efficiency and storage amplification from metadata overhead, we redesigned our inverted index structure to use larger, fixed-size blocks. Rather than align partition boundaries with vector clusters (which led to tiny blocks), turbopuffer's FTS v2 inverted index now maintains posting list blocks at a fixed target size of ~256 postings each, independent of how documents are clustered in our vector index. Larger blocks allow us to better optimize for modern CPUs, which excel at processing large, contiguous blocks of data sequentially. This approach dramatically improves compression ratios and ensures that metadata overhead is amortized across many postings.

```
tpuf FTS v2 block-based partition design
  -----------------------------------------

                         x                          x   x
                       x  x             x           x x  x          x
                        x   x       x    x        x  x x x      x   x x
     Cluster-based: ◎───────────◎ ◎───────────◎ ◎───────────◎ ◎───────────◎ •••

                   ┌──────────────────────────────────┐ ┌─────────────────
      Block-based: │ ~256 postings                    │ │                  •••
                   └──────────────────────────────────┘ └─────────────────
```

```
tpuf FTS v2 block-based
partition design
-----------------------

Cluster-based:

     x        x   x              x x
   x  x      x x  x      x
   x   x      x x x   x    x      x x x
◎───────◎  ◎───────◎ ◎───────◎ ◎───────◎

Block-based:

┌──────────────────────┐ ┌──────────────
│     ~256 postings    │ │ ~256 postings •••
└──────────────────────┘ └──────────────
```

Under the hood, we use [Quickwit's bitpacking crate](https://github.com/quickwit-oss/bitpacking); a Rust port of [Daniel Lemire's simdcomp C library](https://github.com/lemire/simdcomp) to both compress and decode posting lists efficiently. In our benchmarks, we achieved single core throughput of ~741 million postings decoded per second. Assuming 6 bytes per posting, that's ~4.5GB/s, which is several times faster than a general-purpose decompression algorithm like Zstd (~1GB/s).

### Why N=256?

We intentionally chose a target block size of 256 postings to balance several competing concerns.

**Compression efficiency.** Our underlying bitpacking compression operates on frames of 128 postings. We use continuous split and merge operations as documents are upserted to maintain our target block size of 256: when a block grows beyond 512 postings, we split it in half; when it shrinks below 128 postings, we merge it with an adjacent block. This guarantees every block contains between 128 and 512 postings, meaning every block has at least one full, efficiently-compressed 128-posting frame.

**Query performance.** Block size also interacts with our [vectorized MAXSCORE](https://turbopuffer.com/blog/fts-v2-maxscore) algorithm. MAXSCORE skips blocks whose maximum score can't affect the final top-k results. Larger blocks allow bigger skips when they _do_ get skipped, but they also drive up the block's maximum score, making skips less likely to occur. A target of 256 postings strikes a balance; blocks are large enough that skipping them saves meaningful work, but small enough that block-max scores stay tight and skips remain frequent.

**Storage overhead.** Finally, 256 postings per block amortizes the per-KV metadata overhead across enough postings that it becomes negligible, while keeping blocks small enough that queries rarely overfetch.

## Results

In terms of physical index size, **turbopuffer's new inverted index structure is up to 10x smaller than the equivalent index in the previous format**, and our index sizes are now on par with mature open-source text search engines like [Apache Lucene](https://lucene.apache.org/) and [Tantivy](https://github.com/quickwit-oss/tantivy), validating that our block-based partitioning strategy achieves highly efficient compression.

### Up to 10x smaller physical index size

We benchmarked the physical size of the inverted index data for the 40M-document MSMARCO dataset mentioned above between FTS v1 (cluster-based partitions) and FTS v2 (fixed-size blocks), with the following results:

```
changes in inverted index size, FTS v1 vs FTS v2, 40M documents

  v1 ║░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ 51.6 GiB
  v2 ║▓▓▓▓▓ 5.22 GiB
             ▲
             └──── 9.9x reduction in index size
```

```
changes in index size,
FTS v1 vs FTS v2,
40M documents

  51.6 GiB
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░
  ░░░░░░░░    ┌─── 9.9x smaller
  ░░░░░░░░    ▼
  ░░░░░░░░ 5.22 GiB
  ░░░░░░░░ ▓▓▓▓▓▓▓▓
  ░░░░░░░░ ▓▓▓▓▓▓▓▓
═════════════════════
     v1       v2
```

### Less frequent terms see better storage gains

We also ran benchmarks against a [Common Crawl](https://commoncrawl.org/) dataset (150M, 1.22 TiB of web documents, ~352M unique terms) comparing the term-level inverted index sizes for our v1 cluster-based posting list partitions versus our v2 fixed-size partitions:

For the most common term ("©"), total index size decreases by 2.5x:

| measure | cluster-based partitions | block-based partitions | delta |
| --- | --- | --- | --- |
| total index size | 833.1 MiB | 340.0 MiB | -2.5x |
| avg postings per partition | 57 | 316 | +5.5x |

This is the _best case scenario_ for cluster-based partitions, as the most common term will most densely populate the cluster-based partitions, yet block-based are _still_ 2.5x smaller.

For the 10,000th most common term ("裏"), total index size decreases by over 6x:

| measure | cluster-based partitions | block-based partitions | delta |
| --- | --- | --- | --- |
| total index size | 17.82 MiB | 2.868 MiB | -6.2x |
| avg postings per partition | 1.3 | 303 | +233x |

The gap in physical index size between v1 and v2 only widens as terms become less common. This is because less common terms have fewer postings per cluster in v1, leading to smaller partitions and proportionally more KV metadata overhead. In contrast, our v2 block-based approach maintains a fixed number of postings per block regardless of term frequency, so the metadata overhead remains constant and amortized across many postings.

### In sum: up to 20x faster full-text search queries

Our new block-based postings index structure results in up to 10x smaller inverted indexes, with less frequent terms seeing the largest reductions in inverted index size.

Combined with our new [vectorized MAXSCORE](https://turbopuffer.com/blog/fts-v2-maxscore) algorithm, fixed-size posting blocks deliver up to 20x faster full-text search queries:

```
turbopuffer FTS latency (k=100, dataset=English Wikipedia export @ ~5M documents)

         san francisco  v1 ║░░░ 8ms
                        v2 ║▓ 3ms
                           ║
                           ║
               the who  v1 ║░░░░░░░░░░░░░░░░░ 57ms
                        v2 ║▓▓ 7ms
                           ║
                           ║
         united states  v1 ║░░░░░░ 20ms
          constitution  v2 ║▓▓ 5ms
                           ║
                           ║
     lord of the rings  v1 ║░░░░░░░░░░░░░░░░░░░░░░░ 75ms
                        v2 ║▓▓ 6ms
                           ║
                           ║
 pop singer songwriter  v1 ║░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ 174ms
    born 1989 won best  v2 ║▓▓▓▓▓▓ 20ms
     country song time     ║
    person of the year     ║
```

```
turbopuffer FTS latency (ms)

 k=100,
 English Wikipedia export
 ~5M documents

                         174ms
  ░░ v1                  ░░
                         ░░
  ▓▓ v2                  ░░
                         ░░
                         ░░
                         ░░
                         ░░
                         ░░
                         ░░
                         ░░
                         ░░
                         ░░
                         ░░
                         ░░
                   75ms  ░░
                   ░░    ░░
                   ░░    ░░
       57ms        ░░    ░░
       ░░          ░░    ░░
       ░░          ░░    ░░
       ░░          ░░    ░░
       ░░    20ms  ░░    ░░20ms
       ░░    ░░    ░░    ░░▓▓
 8ms   ░░7ms ░░    ░░6ms ░░▓▓
 ░░3ms ░░▓▓  ░░5ms ░░▓▓  ░░▓▓
 ░░▓▓  ░░▓▓  ░░▓▓  ░░▓▓  ░░▓▓
 ══════════════════════════════
 q1    q2    q3    q4    q5
--------------------------------
 q1 = san francisco

 q2 = the who

 q3 = united states
      constitution

 q4 = lord of the rings

 q5 = pop singer songwriter
      born 1989 won best
      country song time
      person of the year
```

## Soon: Faster infix search, search-as-you-type, and filtering

turbopuffer's new inverted index structure has already been rolled out for full-text search indexes across all turbopuffer namespaces as part of the [FTS v2 launch](https://turbopuffer.com/blog/fts-v2). These same improvements will be coming to infix search (e.g. `%puf%`), search-as-you-type, and filtering queries in the near future, all benefiting from the same compression and query performance optimizations.

Fundamentally, the only difference between a full-text search posting list (for BM25) and posting lists for these other use cases is that the latter don't need to store any weights (since they don't do ranking). In our implementation, a posting list is generic over its weight, meaning we can simply substitute in a zero-sized type and re-use all the same code paths. This design efficiency will make it possible to quickly roll out the new inverted index structure across multiple query types.

## Conclusion

FTS v2's new inverted index structure is what earned complexity looks like: we first built something simple, observed its performance in production, then optimized based on the pressure exerted by our customers' workloads.

Unlike the v1 design, where posting list partitions simply aligned with existing cluster boundaries, we now actively maintain fixed-size posting list blocks through split and merge operations. Added complexity, yes, but the performance gains are well worth it.

If you're pushing full-text search or filtered vector search queries to millions or billions of documents, there's never been a better time to start puffin'.

* * *
