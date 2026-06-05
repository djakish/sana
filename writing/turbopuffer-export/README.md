# turbopuffer content export

Markdown export of the turbopuffer **docs** and **blog**, fetched 2026-06-01.

- `docs/` — official documentation (source: turbopuffer's native `.md` pages, byte-for-byte)
- `blog/` — blog posts & podcast/video write-ups (converted from HTML to markdown)

## Documentation

### Platform

- [Introduction](docs/index.md)
- [Architecture](docs/architecture.md)
- [Concepts](docs/concepts.md)
- [Guarantees](docs/guarantees.md)
- [Tradeoffs](docs/tradeoffs.md)
- [Limits](docs/limits.md)
- [Regions](docs/regions.md)
- [Roadmap & Changelog](docs/roadmap.md)
- [Security & Compliance](docs/security.md)
- [Setting up CMEK encryption with an EKM](docs/cmek.md)
- [Private Networking](docs/private-networking.md)
- [Audit Logs](docs/audit-logs.md)
- [Cross-Region Backups](docs/backups.md)
- [Optimizing Performance](docs/performance.md)
- [Namespace Pinning](docs/pinning.md)
- [Namespace Branching](docs/branching.md)

### Guides

- [Quickstart Guide](docs/quickstart.md)
- [Vector Search Guide](docs/vector.md)
- [Full-Text Search Guide](docs/fts.md)
- [Hybrid Search](docs/hybrid.md)
- [Chunking](docs/chunking.md)
- [Testing](docs/testing.md)
- [Permissions](docs/permissions.md)

### API

- [API Overview](docs/overview.md)
- [Write documents](docs/write.md)
- [Query documents](docs/query.md)
- [Metadata](docs/metadata.md)
- [Export documents](docs/export.md)
- [Warm cache](docs/warm-cache.md)
- [List namespaces](docs/namespaces.md)
- [Delete namespace](docs/delete-namespace.md)
- [Evaluate recall](docs/recall.md)

### BYOC (self-hosted)

- [turbopuffer BYOC](docs/byoc.md)
- [BYOC Deployment Runlist](docs/byoc/deployment.md)
- [Configuration](docs/byoc/configuration.md)
- [Control Plane](docs/byoc/control-plane.md)
- [Common Operations](docs/byoc/operations.md)
- [Requirements](docs/byoc/requirements.md)

### Other

- [Enterprise](docs/enterprise.md)
- [Pricing Changelog](docs/pricing-log.md)
- [Vulnerability Disclosure](docs/vdp.md)

## Blog

- [Training SID-1 to beat GPT-5 at search with 1k+ QPS RL](blog/reinforcement-learning-sid-ai.md)  
  _May 20, 2026 • Max Rumpf (Co-founder of SID), Sam Dauncey (Researcher at SID)_
- [Simon Eskildsen on scaling Shopify, building turbopuffer, and the future of databases](blog/podcast-cafe-cursor.md)  
  _May 14, 2026 • Cafe Cursor_
- [Mixing numeric attributes into text search for better first-stage relevance](blog/rank-by-attribute.md)  
  _April 27, 2026 • Adrien Grand (Engineer)_
- [Retrieval After RAG: Hybrid Search, Agents, and Database Design](blog/podcast-latent-space.md)  
  _March 12, 2026 • Latent Space Podcast_
- [Object storage-native database for search](blog/video-andy-pavlo-cmu.md)  
  _March 09, 2026 • CMU Seminar with Andy Pavlo_
- [Rust zero-cost abstractions vs. SIMD](blog/zero-cost.md)  
  _Updated: March 08, 2026 • Xavier Denis (Engineer)_
- [How to build a distributed queue in a single JSON file on object storage](blog/object-storage-queue.md)  
  _February 12, 2026 • Dan Harrison (Engineer)_
- [ANN v3: 200ms p99 query latency over 100 billion vectors](blog/ann-v3.md)  
  _Updated: May 05, 2026 • Nathan VanBenschoten (Chief Architect)_
- [Designing inverted indexes in a KV-store on object storage](blog/fts-v2-postings.md)  
  _January 14, 2026 • Morgan Gallant (Engineer), Adrien Grand (Engineer)_
- [Why BM25 queries with more terms can be faster (and other scaling surprises)](blog/bm25-latency-musings.md)  
  _January 07, 2026 • Adrien Grand (Engineer)_
- [Vectorized MAXSCORE over WAND, especially for long LLM-generated queries](blog/fts-v2-maxscore.md)  
  _Updated: January 14, 2026 • Adrien Grand (Engineer), Morgan Gallant (Engineer)_
- [FTS v2: up to 20x faster full-text search](blog/fts-v2.md)  
  _Updated: February 03, 2026 • Adrien Grand (Engineer), Morgan Gallant (Engineer), Nikhil Benesch (Engineer)_
- [Faster vector search](blog/podcast-the-database-school-aaron-francis.md)  
  _November 13, 2025 • The Database School Podcast_
- [Billion-scale vector storage for RAG](blog/podcast-jason-liu.md)  
  _November 04, 2025 • Jason Liu Podcast_
- [He built a new database in his bedroom](blog/podcast-pmf-show-he-built-new-database-bedroom-now-powers-cursor-notion-anthropic.md)  
  _October 30, 2025 • The PMF Show_
- [Economical way of serving vector search workloads](blog/podcast-vector-podcast-economical-way-serving-vector-search-workloads.md)  
  _September 18, 2025 • Vector Podcast_
- [turbopuffer on Postgres FM](blog/podcast-postgres-fm-turbopuffer.md)  
  _September 12, 2025 • Postgres FM_
- [Memory, evals, and efficient storage in AI systems with turbopuffer and Braintrust](blog/podcast-bessemer.md)  
  _September 11, 2025 • Bessemer Podcast_
- [How to build 10x cheaper with object storage](blog/podcast-barrchives-podcast-how-build-10x-cheaper-object-storage.md)  
  _August 05, 2025 • Barrchives Podcast_
- [The infrastructure company powering the top AI apps](blog/podcast-unsupervised-learning-redpoint-ai-podcast-infrastructure-company-powering-top-ai-apps.md)  
  _July 22, 2025 • Unsupervised Learning_
- [Billion-scale vector search with Notion](blog/video-data-council-notion.md)  
  _May 29, 2025 • Data Council Conference_
- [How do vector (search) databases work?](blog/podcast-how-do-search-databases-work.md)  
  _March 29, 2025 • The Geek Narrator Podcast_
- [Native filtering for high-recall vector search](blog/native-filtering.md)  
  _January 21, 2025 • Bojan Serafimov (Engineer)_
- [Building a database on object storage](blog/podcast-database-from-first-principles.md)  
  _November 16, 2024 • The Geek Narrator Podcast_
- [How to use AI to become a learning machine](blog/podcast-every-how-use-ai-become-learning-machine.md)  
  _September 11, 2024 • Every Podcast_
- [Continuous recall measurement](blog/continuous-recall.md)  
  _September 04, 2024 • Morgan Gallant (Engineer)_
- [turbopuffer: fast search on object storage](blog/turbopuffer.md)  
  _Updated: March 05, 2026 • Simon Hørup Eskildsen (Co-founder & CEO)_
