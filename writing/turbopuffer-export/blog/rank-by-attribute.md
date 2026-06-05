# Mixing numeric attributes into text search for better first-stage relevance

April 27, 2026•Adrien Grand (Engineer)

turbopuffer now supports [using numeric and date attribute values in the ranking expression](https://turbopuffer.com/docs/query#rank-by-attribute) of text queries. Ranking by attribute uses the same fast, vectorized query engine that already powers text scoring, so it can significantly improve first-stage relevance while still scaling efficiently to large (100M+) corpora.

## Multi-stage search is an efficient approach for better relevance

Text search is often implemented with BM25 to score and rank documents by lexical relevance. Single-stage BM25 search scales well to many millions of documents and provides a strong relevance baseline.

However, many applications require a more nuanced evaluation of relevance than pure lexical scoring can provide. To achieve better relevance, modern search systems often use multi-stage search with reranking:

1.   In the **first stage**, BM25 efficiently scores a large corpus of documents to return a narrow list of relevant candidates.
2.   In the **second stage**, a reranker carefully examines the candidates in context, often using a cross-encoder or large language model, to sort the results by relevance.

### The two stages have very different cost profiles

BM25 with pruning algorithms (e.g. MAXSCORE) can [scale efficiently](https://turbopuffer.com/blog/bm25-latency-musings) over many documents by traversing a precomputed index. Reranking, by contrast, generally runs expensive inference on each query–document pair, resulting in orders of magnitude higher per-document cost.

The power of the multi-stage architecture is that it plays to each stage's strength: BM25 can quickly narrow a 100M-document corpus to a 100-candidate set, and the reranker can carefully reason over that set to produce the most relevant 10 results.

```
┏━━━━━━━━━━━━━━━━━━━┓ ──__
┃                   ┃     ‾‾──__
┃                   ┃           ‾‾──__
┃                   ┃                 ‾‾┏━━━━━━━━━━━━━━┓ ── __
┃                   ┃                   ┃              ┃       ‾‾┏━━━━━━━━━┓
┃      Corpus       ┃     1st stage     ┃  Candidates  ┃2nd stage┃ Results ┃
┃      (1M-1B)      ┃       BM25        ┃   (100-1K)   ┃ Rerank  ┃  (10)   ┃
┃                   ┃                   ┃              ┃       __┗━━━━━━━━━┛
┃                   ┃                 __┗━━━━━━━━━━━━━━┛ ── ‾‾
┃                   ┃           __──‾‾
┃                   ┃     __──‾‾
┗━━━━━━━━━━━━━━━━━━━┛ ──‾‾
```

```
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃           Corpus           ┃
┃          (1M-1B)           ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
 ╲                          ╱
   ╲         BM25         ╱
     ╲                  ╱
       ╲              ╱
        ┏━━━━━━━━━━━━┓
        ┃ Candidates ┃
        ┃  (100-1K)  ┃
        ┗━━━━━━━━━━━━┛
        |            |
         |  Rerank  |
          |        |
           |      |
           ┏━━━━━━┓
           ┃Result┃
           ┃ (10) ┃
           ┗━━━━━━┛
```

We walk the latency-recall tradeoff by varying the result count of the first stage. Returning more first-stage results can improve recall by giving the reranker more candidates to carefully judge, but it comes with a twofold cost: first-stage latency scales with result count, and second-stage latency scales with the number of documents evaluated.

Applying the classic "needle in a haystack" metaphor, BM25 efficiently grabs the most promising handfuls of hay, while reranking carefully inspects the handfuls for the needles. For good recall, we must ensure that the handfuls contain the needles. For low latency, we must avoid grabbing too many handfuls. When the two stages are well balanced, they can achieve both objectives.

## What happens when relevance depends on more than text?

Consider a basic BM25 query over a corporate email inbox, presumably to find recent status meeting notes:

```
{
  "rank_by": ["body", "BM25", "weekly status meeting"],
  "limit": 20
}
```

This returns the 20 emails whose bodies achieve the highest BM25 scores for the query, with results potentially spanning decades of emails. The searcher is likely looking for _this_ week's meeting, however, not one from years ago. The most relevant ranking expression, therefore, would evaluate not only the _text_ of the email, but also its _date_.

When text is the only significant determinant of relevance, BM25 serves as a good first-order approximation of the work the reranker will do later. When non-text attributes factor heavily into relevance, however, the approximation breaks down. **Relevance now depends on multiple variables, but BM25 evaluates only one of them**. The status email sent out yesterday might rank 500th by BM25 (far outside the candidate set) because emails with nested threads discussing past meetings have a stronger lexical match.

In multi-stage search, the reranker cannot rescue what the first stage never surfaces. To hit our recall target, we're forced to significantly overfetch at the expense of first and second-stage latency.

The influence of attribute values on search relevance is common across many use cases: web search is influenced by [PageRank](https://en.wikipedia.org/wiki/PageRank), e-commerce search is influenced by sales volume or product engagement, news and email search are influenced by recency, etc. To achieve good recall, search engines must consider these non-text attributes in their first-stage evaluation.

## Context: how BM25 scores text

BM25 scores each document by summing each query term's contribution, a function of its **term frequency** within that document weighted by its **inverse document frequency (IDF)** (a measure of how rare the term is across the corpus). The scoring function is nonlinear, saturating with increasing term frequency rather than growing without bound. Weighting by IDF ensures that rare, discriminating words contribute more than common ones. BM25 also normalizes by document length, so the same term frequency contributes more in shorter documents.

The chart below shows term score contributions for `weekly status meeting` over the [Enron email corpus](https://huggingface.co/datasets/Hellisotherpeople/enron_emails_parsed) (~536k emails). In this corpus, an average-length email containing 2 mentions each of `weekly`, `status`, and `meeting` would receive a total BM25 score of 12.90.

### BM25 score contribution by term frequency

Enron email corpus, 536k docs, k1=1.2, b=0.75, avg-length doc

1.   "weekly" (IDF=3.88)

2.   "status" (IDF=3.30)

3.   "meeting" (IDF=2.20)

## Scoring numeric and date attributes on the same scale as BM25

We cannot naively combine attribute values and BM25 scores in the first stage, as numeric attribute values and BM25 scores will have different ranges and distributions. An email received yesterday is likely not 7× more important than one received a week ago, so we cannot simply sum or multiply an attribute value with the BM25 score; we must manipulate it so that its magnitude and distribution reflect its contribution to relevance.

Here we turn to a [2005 paper](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/craswell_sigir05.pdf) co-authored by [Stephen Robertson](https://en.wikipedia.org/wiki/Stephen_Robertson_(computer_scientist)) — one of the two BM25 authors — that suggests incorporating attributes into the ranking expression in the same way BM25 incorporates the contribution of a new query term.

The paper provides a useful framework for determining whether a given attribute is useful for ranking, and, if so, what the shape of its function should be. You can read the paper if you're interested in the academic details, but here's the key finding: [**sigmoid functions**](https://en.wikipedia.org/wiki/Sigmoid_function) are most useful for converting attribute values into score contributions.

## Ranking by attribute in turbopuffer

Applying this work in turbopuffer, we expose a new [query syntax](https://turbopuffer.com/docs/query#rank-by-attribute) for ranking by attribute. The example below boosts the influence of recency in our email search query using a decaying function with an empirically tuned parameter (`midpoint`) and weight (`Product`):

```
{
  "rank_by": ["Sum", [
    ["body", "BM25", "weekly status meeting"],
    ["Product", 1.5,
      ["Decay",
        ["Dist",
          ["Attribute", "date"], new Date().toISOString(),
        ],
        { "midpoint": "30d" }
      ],
    ]
  ]],
}
```

`Decay` is a sigmoid function that produces a bounded output in [0, 1] over the email's recency, calculated as the `Dist` between the email's `date` and the current date. The steepness of the decay curve is set by the `midpoint` (30 days). The `Product` weight of 1.5 sets the maximum score.

### Decay function: 30d / (distance + 30d)

This mirrors what BM25 does with its term-frequency contributions. The sigmoid shape is _bounded_, so the score doesn't explode as the attribute value grows, and the empirically tuned weight (`Product`) is comparable in spirit to the IDF weights in BM25.

_Note: the turbopuffer API exposes `Saturate` as the counterpart to `Decay`; it produces the same bounded [0, 1] sigmoid shape, but rising with the attribute value instead of falling._

## How we make this scale to 100M+ document corpora

A beneficial consequence of treating attribute contributions as another scored clause in the ranking expression is that turbopuffer can run them through the same vectorized [MAXSCORE](https://turbopuffer.com/blog/fts-v2-maxscore) algorithm used by [our latest full-text search engine](https://turbopuffer.com/blog/fts-v2).

MAXSCORE maintains a running heap of the current candidate documents, using the minimum score in the heap to skip evaluating documents whose maximum score cannot exceed it. This skipping significantly prunes the number of documents evaluated without recall loss.

When the `rank_by` expression includes an attribute component, turbopuffer compiles it into an additional clause that sits alongside the BM25 term clauses in the MAXSCORE plan. For example, turbopuffer internally compiles the above query to the below formula:

```
score = 1.0 * bm25(body, "weekly")
      + 1.0 * bm25(body, "status")
      + 1.0 * bm25(body, "meeting")
      + 1.5 * attribute(date, f(x) = 30d / (x + 30d))
```

The query engine scans the attribute's index entries, iterates over documents that have a non-NULL value for that attribute column, and computes a score for each one. Because the scoring function is applied at query time, not precomputed in the index, you can iteratively tune its shape without reindexing. Within each evaluation window, the partial scores from BM25 and attribute clauses are summed into one score per document. These combined scores are used to set the heap minimum and evaluate which documents can be skipped.

We can observe the impact of these clauses on the combined score of a document in the interface below. Select an example email, or edit the body or date directly, to see how changing text and recency influence the score.

With the attribute clause included in the MAXSCORE evaluation alongside the BM25 clauses, the running heap reflects the full `rank_by` objective, not a BM25-only surrogate we must later repair with the reranker. We no longer need to compensate for BM25's inability to evaluate non-text relevance signals, and we can present a compact, relevant candidate list to the reranker to achieve good recall without overfetching.

While adding an attribute clause does increase latency — for the same reason that adding more terms to a query increases latency — MAXSCORE's aggressive pruning ensures the cost scales efficiently to very large corpus sizes.

## Conclusion

Ranking by attribute brings non-text signals into first-stage retrieval without sacrificing the efficiency that makes the first-stage scalable. By compiling attribute contributions into the same MAXSCORE plan as BM25 term clauses, the query engine fuses text and attribute scores in a single pass and prunes aggressively, so first-stage scoring tracks a combined relevance objective while maintaining the scalability characteristics of BM25.

Most datasets have one or two key attributes that strongly influence relevance, and pulling them into the first stage lets you boost their signal at scale. The exact recall and latency tradeoff will vary with corpus, query, and result count. As with [tuning BM25](https://turbopuffer.com/docs/fts#advanced-tuning), we recommend an empirical approach to measure the impact of attribute ranking on your specific workload.
