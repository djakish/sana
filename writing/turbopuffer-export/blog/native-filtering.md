# Native filtering for high-recall vector search

January 21, 2025•Bojan Serafimov (Engineer)

Vector indexes are often evaluated on [recall](https://en.wikipedia.org/wiki/Precision_and_recall) (correctness) vs latency benchmarks without any filtering, however, most production search queries have a WHERE condition. This significantly increases the difficulty of achieving high recall. In this post we'll explore what makes filtered vector search queries difficult and how we solve this problem. Rather than pre-filtering or post-filtering, turbopuffer uses native filtering to achieve performant high recall queries.

## Intro: Vector search without filters

The goal of a vector index is to solve the Approximate Nearest Neighbors problem ([ANN](https://en.wikipedia.org/wiki/Nearest_neighbor_search#Approximate_nearest_neighbor)).

There are many kinds of vector indexes. The two most popular kinds are graph-based and clustering-based indexes.

A graph-based index avoids exhaustively searching all the vectors in the database by building a neighborhood graph among the vectors and greedily searching a path in that graph.

A clustering-based index groups nearby points together and speeds up queries by only considering clusters whose center is closest to the input vector.

```
|---------------------------|---------------------------|
|                           |                           |
|    x---------x            |    0         1            |
|    |          \           |                           |
|    x---x-------x--x       |    0   0       1  1       |
|     \ /        |   \      |                           |
|      x         x----x     |      0         1    1     |
|     / \       /           |                           |
|    x   \     /            |    0                      |
|         \   /             |                           |
|          \ /              |                           |
|           x---x           |           2   2           |
|           |               |                           |
|           x               |           2               |
|                           |                           |
|                           |                           |
|    Graph-based index      |  Clustering-based index   |
|---------------------------|---------------------------|
```

```
|--------------|--------------|
|              |              |
|  x-------x   |  0       1   |
|  |        \  |              |
|  x--x-----x  |  0   0   1 1 |
|   \/ |     \ |              |
|   x  x-----x |    0     1 1 |
|  / \  /      |              |
| x   \/       |  0           |
|      x---x   |       2   2  |
|      |       |              |
|      x       |       2      |
|              |              |
| Graph-based  | Clustering   |
|    index     |   index      |
|--------------|--------------|
```

In turbopuffer we use a clustering-based index inspired by [SPFresh](https://dl.acm.org/doi/10.1145/3600006.3613166). The main advantage of this index is that it can be incrementally updated. As documents are inserted, overwritten and deleted, the size of each cluster changes, but the SPFresh algorithm dynamically splits and merges clusters to maintain balance. Because the index is self-balancing, it doesn't need to be periodically rebuilt, which allows us to scale to very large namespaces.

The search algorithm roughly boils down to:

1.   Find the nearest clusters
2.   Evaluate all candidates from those clusters

The simplicity of this two step process is very convenient for turbopuffer because if the SSD cache is completely cold, we need to do at most 2 round-trips to object storage to run a query. This is opposed to graph-based ANN structures that would easily require a dozen or more roundtrips.

## Vector search + filters != filtered vector search

Consider the use case of semantically searching over a codebase. You may want to search for the top 10 most relevant files within `foo/src/*`.

```
|-----------------------------------------------------|
|        -                                            |
|                                                     |
|           -         -                               |
|  -                                                  |
|     -                -                              |
|                 -           -                       |
|         Q       -         -                         |
|                                                     |
|                             -                  +    |
|     -                                               |
|                        -                            |
|                           -                  +      |
|                                            +        |
|                                     +        +      |
|                                            +        |
|                                      +              |
|                                                     |
|-----------------------------------------------------|
| legend:                                             |
|  Q:  query vector                                   |
|  +:  document matching the filter                   |
|  -:  document not matching the filter               |
|-----------------------------------------------------|
```

```
|------------------------------|
|      -                       |
|         -       -            |
| -                            |
|    -             -           |
|              -        -      |
|      Q       -      -        |
|                              |
|                   -       +  |
|   -                          |
|               -              |
|                  -        +  |
|                       +      |
|                 +       +    |
|                       +      |
|                   +          |
|------------------------------|
| legend:                      |
|  Q: query vector             |
|  +: matches filter           |
|  -: no match                 |
|------------------------------|
```

The diagram above illustrates this problem, simplified to 2 vector dimensions and a small number of documents (typically the number of dimensions is 256 - 3072 and the number of documents is 0-1B+)

Assume we have some sort of vector index and some sort of attribute index (e.g B-Tree) on the `path` column. With only these indexes to work with, the query planner has two choices: either pre-filter or post-filter.

A pre-filter plan works as follows:

1.   Find all documents that match the filter
2.   Compute the distance to each one
3.   Return the nearest k

This plan would achieve 100% recall, but notice that step (2) doesn't actually use the vector index. Because of that, the cost of this plan is O(dimensions * matches). That's too slow.

A post-filter plan works as follows:

1.   Find the 10 approximate nearest neighbors to Q
2.   Filter the results

This is much faster because it uses the vector index. But as shown in the diagram, none of the nearest 10 documents match the filter! So the recall would be 0%, no matter how well the vector index performs on unfiltered queries.

So the pre-filter and post-filter plans are both bad. We want >90% recall with good latency, and that's not achieveable with query planner hacks unless the vector and the filtering indexes cooperate with each other.

```
| planner    | recall | perf |
|------------|--------|------|
| postfilter |     0% | 20ms |
| prefilter  |   100% | 10s  |
| target     |    90% | 25ms |
```

```
| planner    | recall | perf |
|------------|--------|------|
| postfilter |     0% | 20ms |
| prefilter  |   100% | 10s  |
| target     |    90% | 25ms |
```

## Native filtering

The goal of the query planner is that no matter what kind of filters we have, the number of candidates considered remains roughly the same as it would be for an unfiltered query.

To achieve this goal we designed our attribute indexes to be aware of the primary vector index, understand the clustering hierarchy and react to any changes (like the SPFresh rebalancing operations). This allows us to extract much more information from these indexes, and use them at every step of the vector search process. The resulting query plan is:

1.   Find the nearest clusters that contain at least one match
2.   Evaluate matching candidates from those clusters

The diagram below shows the same dataset as before. We can scan the `"foo/src/*"` range in the attribute index to see that all the matching results are in clusters 4 and 5, allowing us to skip clusters 0, 1, 2, 3, even though they are closer to the query point.

```
|-----------------------------------------------------|
| vector index:                                       |
|                                                     |
|        0                                            |
|                                                     |
|           0         1                               |
|  0                                                  |
|     0                1                              |
|                 1           3                       |
|         Q       1         3                         |
|                                                     |
|                             3                  4    |
|      2                                              |
|                         3                           |
|                            3                  5     |
|                                             5       |
|                                     5         5     |
|                                             5       |
|                                       5             |
|                                                     |
|-----------------------------------------------------|
| attribute indexes:                                  |
|   path=foo/readme.md -> [0, 1]                      |
|   path=foo/src/main.rs -> [5]                       |
|   path=foo/src/bar.rs -> [4, 5]                     |
|   ...                                               |
|-----------------------------------------------------|
| legend:                                             |
|  Q:  query vector                                   |
|  0:  document in cluster 0                          |
|  1:  document in cluster 1                          |
|      ...                                            |
|  i:  document in cluster i                          |
|-----------------------------------------------------|
```

```
|------------------------------|
| vector index:                |
|                              |
|      0                       |
|         0       1            |
| 0                            |
|    0             1           |
|              1        3      |
|      Q       1      3        |
|                              |
|                   3       4  |
|   2                          |
|               3              |
|                  3        5  |
|                       5      |
|                 5       5    |
|                       5      |
|                   5          |
|------------------------------|
| attribute indexes:           |
|  foo/readme.md -> [0,1]      |
|  foo/src/main.rs -> [5]      |
|  foo/src/bar.rs -> [4,5]     |
|------------------------------|
| Q: query vector              |
| 0-5: cluster numbers         |
|------------------------------|
```

## Implementation

The query filter can be a complicated nested expression involving And, Or, Eq, Glob, and [other operators](https://turbopuffer.com/docs/query#filtering-parameters) applied over one or more attributes. The attribute indexes need to quickly convert these filters to data relevant to vector search, like:

*   the set of relevant clusters
*   (if needed) the number of matches in each cluster
*   (if needed) the exact bitmap of matches within a cluster

Retrieval is an old problem with lots of [existing research](https://dbucsd.github.io/paperpdfs/2017_2.pdf). The only tpuf-specific aspects of it are:

1.   How to tie the attribute index to a vector index
2.   How to store this index on blob with minimal write amplification
3.   How to optimize query performance when the SSD cache is cold

The solution to (1) is simple. For each document, the primary vector index assigns an address in the form of `{cluster_id}:{local_id}`, where `local_id` is just a small number unique to that cluster. Then the attribute index uses these addresses to refer to documents. When an address changes, the attribute index needs to be updated.

Solving problem (2) is important because files on blob storage cannot be partially updated. They can only be fully rewritten. Our LSM tree storage engine allows us to implement partial file updates as key-value overwrites. To avoid fully overwriting the entire index on each update, we use `(attribute_value, cluster_id)` as the key, and `Set<local_id>` as the value in the LSM.

To solve problem (3) we need to ensure that the data needed to execute the filter can be downloaded from s3 quickly, and that the number of roundtrips required to run the filter is small. To keep index size small, we compress the `Set<local_id>` values as bitmaps. Additionally, we pre-compute a downsampled version of each index to cluster granularity, which allows us to perform bitmap unions and intersections on the cluster level before fetching exact bitmaps (if at all needed). This two-step process also limits the number of blob storage round-trips on a completely cold query.

```
|-----------------------------------------------------|
| row-level attribute indexes:                        |
|   path=foo/readme.md -> [0:0, 0:1, 1:2]             |
|   path=foo/src/main.rs -> [5:0, 5:1]                |
|   path=foo/src/bar.rs -> [4:0, 5:2, 5:3, 5:4]       |
|   ...                                               |
|-----------------------------------------------------|
| cluster-level attribute indexes:                    |
|   path=foo/readme.md -> [0, 1]                      |
|   path=foo/src/main.rs -> [5]                       |
|   path=foo/src/bar.rs -> [4, 5]                     |
|   ...                                               |
|-----------------------------------------------------|
```

```
|------------------------------|
| row-level indexes:           |
|  readme -> [0:0,0:1,1:2]     |
|  main.rs -> [5:0,5:1]        |
|  bar.rs -> [4:0,5:2,5:3,5:4] |
|------------------------------|
| cluster-level indexes:       |
|  readme -> [0,1]             |
|  main.rs -> [5]              |
|  bar.rs -> [4,5]             |
|------------------------------|
```

* * *
