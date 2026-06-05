# ANN v3: 200ms p99 query latency over 100 billion vectors

Updated: May 05, 2026•Nathan VanBenschoten (Chief Architect)

The pursuit of scale is not vanity. When you take existing systems and optimize them from first principles to achieve a step change in scalability, you can create something entirely new.

Nothing has demonstrated that more clearly than the explosion in deep learning over the past decade. The ML community took decades-old ideas and combined them with advancements in hardware, new algorithms, and hyper-specialization to forge something remarkable.

Both inspired by the ML community and in service of it, we recently rebuilt vector search in turbopuffer to support scales of up to **100 billion vectors** in a **single search index**. We call this technology Approximate Nearest Neighbor (ANN) Search v3, [and it is available now](https://turbopuffer.com/blog/ann-v3#use-now).

In this post, I'll dive into the technical details behind how we built for 100 billion vectors. Along the way, we’ll examine turbopuffer’s architecture, travel up the modern memory hierarchy, zoom into a single CPU core, and then back out to the scale of a distributed cluster.

### ANN v3 query latency, 100B vectors, unfiltered, 1024D, k=10, 92% recall

1.   p50

2.   p90

3.   p95

4.   p99

## Billion-scale ANN search

Let’s look at the numbers to get a sense of the challenge: 100 billion vectors, 1024 dimensions per vector, 2 bytes per dimension (`f16`). **This is vector search over 200TiB of dense vector data**. We want to serve a high rate (> 1k QPS) of ANN queries over this entire dataset, each with a latency target of 200ms or less.

With a healthy dose of mechanical sympathy, let’s consider how our hardware will run this workload and where it will encounter bottlenecks. If one part of the system bottlenecks (disk, network, memory, or CPU), other parts of the system will go underutilized. The key to making the most of the available hardware is to push down bottlenecks and balance resource utilization.

turbopuffer’s [architecture](https://turbopuffer.com/docs/architecture) is simple and opinionated. This simplicity makes the exercise tractable. turbopuffer’s query tier is a stateless layer on top of object storage, consisting of a caching hierarchy and compute. That’s it.

```
╔═ turbopuffer ════════════════════════════╗
╔════════════╗          ║                                          ║░
║            ║░         ║  ┏━━━━━━━━━━━━━━━┓     ┏━━━━━━━━━━━━━━┓  ║░
║   client   ║░───API──▶║  ┃    Memory/    ┃────▶┃    Object    ┃  ║░
║            ║░         ║  ┃   SSD Cache   ┃     ┃ Storage (S3) ┃  ║░
╚════════════╝░         ║  ┗━━━━━━━━━━━━━━━┛     ┗━━━━━━━━━━━━━━┛  ║░
 ░░░░░░░░░░░░░░         ║                                          ║░
                        ╚══════════════════════════════════════════╝░
                         ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
```

```
╔════════════╗
      ║   client   ║░
      ╚════════════╝░
       ░░░░░║░░░░░░░░
            ▼
╔═ turbopuffer ════════════╗
║  ┏━━━━━━━━━━━━━━━━━━━━┓  ║░
║  ┃    Memory/SSD      ┃  ║░
║  ┃      Cache         ┃  ║░
║  ┗━━━━━━━━┳━━━━━━━━━━━┛  ║░
║           ▼              ║░
║  ┏━━━━━━━━━━━━━━━━━━━━┓  ║░
║  ┃    Object Storage  ┃  ║░
║  ┃      (S3)          ┃  ║░
║  ┗━━━━━━━━━━━━━━━━━━━━┛  ║░
╚══════════════════════════╝░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░
```

### Identifying the bottleneck

When trying to process large amounts of data at high throughput, this system architecture could bottleneck in one of two ways. First, it could bottleneck on the CPU instructions needed to process the data (”compute-bound”). Second, it could bottleneck on the data path up the memory hierarchy feeding the CPU (”bandwidth-bound”).

We can borrow a strategy from the GPU community in order to estimate where it will bottleneck by classifying our workload’s [arithmetic intensity](https://en.wikipedia.org/wiki/Roofline_model#Arithmetic_intensity). Arithmetic intensity is the ratio of arithmetic operations to memory operations. It is often defined using GPU FLOPs and bytes transferred through GPU memory, but it is generalizable to other domains.

Different algorithms have different intensities. For example, a matrix-matrix multiplication is more intensive than a vector dot product. This is because in a matrix multiplication (`SGEMM`), each element in one matrix is multiplied against N elements (a full row or column) in the other matrix. In a vector dot product (`SDOT`), each element is multiplied only against one element from the other vector.

```
▲
               │                                    ╱
               │                  memory bandwidth
               │                   (bytes/second) ╱
               │
               │                                ╱              arithmetic bandwidth
               ├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─██───────────────────────┬────────────
               │                              ╱ ridge point
               │                             ╱                         │
               │                            ╱
               │                           ╱                           │
               │                          ╱
               │                         ╱                             │
  performance  │                        ╱
(FLOPS/second) │                       ╱ │                             │
               │                      ╱
               │                     ╱   │ arithmetic                  │ arithmetic
               │                    ╱      intensity 1                   intensity 2
               │                   ╱     │ (memory-bound)              │ (compute-bound)
               │                  ╱
               │                 ╱       │                             │
               │                ╱
               │               ╱         │                             │
               │              ╱
               │             ╱           │                             │
               │            ╱
               │           ╱             │                             │
               │          ╱
               │         ╱               │                             │
               │        ╱
               └───────╳─────────────────┴─────────────────────────────┴────────────▶
                                       arithmetic intensity
                                           (FLOPs/byte)
```

```
performance
(FLOPS/s)
▲
│                       ╱
│    memory bandwidth
│     (bytes/second)  ╱
│                     arithmetic
│                   ╱ bandwidth
├ ─ ─ ─ ─ ─ ─ ─ ─ ██───┬──────
│          ridge  ╱    │
│          point ╱     │
│               ╱      │
│              ╱       │
│             ╱        │
│            ╱         │
│           ╱          │
│          ╱│          │
│         ╱            │
│        ╱  │memory    │compute
│       ╱    bound     │bound
│      ╱    │          │
│     ╱                │
│    ╱      │          │
│   ╱                  │
│  ╱        │          │
│ ╱                    │
└╳──────────┴──────────┴──▶
       arithmetic intensity
               (FLOPs/byte)
```

_adapted from [https://modal.com/gpu-glossary/perf/arithmetic-intensity](https://modal.com/gpu-glossary/perf/arithmetic-intensity)_

As a rule of thumb, a workload that has a small constant arithmetic intensity (e.g., SDOT) will be memory-bound. A workload that has a large constant or linear arithmetic intensity (e.g., SGEMM) will be compute-bound. The intuition is simple enough — if a byte pulled into a compute register is only used once or twice, more work goes into fetching the byte than is needed to operate on it. Meanwhile, if that byte is used many times, the memory fetch is amortized and the computation over it dominates.

If we imagine the kernel of a vector search, the system fetches each data vector and performs a distance calculation between it and a query vector. This distance function is essentially a vector dot product, multiplying the data and query value in each of the corresponding vector dimensions.

```
╭  ╮   ╭  ╮
│d1│   │q1│
│d2│   │q2│
│d3│ • │q3│ = ∑ di • qi
│••│   │••│
│di│   │qi│
╰  ╯   ╰  ╯
```

```
╭  ╮   ╭  ╮
│d1│   │q1│
│d2│   │q2│
│d3│ • │q3│ = ∑ di • qi
│••│   │••│
│di│   │qi│
╰  ╯   ╰  ╯
```

Since each element in a data vector is used only once by the distance function, the arithmetic intensity of vector search is low. Most of the work goes into pulling many large data vectors into CPU registers. Recognizing this, we can predict that vector search will be **bandwidth-bound**, as is the case for many analytics and search systems.

Consequently, it doesn’t really matter how efficient the CPU instructions of our distance kernel are (within reason). If we are trying to maximize throughput (queries per second) on a machine, we are going to be limited by the number of data vectors we can run the kernel over each second.

With this insight in hand, our objective with ANN v3 is to **utilize cache space efficiently** and **balance bandwidth demands** to prevent the network, disk, or main memory from being a dominant bottleneck that limits the system’s ability to scale.

Let’s take a look at all of the places in turbopuffer’s memory hierarchy that might prevent a bandwidth-bound workload from scaling.

```
╱ ╲
                   ╱   ╲
                  ╱ CPU ╲_______________________│ Size: < 1 KB
                 ╱  Reg  ╲                      │ Bandwidth: >10 TB/s
                ╱—————————╲
               ╱  L1/L2/L3 ╲____________________│ Size: KBs - MBs
              ╱    Cache    ╲                   │ Bandwidth: 1 TB/s - 10s TB/s
             ╱———————————————╲
            ╱   Main Memory   ╲_________________│ Size: GBs - TBs
           ╱      (DRAM)       ╲                │ Bandwidth: 100 GB/s - 500 GB/s
          ╱—————————————————————╲
         ╱       NVMe SSD        ╲______________│ Size: TBs - 10s TBs
        ╱   (direct I/O cache)    ╲             │ Bandwidth: 1 GB/s - 30 GB/s
       ╱———————————————————————————╲
      ╱    Cloud Object Storage     ╲___________│ Size: PBs - EBs
     ╱  (e.g., S3, GCS, Azure Blob)  ╲          │ Bandwidth: 1 GB/s - 10 GB/s
    ╱—————————————————————————————————╲
```

```
┌────────────┐
│CPU         │ Size: <1KB
│Registers   │ BW: >10 TB/s
├────────────┤
│L1/L2/L3    │ Size: KB-MB
│Cache       │ BW: 1-10s TB/s
├────────────┤
│Main Memory │ Size: GB-TB
│(DRAM)      │ BW: 100-500 GB/s
├────────────┤
│NVMe SSD    │ Size: 1-10s TBs
│(direct I/O)│ BW: 1-30 GB/s
├────────────┤
│Object      │ Size: PB-EB
│Storage     │ BW: 1-10 GB/s
└────────────┘
```

We observe four distinct boundaries where bandwidth may become the limiting factor.

*   Object Storage (where data is durably stored) ↔ NVMe
*   NVMe ↔ DRAM
*   DRAM ↔ L3/L2/L1
*   L3/L2/L1 ↔ CPU registers (where the compute actually happens)

Take note of the difference in bandwidth between levels of the hierarchy, but also differences in size. Higher tiers are orders of magnitude smaller, but can service many orders of magnitude higher rates of data loads.

To balance bandwidth across this hierarchy, ANN v3 combines two complementary techniques: **hierarchical clustering** and **binary quantization**.

Each exploits the same general strategy of “approximation and refinement”. ANN v3 works by first quickly answering: _roughly_ where is the answer? and only then answering: out of that set, _exactly_ what is the answer?

### Hierarchical clustering to narrow the search space

The first technique is _hierarchical clustering_ in the index structure. Vector indexes in turbopuffer are based on [SPFresh](https://dl.acm.org/doi/10.1145/3600006.3613166), a centroid-based approximate nearest neighbor index that supports incremental updates. In a centroid-based index, vectors are grouped into clusters, each represented by a single "centroid" vector (typically the mean of all vectors in that cluster). At query time, we first compare the query to centroids to identify promising clusters, then search only within those clusters. We extended the SPTAG graph-based index described in the original SPFresh paper, nesting clusters hierarchically in a multi-dimensional tree structure.

While hierarchical clustering is not new to v3, it is a very important aspect of cold query performance in turbopuffer. When a namespace is _cold_ (not cached on SSD), turbopuffer must fetch some or all of it from object storage. Instead of traversing a graph with sequential object storage round-trips to locate the relevant data clusters, the hierarchy bounds the number of round-trips to object storage to the height of the SPFresh tree. This places a bound on tail latency, even for the coldest query.

```
┌───────────────────┐
                      │ root centroid c0  │
                      └───────────────────┘
                         ╱      │      ╲
                        ╱       │       ╲
┌───────────────────┐ ┌───────────────────┐ ┌───────────────────┐
│    centroid c1    │ │    centroid c2    │ │    centroid c3    │
└───────────────────┘ └───────────────────┘ └───────────────────┘
        ╱    │    ╲        ╱    │    ╲        ╱    │    ╲
       ╱     │     ╲      ╱     │     ╲      ╱     │     ╲
┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌──────────
│ data vec v1 |  │ data vec v2 |  │ data vec v3 │  │ •••
└─────────────┘  └─────────────┘  └─────────────┘  └──────────
```

```
┌───────────────┐
      │ root centroid │
      │       c0      │
      └───────────────┘
        ╱     │     ╲
       ╱      │      ╲
┌────────┐┌────────┐┌────────┐
│centroid││centroid││centroid│
|   c1   ││   c2   ││   c3   │
└────────┘└────────┘└────────┘
      ╱│╲       ╱│╲       ╱│╲
     ╱ │ ╲     ╱ │ ╲     ╱ │ ╲
    ╱  │  ╲   ╱  │  ╲   ╱  │  ╲
┌──────┐ ┌──────┐ ┌──────┐ ┌────
│ data │ │ data │ │ data │ │
|  v1  │ │  v2  │ │  v3  │ │ •••
└──────┘ └──────┘ └──────┘ └────
```

In the case of 100 billion vector search, we can’t afford to contend with the low bandwidth of object storage (<5 GB/s) for even a fraction of data vector reads, so we size deployments to store the entire tree on SSD. Yet even when cached, the tree structure complements the hardware.

Clustering interacts well with the memory hierarchy because it provides **spatial locality**. Vectors closer in space - those likely to be accessed together - are stored contiguously. This makes memory and disk accesses efficient. Specifically, it means that there is very little amplification when reading from lower levels of the hierarchy, even when those levels enforce a minimum read granularity (e.g., 4KiB from disk). Every byte fetched will be put to good use.

_Hierarchical_ clustering, specifically, interacts well with the memory hierarchy because it provides **temporal locality**. Vector clusters in the upper levels of the tree are accessed frequently, so they will naturally remain resident in main memory. We use a 100x branching factor between levels of our tree to balance tree depth with cluster size. Each node in the tree has approximately 100 children, creating a wide, shallow tree structure. This branching factor roughly matches the size ratio between DRAM and SSD (10x - 50x), meaning that if we can fit all data vectors on SSD, we can fit all centroid vectors in DRAM.

All of this gets us back to the original purpose of the approximate nearest neighbor index: reducing the search space for each query. Approximate indexes are a compromise between performance and recall. For centroid-based indexes, we navigate this compromise by controlling how many clusters are scanned at each level of the tree (often called the "probes" or "beam width" parameter).

For vector search at this scale, we found experimentally that with good clustering, we needed to search about 500 data vector clusters (each 100 vectors large) on each machine to achieve our recall target. This equates to a bandwidth requirement of **100MB per level of the tree**:

```
100 vectors     1024 dimensions      2 bytes
500 clusters x ───────────── x ───────────────── x ─────────── = 100MB per level
                  cluster           vector          dimension
```

```
100 vec   1024D   2 bytes
500 • ─────── • ───── • ───────
      cluster   vector  dim

    = 100MB per level
```

We can use this to estimate throughput limits:

|  | ANN tree levels | memory hierarchy tier | bandwidth | max qps |
| --- | --- | --- | --- | --- |
| centroid vectors (upper levels) | 3 | DRAM | 300 GB/s | **1,000 qps** (300GB/(3x100MB)) |
| data vectors (lowest level) | 1 | NVMe SSD | 10 GB/s | **100 qps** (10GB/(1x100MB)) |

The derivation shows that with hierarchical clustering alone, we will end up disk-bound fetching data vector clusters and maxing out at around 100 queries per second. 100 qps over 100 billion vectors is admirable, but we can do better.

### Binary quantization to compress vector sizes

The second technique is _binary quantization_ of data vectors. When vectors are inserted into turbopuffer, the system stores both the full-precision vector (`f32` or `f16` per dimension) and a transparently computed, quantized form of the vector (1-bit per dimension).

```
[ 0.94, -0.01,  0.39, -0.72,  0.02, -0.85, -0.18,  0.99,  0.45 ]
                                    |
                                    │ binary quantization
                                    │
                                    ▼
                      [ 1, 0, 1, 0, 1, 0, 0, 1, 1 ]
```

```
╭       ╮               ╭   ╮
│  0.94 │               │ 1 │
│ -0.01 │               │ 0 │
│  0.39 │               │ 1 │
│ -0.72 │ ────────────▶ │ 0 │
│  0.02 │    binary     │ 1 │
│ -0.85 │ quantization  │ 0 │
│ -0.18 │               │ 0 │
│  0.99 │               │ 1 │
│  0.45 │               │ 1 │
╰       ╯               ╰   ╯
```

The math works out as expected; **binary quantization provides a 16-32x compression for data vectors**. This allows these quantized vectors to be stored higher in the memory hierarchy and minimizes their memory bandwidth demands.

To avoid this compression leading to a loss of search quality, ANN v3 employs the [RaBitQ](https://dl.acm.org/doi/pdf/10.1145/3654970) quantization method. RaBitQ exploits the mathematical properties of high-dimensional space ([concentration of measure](https://en.wikipedia.org/wiki/Concentration_of_measure)) to compress aggressively while preserving high recall. Specifically, in high dimensions, vector components naturally become more uniformly distributed, which means quantization errors are spread evenly across all dimensions rather than concentrated in problematic directions. This uniform error distribution enables RaBitQ to provide tight theoretical error bounds alongside distance estimations made on quantized vectors.

For example, consider a data vector V d and query vector V q. A full-precision distance computation might compute the cosine distance between V d and V q as 0.75. Now consider their binary quantized forms, V d' and V q'. Due to the loss of information from quantizing, RaBitQ cannot perfectly compute the true cosine distance by looking just at the quantized vectors. Instead, it will compute an estimated range like [0.69, 0.83].

Despite being imprecise, this confidence interval can be used to conclude that V d is closer to V q than some other vector V d2 whose quantized distance estimate has a range [0.87, 0.91]. For other data vectors with overlapping distance estimate ranges (e.g., [0.51, 0.77]), RaBitQ makes no promises. Such vectors must be recompared using their full precision vectors.

During a vector search, turbopuffer first evaluates the search on quantized vectors, uses the error bounds to determine all vectors that could be in the _true_ top-k, then fetches their corresponding full-precision vectors, and reranks over those to compute the final result. In practice, we find that less than 1% of data vectors in the narrowed search space need to be reranked to avoid an impact on recall.

## Putting it all together...

These two techniques compose, and their benefits multiply.

Upper levels of a quantized ANN tree are small but frequently accessed. As a result, they naturally remain resident all the way up in the L3 CPU cache. We can write out the math to demonstrate this to ourselves. With a branching factor of 100, each level L in the tree contains `100^L * 1024/8` bytes of quantized vector data (1 bit per dimension). With a 504MiB shared L3 cache, we can fit all three upper levels of the tree in L3 cache, the largest requiring `100^3 * 128 = 128 MiB` of cache space.

The lowest level of the quantized ANN tree is stored in DRAM, as was the case before. However, because these vectors are compressed, they require less memory bandwidth to access.

Meanwhile, the full-precision vectors remain on local SSD. However, only a small fraction is fetched during the reranking phase, through a highly concurrent [scatter-gather](https://en.wikipedia.org/wiki/Gather/scatter_(vector_addressing)). This access pattern is ideal for modern NVMe drives, which have random read latencies around 50-100 microseconds and excel at handling many parallel I/O operations simultaneously, allowing us to access the full bandwidth of the disks.

We can again estimate throughput limits. Remember that quantized vectors are 16x smaller than unquantized `f16` vectors, equating to a bandwidth requirement of `500 x 100 x 1024 x 2 / 16 = 6MB` per level of the tree.

|  | ANN tree levels | memory hierarchy tier | bandwidth | max qps |
| --- | --- | --- | --- | --- |
| quantized centroid vectors (upper levels) | 3 | L3 cache | 600GB/s | **33,000 qps** (600GB/(3x6MB)) |
| quantized data vectors (lowest level) | 1 | DRAM | 300GB/s | **50,000 qps** (300GB/(1x6MB)) |
| full precision data vectors (lowest level) | 1% of 1 | NVMe SSD | 10 GB/s | **10,000 qps** (10GB/(0.01x100MB)) |

The changes here demonstrate a remarkable dynamic in the memory hierarchy. Data compression both reduces bandwidth requirements and allows data to remain resident in higher bandwidth tiers at the same time, leading to a multiplicative effect. Whereas before the theoretical throughput limit was 100 qps, quantization unlocks a theoretical limit of 10,000 qps!

## ...to end up compute-bound...

By combining hierarchical clustering with binary quantization, ANN v3 makes efficient use of cache space and balances bandwidth demands across the tiers of the memory hierarchy.

However, if we run a production load against it, we notice something interesting. Instead of hitting the theoretical 10,000 qps, the system ends up saturated around 1,000 qps, 10% of where we’d like to be.

What is happening here? Why are we no longer limited by disk bandwidth, and instead limited somewhere else?

Returning to our framework for arithmetic intensity, we discover what changed. When we added in binary quantization, intensity shot up. With `f16` vector elements, every two bytes fetched from memory were used for a single logical operation. With binary quantized elements, however, every two bytes fetched are used for **sixteen** logical operations (one per bit). In fact, the RaBitQ algorithm reuses each bit four times when computing distance estimates (section 3.3.2 of the [paper](https://arxiv.org/pdf/2405.12497)), leading to 64x higher arithmetic intensity.

This large constant arithmetic intensity is enough to tip the scale towards the system being compute-bound. Each CPU core in our system cannot keep up with the rate of highly compressed vector data being fed to it, bottlenecking throughput.

Optimizing a compute-bound system is a different game, requiring an obsessive focus on:

1.   doing the same work with fewer instructions
2.   keeping the CPU pipelines fed by avoiding stalls and branch mispredictions

As an example, we were recently surprised to find that [AVX2](https://en.wikipedia.org/wiki/Advanced_Vector_Extensions) (256-bit x86 SIMD) does not have an accelerated [popcount instruction](https://en.wikichip.org/wiki/population_count). Counting the number of bits set in a series of bytes is an important operation for RaBitQ, so making this operation fast was essential. We switched a core distance estimation kernel over to [AVX-512](https://en.wikipedia.org/wiki/AVX-512) to gain access to its `VPOPCNTDQ` instruction. This instruction is capable of counting the bits set to one across a 512-bit register in only 3 cpu cycles of latency. Better yet, it can be pipelined for a throughput of 1 instruction per cycle.

Making the switch improved the performance of the kernel in microbenchmarks by 30% and improved end-to-end production throughput by about 5%. When microbenchmarks translate to end-to-end performance gains, you have correctly identified the system’s bottleneck.

A more general discussion of optimizing a compute-bound system is a deeper topic than we have room for in this post, but it remains a focus of our work to this day as we continue to optimize ANN v3 for higher throughput.

## ...in a distributed cluster

Careful readers may have noticed one inconsistency in the description of ANN v3 so far. I mentioned up top that our goal was to handle vector search over 200 TiB of dense vector data. We also discussed that for this workload shape, all of this data should be cached on SSD to avoid the limited bandwidth and high latency of object storage. And yet, NVMe SSDs only get so large...

This is where distribution comes in. Modern clouds provide storage-dense VMs with fast locally attached NVMe drives (e.g., GCP's `z3`, AWS’s `I7i`, Azure’s `Lsv4`). These drives are large (10-40TiB) but not 200TiB large. To achieve the desired aggregate SSD capacity, we use a cluster of storage-optimized machines, each storing a subset of the index.

At this scale, simple random sharding works well. During ingestion, each vector is randomly assigned to one of the shards of the index. An ANN search query is broadcast to all shards, and the global top-k is stitched together from the sub-results.

This technique can scale to arbitrarily large indexes, but its cost scales linearly with the number of machines in the cluster. This is why it is crucial to maximize the efficiency of a single machine before turning to distribution. Moving in the other direction leads to an unnecessarily expensive system.

## Conclusion

When performance at scale becomes cost-efficient, it stops being a benchmarking exercise and becomes a building block.

The ANN v3 architecture pushes turbopuffer to 100 billion-vector scale at thousands of QPS, while holding p99 latency at 200ms. More importantly, it does this while keeping costs low enough to run continuously in production. It achieves this through hierarchical clustering that matches the memory hierarchy, binary quantization that compresses vectors by 16-32x with reranking to maintain recall, and distribution across storage-dense machines - all working together to maximize hardware utilization and minimize bottlenecks.

turbopuffer customers can now use ANN v3 with the sharding and pinning technique by splitting their largest indexes into 4 TiB namespaces and randomly assigning vectors across them, then [pinning each shard](https://turbopuffer.com/docs/pinning) to its own SSD. We're also working on making the sharding fully transparent in the future.
