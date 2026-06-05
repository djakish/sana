# Vectorized MAXSCORE over WAND, especially for long LLM-generated queries

Updated: January 14, 2026•Adrien Grand (Engineer), Morgan Gallant (Engineer)

[FTS v2](https://turbopuffer.com/blog/fts-v2), the newest version of turbopuffer's homegrown text search engine, is up to 20x faster than v1 thanks to (a) an [improved storage layout](https://turbopuffer.com/blog/fts-v2-postings) and (b) a better search algorithm. This post is about the better search algorithm.

turbopuffer is often queried by agents, who tend to craft longer queries than humans. A key characteristic of the FTS v2 search algorithm is that it performs very well on such long queries (tens of terms). In particular, it is up to several times faster than block-max WAND, the most popular algorithm for lexical search.

Below are some representative benchmarks ([full results](https://turbopuffer.com/blog/fts-v2-maxscore#appendix-turbopuffer-fts-v2-benchmark-results)) of FTS v1 versus FTS v2 on a 5M-document Wikipedia export sample dataset borrowed from [Quickwit's Search Benchmark Game](https://github.com/quickwit-oss/search-benchmark-game/blob/master/Makefile). The results support the claim that FTS v2 works extremely well not only on simple queries, but also on harder queries with many terms or those with only stopwords.

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

## Search algorithms: MAXSCORE vs WAND

Consider the query "new york" with a top-k of 10. It has two terms: "new" and "york", each contributing a max score of 2.0 and 5.0, respectively. "york" has a higher score because the word is less common than "new". The query score is the sum of the per-term scores, higher score is better.

We can deduce that any document may get scores up to the following values depending on what terms it contains:

| Document | Max possible score |
| --- | --- |
| only "new" | 2.0 |
| only "york" | 5.0 |
| "new" and "york" | 7.0 |

During evaluation, we may find ourselves in a scenario where the 10th best-scoring document so far has a score of 4.0. As k=10, any document containing only "new" can never enter the results! At this point, we can skip all documents that only contain "new". This simple optimization can lead to dramatic speedups.

There are two main algorithms that take advantage of this idea to speed up query evaluation without trading recall: **MAXSCORE** and **WAND**.

Let's look at how they evaluate a query on terms "new", "york", and "population" which respectively contribute up to 2.0, 5.0, and 4.0 to the score (top_k=3 for brevity).

### MAXSCORE: term-centric

The first family of search algorithms is [MAXSCORE](https://www.sciencedirect.com/science/article/abs/pii/030645739500020H). It starts by sorting the query terms by their score contribution: `[new(2.0), population(4.0), york(5.0)]`.

Then, it starts iterating through the documents that match each term, keeping track of the highest scoring documents in the top-k heap.

Let’s add a proverbial breakpoint a few iterations in:

```
----------------------------------------------------------
| Term         | Max score | Postings (matching doc IDs) |
|--------------|-----------|-----------------------------|
| new          |       2.0 | 0, 1, 3, 5, 6, 7, 9, 10     |
|              |           |          ^                  |
| population   |       4.0 | 1, 2, 3, 8, 9               |
|              |           |          ^                  |
| york         |       5.0 | 2, 3, 7, 10                 |
|              |           |       ^                     |
----------------------------------------------------------

# (score, doc_id) -- bigger score is better
TOPK3_HEAP = [(6.0, 1), (9.0, 2), (11.0, 3)]
```

At this point, the minimum score in the heap is 6.0, so documents with “new” alone can no longer qualify, and “new” has become a non-essential term. We can simply ignore it when looking for the next document to evaluate, speeding up query performance.

In a few iterations, the minimum score will be 7.0, at which point “population” _also_ becomes non-essential, speeding up the remainder of the query execution even further!

**MAXSCORE: use essential terms to find candidates, use all terms to compute scores**

### WAND: document-centric

The second family of skipping algorithms is [WAND](https://dl.acm.org/doi/abs/10.1145/956863.956944) ("weak and"). We consider it document-centric, because instead of continuously determining which terms no longer qualify, it attempts to find the next doc ID that could potentially qualify for the top-k heap.

Let's breakpoint at the same moment as with MAXSCORE:

```
----------------------------------------------------------
| Term         | Max score | Postings (matching doc IDs) |
|--------------|-----------|-----------------------------|
| new          |       2.0 | 0, 1, 3, 5, 6, 7, 9, 10     |
|              |           |          ^                  |
| population   |       4.0 | 1, 2, 3, 8, 9               |
|              |           |          ^                  |
| york         |       5.0 | 2, 3, 7, 10                 |
|              |           |       ^                     |
----------------------------------------------------------

# (score, doc_id) -- bigger score is better
TOPK3_HEAP = [(6.0, 1), (9.0, 2), (11.0, 3)]
```

WAND sorts doc IDs in ascending order and calculates the score upper bound for each ID. A term contributes to the document score if its iterator has passed and confirmed the doc ID is present, or hasn't yet reached the doc ID (WAND must assume it is in the posting list).

For example:

```
# (doc ID, score upper bound)
[
  (5, 2.0),   # matches "new", will not match "york" or "population"
  (7, 7.0),   # matches "york", might match "new"
  (8, 11.0)   # matches "population", might match "new" and "york"
]
```

Then, WAND finds the first document whose best possible score exceeds the minimum heap score (6.0). Here, that's doc ID 7, so WAND evaluates it next. If the minimum heap score were greater than 7.0, WAND would skip directly to 8.

**WAND: use all terms, continuously skipping doc IDs that cannot possibly qualify.**

### Block-max MAXSCORE/WAND

In their basic forms, these algorithms use global max scores: each term has a single max score (like 2.0 for "new") that applies to its entire posting list containing all doc IDs. With a global max score, we must evaluate each document individually to determine if it can qualify for the heap. But what if we could skip entire groups of documents at once?

This is what the block-max variants of MAXSCORE and WAND achieve. Block-max divides each posting list into fixed-size blocks containing a subset of doc IDs, storing a local max score for each block (the maximum score any document in that block can get from that term). Documents within a block can have different scores because term frequency (TF) varies per document, even though inverse document frequency (IDF) is constant for the term. If a block's local max score doesn't exceed the minimum heap score, the algorithm can skip the entire block of documents. Block-max MAXSCORE and block-max WAND are currently considered the state of the art for lexical search.

There are really no disadvantages to block-max MAXSCORE/WAND versus MAXSCORE/WAND. For brevity, I'll be using "MAXSCORE" and "WAND" to describe the algorithms for the remainder of this post, but know I'm referring to their block-max counterparts.

### Comparison

WAND is currently the dominant algorithm for query evaluation, as it takes more information into account (the current positions of the postings list iterators) and can thus skip more documents than MAXSCORE.

However, skipping more documents doesn't necessarily mean faster. In order to skip more documents, WAND must spend more cycles computing the next doc ID that can qualify for the top-k heap. This introduces more work per evaluated document, giving WAND a lower throughput than MAXSCORE. When WAND's skipping power is outweighed by low throughput, MAXSCORE becomes more performant.

The [Apache Lucene](https://lucene.apache.org/) project initially started with WAND. While it worked well for most queries, users reported certain query types where it was slower than even _exhaustive_ evaluation due to poor skipping decisions compounded by poor throughput. This led Lucene to switch to MAXSCORE and optimize it for throughput by improving memory locality, reducing unpredictable branches, and taking advantage of SIMD instructions.

| Algorithm | Skipping | Evaluation throughput |
| --- | --- | --- |
| Exhaustive | None | Very good |
| WAND | Very good | Average |
| MAXSCORE | Good | Good |
| Lucene's vectorized MAXSCORE | Good | Very good |

Lucene's vectorized MAXSCORE throughput is so good that it's also extremely competitive with WAND on simple queries where WAND is expected to shine, as confirmed by the [Tantivy vs. Lucene benchmarks](https://tantivy-search.github.io/bench/) (Tantivy implements WAND). And because throughput is very good, it doesn't become slower than exhaustive search in cases when skipping is hard.

This good throughput is especially important on long queries (tens of terms) where skipping is harder, especially for WAND-based algorithms whose overhead scales with the number of terms. Our evaluations confirmed those made by Apache Lucene; this algorithm often performs several times faster than WAND on long queries.

Side note: the WAND vs. MAXSCORE trade-off has similarities with HNSW vs. [SPFresh](https://turbopuffer.com/docs/architecture), the system turbopuffer uses for ANN search. WAND and HNSW optimize for skipping first, and throughput second, whereas Lucene's MAXSCORE and SPFresh optimize for throughput first, and skipping second.

## turbopuffer's MAXSCORE variant

While building on the same skipping logic as MAXSCORE, we borrow from Lucene's vectorized implementation, differing significantly from the textbook description in order to improve throughput.

The main novelty is that postings iterators are no longer advanced in an alternating fashion. In traditional MAXSCORE implementations, iterators advance alternately: if iterator A is at doc ID 5 and iterator B is at doc ID 3, the algorithm advances B to catch up, perhaps advancing B beyond A, causing A to then catch up, and so on.

Our implementation works differently. After computing the next range of doc IDs to evaluate based on local max scores, it processes all matching doc IDs and scores needed from each postings iterator in one batch before moving to the next iterator. This means the same iterator advances many times in a row (often tens of times or more) before switching to another iterator.

This batching approach achieves specific CPU-level throughput optimizations:

*   **Better memory locality**: Accessing consecutive memory locations from the same iterator enables the CPU's cache prefetcher to predict and load upcoming data before it's needed, achieving higher cache hit rates.
*   **Branch prediction**: When the same iterator advances many times in a row, the CPU's branch predictor achieves higher accuracy by learning a consistent pattern, keeping the [instruction pipeline](https://en.wikipedia.org/wiki/Instruction_pipelining) full more often.
*   **SIMD vectorization**: Modern CPUs take advantage of [SIMD instructions](https://en.wikipedia.org/wiki/Single_instruction,_multiple_data) to process multiple data elements in parallel, but they require sequential data. Processing many consecutive elements from the same iterator enables SIMD optimizations that wouldn't be possible with frequently alternating iterators.

## Conclusion

Designing fast search engines is a subtle optimization problem. On the one hand, CPUs reach peak efficiency on serial workloads with predictable branches. On the other hand, the smartest algorithms often have a random access pattern and lots of conditionals. So it takes some effort to select the right algorithm and then tune it to get the best out of the CPU.

Plus, context keeps changing. [AVX-512](https://en.wikipedia.org/wiki/AVX-512) has now been widely available on server-side CPUs for several years, further moving the cursor in the direction of serial, branchless workloads. "Serial and dumb" can often beat "smart and random" on modern CPUs. Furthermore, agents write longer queries than humans, so it's becoming increasingly important for text search to scale well with the number of terms.

These factors force us to periodically revisit our choices of algorithms and their implementations. For text search, this means that the cursor has shifted more and more from WAND to MAXSCORE, which scales better with the number of terms and can be tuned to be more CPU-friendly.

Thanks for reading through this blog. I hope you enjoyed the deep dive, as well as the performance that you're now getting on text search with turbopuffer thanks to FTS v2.

* * *
