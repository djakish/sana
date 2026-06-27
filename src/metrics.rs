//! In-process metrics registry. Dependency-free: counters and histogram
//! buckets are plain atomics and the only exposition format is Prometheus
//! text, rendered by hand.
//!
//! The registry is shared as `Arc<Metrics>`. The metered object-store decorator
//! increments it on the I/O path, namespace write/query paths record phase
//! latencies, and the `/metrics` endpoint renders a snapshot. Reads are
//! `Relaxed`: counters are independent and a metrics scrape never needs a
//! consistent cross-counter view.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Histogram bucket upper bounds in microseconds: 100µs doubling to ~6.6s.
/// Spans sub-millisecond local-disk requests through multi-second scans.
const BUCKET_BOUNDS_US: [u64; 17] = [
    100, 200, 400, 800, 1_600, 3_200, 6_400, 12_800, 25_600, 51_200, 102_400, 204_800, 409_600,
    819_200, 1_638_400, 3_276_800, 6_553_600,
];

/// Bucket count including the final overflow (`+Inf`) bucket.
const BUCKETS: usize = BUCKET_BOUNDS_US.len() + 1;

/// Queue broker batch-size buckets, in number of mutation requests.
const BATCH_SIZE_BOUNDS: [u64; 11] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1_024];

/// Bucket count including the final overflow (`+Inf`) bucket.
const VALUE_BUCKETS: usize = BATCH_SIZE_BOUNDS.len() + 1;

#[derive(Debug, Default)]
pub struct Metrics {
    pub object_store: ObjectStoreMetrics,
    pub cache: CacheMetrics,
    pub latency: LatencyMetrics,
    pub search: SearchMetrics,
    pub queue: QueueMetrics,
    pub index_lag: IndexLagMetrics,
    pub maintenance: MaintenanceMetrics,
    pub worker: WorkerMetrics,
}

impl Metrics {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            object_store: self.object_store.snapshot(),
            cache: self.cache.snapshot(),
            latency: self.latency.snapshot(),
            search: self.search.snapshot(),
            queue: self.queue.snapshot(),
            index_lag: self.index_lag.snapshot(),
            maintenance: self.maintenance.snapshot(),
            worker: self.worker.snapshot(),
        }
    }
}

pub fn incr(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub fn add(counter: &AtomicU64, amount: u64) {
    counter.fetch_add(amount, Ordering::Relaxed);
}

pub fn set(gauge: &AtomicU64, value: u64) {
    gauge.store(value, Ordering::Relaxed);
}

/// Backend object-store traffic, counted below the cache so only true round
/// trips register. Byte counters are decoded payload sizes, not wire sizes.
#[derive(Debug, Default)]
pub struct ObjectStoreMetrics {
    pub gets: AtomicU64,
    pub get_ranges: AtomicU64,
    pub puts: AtomicU64,
    pub puts_if_absent: AtomicU64,
    pub compare_and_sets: AtomicU64,
    pub cas_mismatches: AtomicU64,
    pub lists: AtomicU64,
    pub deletes: AtomicU64,
    pub get_bytes: AtomicU64,
    pub range_bytes: AtomicU64,
    pub put_bytes: AtomicU64,
    /// Wall-clock latency of every backend request, all families together.
    /// The per-family counters above give the mix; this gives the round trip.
    pub request_latency: Histogram,
}

impl ObjectStoreMetrics {
    fn snapshot(&self) -> ObjectStoreSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        ObjectStoreSnapshot {
            gets: load(&self.gets),
            get_ranges: load(&self.get_ranges),
            puts: load(&self.puts),
            puts_if_absent: load(&self.puts_if_absent),
            compare_and_sets: load(&self.compare_and_sets),
            cas_mismatches: load(&self.cas_mismatches),
            lists: load(&self.lists),
            deletes: load(&self.deletes),
            get_bytes: load(&self.get_bytes),
            range_bytes: load(&self.range_bytes),
            put_bytes: load(&self.put_bytes),
            request_latency: self.request_latency.snapshot(),
        }
    }
}

/// Mirror of the immutable-object cache's internal stats. The cache's
/// mutex-guarded state is the source of truth; it *sets* these after each
/// operation rather than incrementing them, so one cache per registry.
#[derive(Debug, Default)]
pub struct CacheMetrics {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub bypasses: AtomicU64,
    pub evictions: AtomicU64,
    pub admission_rejections: AtomicU64,
    pub capacity_bytes: AtomicU64,
    pub resident_bytes: AtomicU64,
    pub entries: AtomicU64,
}

impl CacheMetrics {
    fn snapshot(&self) -> CacheSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        CacheSnapshot {
            hits: load(&self.hits),
            misses: load(&self.misses),
            bypasses: load(&self.bypasses),
            evictions: load(&self.evictions),
            admission_rejections: load(&self.admission_rejections),
            capacity_bytes: load(&self.capacity_bytes),
            resident_bytes: load(&self.resident_bytes),
            entries: load(&self.entries),
        }
    }
}

/// Vector-ANN and full-text work counters, recorded by the query executor.
#[derive(Debug, Default)]
pub struct SearchMetrics {
    /// ANN queries served from published vector segments (not exact fallback).
    pub ann_queries: AtomicU64,
    /// Posting candidates returned by segment scans before liveness filtering.
    pub ann_candidates: AtomicU64,
    /// RaBitQ code estimations performed (L2 quantized path only).
    pub ann_estimated: AtomicU64,
    /// Candidates exact-reranked after the confidence-bound prune.
    pub ann_reranked: AtomicU64,
    /// Candidates eliminated by the RaBitQ lower bound without a rerank.
    pub ann_pruned: AtomicU64,
    /// Text queries executed (indexed or exhaustive path).
    pub text_queries: AtomicU64,
    /// Posting blocks decoded and scored by block MAXSCORE.
    pub text_blocks_read: AtomicU64,
    /// Posting blocks skipped by the rank-safe block upper bound.
    pub text_blocks_skipped: AtomicU64,
}

impl SearchMetrics {
    fn snapshot(&self) -> SearchSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        SearchSnapshot {
            ann_queries: load(&self.ann_queries),
            ann_candidates: load(&self.ann_candidates),
            ann_estimated: load(&self.ann_estimated),
            ann_reranked: load(&self.ann_reranked),
            ann_pruned: load(&self.ann_pruned),
            text_queries: load(&self.text_queries),
            text_blocks_read: load(&self.text_blocks_read),
            text_blocks_skipped: load(&self.text_blocks_skipped),
        }
    }
}

/// Store-global indexing queue metrics. These are measured at the queue owner
/// rather than inferred from broad object-store counters, so a broker scrape can
/// answer whether the single JSON queue is still healthy before sharding it.
#[derive(Debug, Default)]
pub struct QueueMetrics {
    pub cas_attempts: AtomicU64,
    pub cas_successes: AtomicU64,
    pub cas_retries: AtomicU64,
    pub broker_batches: AtomicU64,
    pub broker_batch_requests: AtomicU64,
    pub jobs: AtomicU64,
    pub available_jobs: AtomicU64,
    pub claimed_jobs: AtomicU64,
    pub oldest_job_age_seconds: AtomicU64,
    pub broker_batch_size: ValueHistogram,
    pub claim_wait: Histogram,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct QueueStateSample {
    pub jobs: u64,
    pub available_jobs: u64,
    pub claimed_jobs: u64,
    pub oldest_job_age_seconds: u64,
}

impl QueueMetrics {
    pub fn record_state(&self, sample: QueueStateSample) {
        set(&self.jobs, sample.jobs);
        set(&self.available_jobs, sample.available_jobs);
        set(&self.claimed_jobs, sample.claimed_jobs);
        set(&self.oldest_job_age_seconds, sample.oldest_job_age_seconds);
    }

    pub fn record_broker_batch(&self, request_count: u64) {
        incr(&self.broker_batches);
        add(&self.broker_batch_requests, request_count);
        self.broker_batch_size.observe(request_count);
    }

    pub fn record_claim_wait(&self, wait: Duration) {
        self.claim_wait.observe(wait);
    }

    fn snapshot(&self) -> QueueSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        QueueSnapshot {
            cas_attempts: load(&self.cas_attempts),
            cas_successes: load(&self.cas_successes),
            cas_retries: load(&self.cas_retries),
            broker_batches: load(&self.broker_batches),
            broker_batch_requests: load(&self.broker_batch_requests),
            jobs: load(&self.jobs),
            available_jobs: load(&self.available_jobs),
            claimed_jobs: load(&self.claimed_jobs),
            oldest_job_age_seconds: load(&self.oldest_job_age_seconds),
            broker_batch_size: self.broker_batch_size.snapshot(),
            claim_wait: self.claim_wait.snapshot(),
        }
    }
}

/// Per-namespace indexing lag, refreshed by each reconciliation scan. The map
/// is replaced wholesale so deleted namespaces drop out of the scrape.
#[derive(Debug, Default)]
pub struct IndexLagMetrics {
    samples: Mutex<BTreeMap<String, IndexLagSample>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IndexLagSample {
    pub unindexed_bytes: u64,
    pub unindexed_batches: u64,
}

impl IndexLagMetrics {
    pub fn record(&self, samples: BTreeMap<String, IndexLagSample>) {
        *self
            .samples
            .lock()
            .expect("index-lag mutex is never poisoned") = samples;
    }

    fn snapshot(&self) -> BTreeMap<String, IndexLagSample> {
        self.samples
            .lock()
            .expect("index-lag mutex is never poisoned")
            .clone()
    }
}

/// Background maintenance-loop outcomes, recorded once per policy pass so a
/// `/metrics` scrape can answer whether compaction, vector maintenance, and GC
/// are making progress without parsing stderr. The leader-lease loser path and
/// whole-pass failures are counted separately from per-namespace errors.
#[derive(Debug, Default)]
pub struct MaintenanceMetrics {
    /// Passes that ran (this process held the leader lease).
    pub passes: AtomicU64,
    /// Passes skipped because another process held the maintenance lease.
    pub skipped_leased_passes: AtomicU64,
    pub compactions: AtomicU64,
    pub vector_maintenance: AtomicU64,
    /// Fresh orphan candidates observed this pass (deletable next pass).
    pub gc_candidates: AtomicU64,
    pub gc_deletions: AtomicU64,
    /// Per-namespace failures plus whole-pass failures.
    pub errors: AtomicU64,
}

/// One maintenance pass folded into counter deltas. Built from a
/// `MaintenanceReport` by the caller so this module stays independent of the
/// maintenance layer, mirroring [`QueueStateSample`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MaintenancePassSample {
    pub compactions: u64,
    pub vector_maintenance: u64,
    pub gc_candidates: u64,
    pub gc_deletions: u64,
    pub errors: u64,
}

impl MaintenanceMetrics {
    /// Record a completed (leader-held) pass.
    pub fn record_pass(&self, sample: MaintenancePassSample) {
        incr(&self.passes);
        add(&self.compactions, sample.compactions);
        add(&self.vector_maintenance, sample.vector_maintenance);
        add(&self.gc_candidates, sample.gc_candidates);
        add(&self.gc_deletions, sample.gc_deletions);
        add(&self.errors, sample.errors);
    }

    /// Record a pass skipped because another process owns the leader lease.
    pub fn record_skipped_pass(&self) {
        incr(&self.skipped_leased_passes);
    }

    /// Record a pass that failed before producing a report.
    pub fn record_failed_pass(&self) {
        incr(&self.errors);
    }

    fn snapshot(&self) -> MaintenanceSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        MaintenanceSnapshot {
            passes: load(&self.passes),
            skipped_leased_passes: load(&self.skipped_leased_passes),
            compactions: load(&self.compactions),
            vector_maintenance: load(&self.vector_maintenance),
            gc_candidates: load(&self.gc_candidates),
            gc_deletions: load(&self.gc_deletions),
            errors: load(&self.errors),
        }
    }
}

/// Indexing-worker job outcomes, recorded by the serve worker loop. `claims`
/// counts every job the worker engaged this tick; `flushes` is the subset that
/// published a new index (the rest were already indexed). `failures` are jobs
/// returned to the queue for retry; `stale_claim_rejections` are claims fenced
/// at publish or heartbeat because they went stale.
#[derive(Debug, Default)]
pub struct WorkerMetrics {
    pub claims: AtomicU64,
    pub flushes: AtomicU64,
    pub failures: AtomicU64,
    pub stale_claim_rejections: AtomicU64,
}

impl WorkerMetrics {
    fn snapshot(&self) -> WorkerSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        WorkerSnapshot {
            claims: load(&self.claims),
            flushes: load(&self.flushes),
            failures: load(&self.failures),
            stale_claim_rejections: load(&self.stale_claim_rejections),
        }
    }
}

/// Wall-clock latency at the dominant write/query seams. Phases are the
/// expensive spans, not a partition: a request's phases need not sum to its
/// `*_total` observation, and a phase that does not occur for a request
/// (e.g. `plan` for a plain append) records nothing.
#[derive(Debug, Default)]
pub struct LatencyMetrics {
    /// End-to-end accepted write request (any kind).
    pub write_total: Histogram,
    /// Pre-commit strong-snapshot work: filter-mutation candidate discovery.
    pub write_plan: Histogram,
    /// Commit-lock wait plus staging, CAS reservation, and WAL publication.
    pub write_commit: Histogram,
    /// Post-commit advisory indexing-queue enqueue.
    pub write_notify: Histogram,
    /// End-to-end query/multi-query request.
    pub query_total: Histogram,
    /// Strong snapshot load: manifest pointer + body + commit state.
    pub query_plan: Histogram,
    /// Candidate-generation reads: attribute SSTs, text postings, or vector
    /// segments plus their version map.
    pub query_candidates: Histogram,
    /// Unindexed WAL overlay read.
    pub query_overlay: Histogram,
    /// Scoring/ordering: ANN scan, BM25 search, exact rerank, or sort.
    pub query_rank: Histogram,
    /// Resolving candidate ids to documents (or the full-scan fallback).
    pub query_materialize: Histogram,
}

impl LatencyMetrics {
    fn snapshot(&self) -> LatencySnapshot {
        LatencySnapshot {
            write_total: self.write_total.snapshot(),
            write_plan: self.write_plan.snapshot(),
            write_commit: self.write_commit.snapshot(),
            write_notify: self.write_notify.snapshot(),
            query_total: self.query_total.snapshot(),
            query_plan: self.query_plan.snapshot(),
            query_candidates: self.query_candidates.snapshot(),
            query_overlay: self.query_overlay.snapshot(),
            query_rank: self.query_rank.snapshot(),
            query_materialize: self.query_materialize.snapshot(),
        }
    }
}

/// Fixed-bucket latency histogram. Buckets hold per-bucket counts internally
/// and render cumulatively as Prometheus expects; `_count` is the `+Inf` line.
#[derive(Debug, Default)]
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    sum_micros: AtomicU64,
}

impl Histogram {
    pub fn observe(&self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let slot = BUCKET_BOUNDS_US
            .iter()
            .position(|bound| micros <= *bound)
            .unwrap_or(BUCKET_BOUNDS_US.len());
        if let Some(bucket) = self.buckets.get(slot) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }

    /// Run `work` and record its wall-clock duration, success or failure.
    pub async fn time<T>(&self, work: impl Future<Output = T>) -> T {
        let start = Instant::now();
        let out = work.await;
        self.observe(start.elapsed());
        out
    }

    fn snapshot(&self) -> HistogramSnapshot {
        let mut buckets = [0u64; BUCKETS];
        for (bucket, source) in buckets.iter_mut().zip(&self.buckets) {
            *bucket = source.load(Ordering::Relaxed);
        }
        HistogramSnapshot {
            buckets,
            sum_micros: self.sum_micros.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MetricsSnapshot {
    pub object_store: ObjectStoreSnapshot,
    pub cache: CacheSnapshot,
    pub latency: LatencySnapshot,
    pub search: SearchSnapshot,
    pub queue: QueueSnapshot,
    pub index_lag: BTreeMap<String, IndexLagSample>,
    pub maintenance: MaintenanceSnapshot,
    pub worker: WorkerSnapshot,
}

impl MetricsSnapshot {
    /// Render the snapshot in the Prometheus text exposition format.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::new();
        for series in self.object_store.series() {
            series.render(&mut out);
        }
        render_histogram(
            &mut out,
            "sana_object_store_request_seconds",
            "Wall-clock latency of backend object-store requests.",
            &[(None, &self.object_store.request_latency)],
        );
        for series in self.cache.series() {
            series.render(&mut out);
        }
        self.latency.render(&mut out);
        for series in self.search.series() {
            series.render(&mut out);
        }
        for series in self.queue.series() {
            series.render(&mut out);
        }
        self.queue.render_histograms(&mut out);
        for series in self.maintenance.series() {
            series.render(&mut out);
        }
        for series in self.worker.series() {
            series.render(&mut out);
        }
        self.render_index_lag(&mut out);
        out
    }

    fn render_index_lag(&self, out: &mut String) {
        use std::fmt::Write;
        // Namespace names are validated to [A-Za-z0-9-_.], so label values
        // need no escaping.
        type LagValue = fn(&IndexLagSample) -> u64;
        let families: [(&str, &str, LagValue); 2] = [
            (
                "sana_namespace_unindexed_bytes",
                "Committed WAL bytes not yet absorbed by the index, per namespace.",
                |sample| sample.unindexed_bytes,
            ),
            (
                "sana_namespace_unindexed_batches",
                "Committed WAL batches not yet absorbed by the index, per namespace.",
                |sample| sample.unindexed_batches,
            ),
        ];
        for (name, help, value) in families {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} gauge");
            for (namespace, sample) in &self.index_lag {
                let _ = writeln!(out, "{name}{{namespace=\"{namespace}\"}} {}", value(sample));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ObjectStoreSnapshot {
    pub gets: u64,
    pub get_ranges: u64,
    pub puts: u64,
    pub puts_if_absent: u64,
    pub compare_and_sets: u64,
    pub cas_mismatches: u64,
    pub lists: u64,
    pub deletes: u64,
    pub get_bytes: u64,
    pub range_bytes: u64,
    pub put_bytes: u64,
    pub request_latency: HistogramSnapshot,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LatencySnapshot {
    pub write_total: HistogramSnapshot,
    pub write_plan: HistogramSnapshot,
    pub write_commit: HistogramSnapshot,
    pub write_notify: HistogramSnapshot,
    pub query_total: HistogramSnapshot,
    pub query_plan: HistogramSnapshot,
    pub query_candidates: HistogramSnapshot,
    pub query_overlay: HistogramSnapshot,
    pub query_rank: HistogramSnapshot,
    pub query_materialize: HistogramSnapshot,
}

impl LatencySnapshot {
    fn render(&self, out: &mut String) {
        render_histogram(
            out,
            "sana_write_seconds",
            "End-to-end write request latency.",
            &[(None, &self.write_total)],
        );
        render_histogram(
            out,
            "sana_write_phase_seconds",
            "Write latency at the dominant phases; phases need not sum to the total.",
            &[
                (Some("plan"), &self.write_plan),
                (Some("commit"), &self.write_commit),
                (Some("notify"), &self.write_notify),
            ],
        );
        render_histogram(
            out,
            "sana_query_seconds",
            "End-to-end query request latency.",
            &[(None, &self.query_total)],
        );
        render_histogram(
            out,
            "sana_query_phase_seconds",
            "Query latency at the dominant phases; phases need not sum to the total.",
            &[
                (Some("plan"), &self.query_plan),
                (Some("candidates"), &self.query_candidates),
                (Some("overlay"), &self.query_overlay),
                (Some("rank"), &self.query_rank),
                (Some("materialize"), &self.query_materialize),
            ],
        );
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HistogramSnapshot {
    pub buckets: [u64; BUCKETS],
    pub sum_micros: u64,
}

impl HistogramSnapshot {
    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }
}

/// Fixed-bucket integer histogram for non-latency observations.
#[derive(Debug, Default)]
pub struct ValueHistogram {
    buckets: [AtomicU64; VALUE_BUCKETS],
    sum: AtomicU64,
}

impl ValueHistogram {
    pub fn observe(&self, value: u64) {
        let slot = BATCH_SIZE_BOUNDS
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(BATCH_SIZE_BOUNDS.len());
        if let Some(bucket) = self.buckets.get(slot) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.sum.fetch_add(value, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ValueHistogramSnapshot {
        let mut buckets = [0u64; VALUE_BUCKETS];
        for (bucket, source) in buckets.iter_mut().zip(&self.buckets) {
            *bucket = source.load(Ordering::Relaxed);
        }
        ValueHistogramSnapshot {
            buckets,
            sum: self.sum.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ValueHistogramSnapshot {
    pub buckets: [u64; VALUE_BUCKETS],
    pub sum: u64,
}

impl ValueHistogramSnapshot {
    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }
}

struct Series {
    name: &'static str,
    help: &'static str,
    kind: &'static str,
    value: u64,
}

impl Series {
    fn new(name: &'static str, help: &'static str, value: u64) -> Self {
        Self {
            name,
            help,
            kind: "counter",
            value,
        }
    }

    fn gauge(name: &'static str, help: &'static str, value: u64) -> Self {
        Self {
            name,
            help,
            kind: "gauge",
            value,
        }
    }

    fn render(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help);
        let _ = writeln!(out, "# TYPE {} {}", self.name, self.kind);
        let _ = writeln!(out, "{} {}", self.name, self.value);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CacheSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub bypasses: u64,
    pub evictions: u64,
    pub admission_rejections: u64,
    pub capacity_bytes: u64,
    pub resident_bytes: u64,
    pub entries: u64,
}

impl CacheSnapshot {
    fn series(&self) -> [Series; 8] {
        [
            Series::new(
                "sana_cache_hits_total",
                "Immutable-object cache hits.",
                self.hits,
            ),
            Series::new(
                "sana_cache_misses_total",
                "Immutable-object cache misses.",
                self.misses,
            ),
            Series::new(
                "sana_cache_bypasses_total",
                "Reads of mutable keys that bypass the cache.",
                self.bypasses,
            ),
            Series::new(
                "sana_cache_evictions_total",
                "Entries evicted to fit the byte capacity.",
                self.evictions,
            ),
            Series::new(
                "sana_cache_admission_rejections_total",
                "Objects too large to ever fit the cache.",
                self.admission_rejections,
            ),
            Series::gauge(
                "sana_cache_capacity_bytes",
                "Configured cache byte capacity.",
                self.capacity_bytes,
            ),
            Series::gauge(
                "sana_cache_resident_bytes",
                "Bytes currently resident in the cache.",
                self.resident_bytes,
            ),
            Series::gauge(
                "sana_cache_entries",
                "Objects currently resident in the cache.",
                self.entries,
            ),
        ]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SearchSnapshot {
    pub ann_queries: u64,
    pub ann_candidates: u64,
    pub ann_estimated: u64,
    pub ann_reranked: u64,
    pub ann_pruned: u64,
    pub text_queries: u64,
    pub text_blocks_read: u64,
    pub text_blocks_skipped: u64,
}

impl SearchSnapshot {
    fn series(&self) -> [Series; 8] {
        [
            Series::new(
                "sana_search_ann_queries_total",
                "ANN queries served from published vector segments.",
                self.ann_queries,
            ),
            Series::new(
                "sana_search_ann_candidates_total",
                "Posting candidates returned by ANN segment scans.",
                self.ann_candidates,
            ),
            Series::new(
                "sana_search_ann_estimated_total",
                "RaBitQ code estimations performed.",
                self.ann_estimated,
            ),
            Series::new(
                "sana_search_ann_reranked_total",
                "ANN candidates exact-reranked after pruning.",
                self.ann_reranked,
            ),
            Series::new(
                "sana_search_ann_pruned_total",
                "ANN candidates eliminated by the RaBitQ lower bound.",
                self.ann_pruned,
            ),
            Series::new(
                "sana_search_text_queries_total",
                "Full-text queries executed.",
                self.text_queries,
            ),
            Series::new(
                "sana_search_text_blocks_read_total",
                "Text posting blocks decoded and scored.",
                self.text_blocks_read,
            ),
            Series::new(
                "sana_search_text_blocks_skipped_total",
                "Text posting blocks skipped by block MAXSCORE.",
                self.text_blocks_skipped,
            ),
        ]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct QueueSnapshot {
    pub cas_attempts: u64,
    pub cas_successes: u64,
    pub cas_retries: u64,
    pub broker_batches: u64,
    pub broker_batch_requests: u64,
    pub jobs: u64,
    pub available_jobs: u64,
    pub claimed_jobs: u64,
    pub oldest_job_age_seconds: u64,
    pub broker_batch_size: ValueHistogramSnapshot,
    pub claim_wait: HistogramSnapshot,
}

impl QueueSnapshot {
    fn series(&self) -> [Series; 9] {
        [
            Series::new(
                "sana_index_queue_cas_attempts_total",
                "Compare-and-set attempts against the indexing queue object.",
                self.cas_attempts,
            ),
            Series::new(
                "sana_index_queue_cas_successes_total",
                "Successful compare-and-set writes against the indexing queue object.",
                self.cas_successes,
            ),
            Series::new(
                "sana_index_queue_cas_retries_total",
                "Indexing queue compare-and-set attempts rejected by a version mismatch.",
                self.cas_retries,
            ),
            Series::new(
                "sana_index_queue_broker_batches_total",
                "Group-commit batches attempted by the indexing queue broker.",
                self.broker_batches,
            ),
            Series::new(
                "sana_index_queue_broker_batch_requests_total",
                "Queue mutation requests included in broker group-commit batches.",
                self.broker_batch_requests,
            ),
            Series::gauge(
                "sana_index_queue_jobs",
                "Live jobs in the indexing queue.",
                self.jobs,
            ),
            Series::gauge(
                "sana_index_queue_available_jobs",
                "Live indexing queue jobs immediately eligible for claim.",
                self.available_jobs,
            ),
            Series::gauge(
                "sana_index_queue_claimed_jobs",
                "Live indexing queue jobs with an unexpired claim lease.",
                self.claimed_jobs,
            ),
            Series::gauge(
                "sana_index_queue_oldest_job_age_seconds",
                "Age of the oldest live indexing queue job.",
                self.oldest_job_age_seconds,
            ),
        ]
    }

    fn render_histograms(&self, out: &mut String) {
        render_value_histogram(
            out,
            "sana_index_queue_broker_batch_size",
            "Queue mutation requests per broker group-commit batch.",
            &self.broker_batch_size,
        );
        render_histogram(
            out,
            "sana_index_queue_claim_wait_seconds",
            "Time from indexing job creation to successful claim.",
            &[(None, &self.claim_wait)],
        );
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MaintenanceSnapshot {
    pub passes: u64,
    pub skipped_leased_passes: u64,
    pub compactions: u64,
    pub vector_maintenance: u64,
    pub gc_candidates: u64,
    pub gc_deletions: u64,
    pub errors: u64,
}

impl MaintenanceSnapshot {
    fn series(&self) -> [Series; 7] {
        [
            Series::new(
                "sana_maintenance_passes_total",
                "Maintenance passes run by the lease-holding leader.",
                self.passes,
            ),
            Series::new(
                "sana_maintenance_skipped_leased_passes_total",
                "Maintenance passes skipped because another process held the leader lease.",
                self.skipped_leased_passes,
            ),
            Series::new(
                "sana_maintenance_compactions_total",
                "Namespace compactions published by maintenance passes.",
                self.compactions,
            ),
            Series::new(
                "sana_maintenance_vector_maintenance_total",
                "Vector split/merge/reassign publications by maintenance passes.",
                self.vector_maintenance,
            ),
            Series::new(
                "sana_maintenance_gc_candidates_total",
                "Fresh orphan objects observed by maintenance GC, deletable next pass.",
                self.gc_candidates,
            ),
            Series::new(
                "sana_maintenance_gc_deletions_total",
                "Orphan objects deleted by maintenance GC.",
                self.gc_deletions,
            ),
            Series::new(
                "sana_maintenance_errors_total",
                "Per-namespace and whole-pass maintenance failures.",
                self.errors,
            ),
        ]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WorkerSnapshot {
    pub claims: u64,
    pub flushes: u64,
    pub failures: u64,
    pub stale_claim_rejections: u64,
}

impl WorkerSnapshot {
    fn series(&self) -> [Series; 4] {
        [
            Series::new(
                "sana_index_worker_claims_total",
                "Indexing jobs engaged by the worker (claimed and run to a terminal outcome).",
                self.claims,
            ),
            Series::new(
                "sana_index_worker_flushes_total",
                "Claimed indexing jobs that published a new index.",
                self.flushes,
            ),
            Series::new(
                "sana_index_worker_failures_total",
                "Indexing jobs returned to the queue for retry after a worker failure.",
                self.failures,
            ),
            Series::new(
                "sana_index_worker_stale_claim_rejections_total",
                "Worker claims fenced at publish or heartbeat because they went stale.",
                self.stale_claim_rejections,
            ),
        ]
    }
}

impl ObjectStoreSnapshot {
    fn series(&self) -> [Series; 11] {
        [
            Series::new(
                "sana_object_store_gets_total",
                "Object-store get requests.",
                self.gets,
            ),
            Series::new(
                "sana_object_store_get_ranges_total",
                "Object-store ranged get requests.",
                self.get_ranges,
            ),
            Series::new(
                "sana_object_store_puts_total",
                "Object-store put requests.",
                self.puts,
            ),
            Series::new(
                "sana_object_store_puts_if_absent_total",
                "Object-store put-if-absent requests.",
                self.puts_if_absent,
            ),
            Series::new(
                "sana_object_store_compare_and_sets_total",
                "Object-store compare-and-set requests.",
                self.compare_and_sets,
            ),
            Series::new(
                "sana_object_store_cas_mismatches_total",
                "Compare-and-set requests rejected by a version mismatch.",
                self.cas_mismatches,
            ),
            Series::new(
                "sana_object_store_lists_total",
                "Object-store list requests.",
                self.lists,
            ),
            Series::new(
                "sana_object_store_deletes_total",
                "Object-store delete requests.",
                self.deletes,
            ),
            Series::new(
                "sana_object_store_get_bytes_total",
                "Bytes returned by object-store gets.",
                self.get_bytes,
            ),
            Series::new(
                "sana_object_store_range_bytes_total",
                "Bytes returned by object-store ranged gets.",
                self.range_bytes,
            ),
            Series::new(
                "sana_object_store_put_bytes_total",
                "Bytes written by object-store put-family requests.",
                self.put_bytes,
            ),
        ]
    }
}

/// One Prometheus histogram family. Each entry is a series; `Some(phase)`
/// becomes a `phase="..."` label, `None` renders unlabeled.
fn render_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    series: &[(Option<&str>, &HistogramSnapshot)],
) {
    use std::fmt::Write;
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");
    for (phase, histogram) in series {
        let bucket_prefix = phase.map_or(String::new(), |phase| format!("phase=\"{phase}\","));
        let suffix_labels = phase.map_or(String::new(), |phase| format!("{{phase=\"{phase}\"}}"));
        let mut cumulative = 0u64;
        for (count, bound) in histogram.buckets.iter().zip(BUCKET_BOUNDS_US.iter()) {
            cumulative += *count;
            let _ = writeln!(
                out,
                "{name}_bucket{{{bucket_prefix}le=\"{}\"}} {cumulative}",
                seconds(*bound)
            );
        }
        cumulative += histogram.buckets.last().copied().unwrap_or(0);
        let _ = writeln!(
            out,
            "{name}_bucket{{{bucket_prefix}le=\"+Inf\"}} {cumulative}"
        );
        let _ = writeln!(
            out,
            "{name}_sum{suffix_labels} {}",
            seconds(histogram.sum_micros)
        );
        let _ = writeln!(out, "{name}_count{suffix_labels} {cumulative}");
    }
}

fn render_value_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    histogram: &ValueHistogramSnapshot,
) {
    use std::fmt::Write;
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");
    let mut cumulative = 0u64;
    for (count, bound) in histogram.buckets.iter().zip(BATCH_SIZE_BOUNDS.iter()) {
        cumulative += *count;
        let _ = writeln!(out, "{name}_bucket{{le=\"{bound}\"}} {cumulative}");
    }
    cumulative += histogram.buckets.last().copied().unwrap_or(0);
    let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {cumulative}");
    let _ = writeln!(out, "{name}_sum {}", histogram.sum);
    let _ = writeln!(out, "{name}_count {cumulative}");
}

fn seconds(micros: u64) -> f64 {
    micros as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_increments() {
        let metrics = Metrics::shared();
        incr(&metrics.object_store.gets);
        incr(&metrics.object_store.gets);
        add(&metrics.object_store.get_bytes, 4096);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.object_store.gets, 2);
        assert_eq!(snapshot.object_store.get_bytes, 4096);
        assert_eq!(snapshot.object_store.puts, 0);
    }

    #[test]
    fn prometheus_render_has_help_type_and_value() {
        let mut metrics = Metrics::default();
        *metrics.object_store.cas_mismatches.get_mut() = 7;
        let text = metrics.snapshot().to_prometheus();

        assert!(text.contains("# HELP sana_object_store_cas_mismatches_total"));
        assert!(text.contains("# TYPE sana_object_store_cas_mismatches_total counter"));
        assert!(text.contains("\nsana_object_store_cas_mismatches_total 7\n"));
        assert!(text.contains("# HELP sana_index_queue_jobs"));
        assert!(text.contains("# TYPE sana_index_queue_broker_batch_size histogram"));
        // Every family is emitted, even at zero.
        assert_eq!(text.matches(" counter\n").count(), 40);
        assert_eq!(text.matches(" gauge\n").count(), 9);
        assert_eq!(text.matches(" histogram\n").count(), 7);
    }

    #[test]
    fn maintenance_metrics_fold_pass_reports() {
        let metrics = Metrics::default();
        metrics.maintenance.record_pass(MaintenancePassSample {
            compactions: 2,
            vector_maintenance: 1,
            gc_candidates: 5,
            gc_deletions: 3,
            errors: 1,
        });
        metrics.maintenance.record_skipped_pass();
        metrics.maintenance.record_failed_pass();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.maintenance.passes, 1);
        assert_eq!(snapshot.maintenance.skipped_leased_passes, 1);
        assert_eq!(snapshot.maintenance.compactions, 2);
        assert_eq!(snapshot.maintenance.vector_maintenance, 1);
        assert_eq!(snapshot.maintenance.gc_candidates, 5);
        assert_eq!(snapshot.maintenance.gc_deletions, 3);
        // One per-namespace error from the pass plus one failed pass.
        assert_eq!(snapshot.maintenance.errors, 2);

        let text = snapshot.to_prometheus();
        assert!(text.contains("# TYPE sana_maintenance_passes_total counter"));
        assert!(text.contains("\nsana_maintenance_compactions_total 2\n"));
        assert!(text.contains("\nsana_maintenance_gc_deletions_total 3\n"));
        assert!(text.contains("\nsana_maintenance_errors_total 2\n"));
    }

    #[test]
    fn worker_metrics_count_outcomes() {
        let metrics = Metrics::default();
        incr(&metrics.worker.claims);
        incr(&metrics.worker.claims);
        incr(&metrics.worker.flushes);
        incr(&metrics.worker.failures);
        incr(&metrics.worker.stale_claim_rejections);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.worker.claims, 2);
        assert_eq!(snapshot.worker.flushes, 1);
        assert_eq!(snapshot.worker.failures, 1);
        assert_eq!(snapshot.worker.stale_claim_rejections, 1);

        let text = snapshot.to_prometheus();
        assert!(text.contains("\nsana_index_worker_claims_total 2\n"));
        assert!(text.contains("\nsana_index_worker_flushes_total 1\n"));
        assert!(text.contains("\nsana_index_worker_stale_claim_rejections_total 1\n"));
    }

    #[test]
    fn index_lag_renders_labeled_gauges_and_replaces_wholesale() {
        let metrics = Metrics::default();
        metrics.index_lag.record(BTreeMap::from([
            (
                "docs".to_string(),
                IndexLagSample {
                    unindexed_bytes: 42,
                    unindexed_batches: 2,
                },
            ),
            ("empty".to_string(), IndexLagSample::default()),
        ]));
        let text = metrics.snapshot().to_prometheus();
        assert!(text.contains("# TYPE sana_namespace_unindexed_bytes gauge"));
        assert!(text.contains("sana_namespace_unindexed_bytes{namespace=\"docs\"} 42\n"));
        assert!(text.contains("sana_namespace_unindexed_batches{namespace=\"docs\"} 2\n"));
        assert!(text.contains("sana_namespace_unindexed_bytes{namespace=\"empty\"} 0\n"));

        // A later scan without "docs" drops its series from the scrape.
        metrics.index_lag.record(BTreeMap::new());
        let text = metrics.snapshot().to_prometheus();
        assert!(!text.contains("namespace=\"docs\""));
    }

    #[test]
    fn histogram_places_observations_and_sums_micros() {
        let histogram = Histogram::default();
        histogram.observe(Duration::from_micros(50)); // first bucket (<=100µs)
        histogram.observe(Duration::from_micros(150)); // second bucket (<=200µs)
        histogram.observe(Duration::from_secs(60)); // beyond the last bound: +Inf

        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.buckets[0], 1);
        assert_eq!(snapshot.buckets[1], 1);
        assert_eq!(snapshot.buckets[BUCKETS - 1], 1);
        assert_eq!(snapshot.count(), 3);
        assert_eq!(snapshot.sum_micros, 50 + 150 + 60_000_000);
    }

    #[tokio::test]
    async fn histogram_time_records_errors_too() {
        let metrics = Metrics::default();
        let failed: Result<(), &str> = metrics
            .latency
            .query_total
            .time(async { Err("query failure") })
            .await;
        assert!(failed.is_err());
        assert_eq!(metrics.latency.query_total.snapshot().count(), 1);
    }

    #[test]
    fn queue_metrics_render_state_counters_and_histograms() {
        let metrics = Metrics::default();
        metrics.queue.record_state(QueueStateSample {
            jobs: 3,
            available_jobs: 2,
            claimed_jobs: 1,
            oldest_job_age_seconds: 9,
        });
        incr(&metrics.queue.cas_attempts);
        incr(&metrics.queue.cas_successes);
        incr(&metrics.queue.cas_retries);
        metrics.queue.record_broker_batch(17);
        metrics
            .queue
            .record_claim_wait(Duration::from_millis(1_500));

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.queue.jobs, 3);
        assert_eq!(snapshot.queue.broker_batch_size.count(), 1);
        assert_eq!(snapshot.queue.claim_wait.count(), 1);

        let text = snapshot.to_prometheus();
        assert!(text.contains("sana_index_queue_jobs 3\n"));
        assert!(text.contains("sana_index_queue_available_jobs 2\n"));
        assert!(text.contains("sana_index_queue_claimed_jobs 1\n"));
        assert!(text.contains("sana_index_queue_oldest_job_age_seconds 9\n"));
        assert!(text.contains("sana_index_queue_cas_retries_total 1\n"));
        assert!(text.contains("sana_index_queue_broker_batch_size_bucket{le=\"32\"} 1\n"));
        assert!(text.contains("sana_index_queue_claim_wait_seconds_count 1\n"));
    }

    #[test]
    fn prometheus_histogram_render_is_cumulative() {
        let metrics = Metrics::default();
        metrics
            .latency
            .query_plan
            .observe(Duration::from_micros(50));
        metrics
            .latency
            .query_plan
            .observe(Duration::from_micros(150));
        let text = metrics.snapshot().to_prometheus();

        assert!(text.contains("# TYPE sana_query_phase_seconds histogram"));
        assert!(text.contains("sana_query_phase_seconds_bucket{phase=\"plan\",le=\"0.0001\"} 1\n"));
        assert!(text.contains("sana_query_phase_seconds_bucket{phase=\"plan\",le=\"0.0002\"} 2\n"));
        assert!(text.contains("sana_query_phase_seconds_bucket{phase=\"plan\",le=\"+Inf\"} 2\n"));
        assert!(text.contains("sana_query_phase_seconds_sum{phase=\"plan\"} 0.0002\n"));
        assert!(text.contains("sana_query_phase_seconds_count{phase=\"plan\"} 2\n"));
        // Unlabeled totals render without braces.
        assert!(text.contains("sana_query_seconds_bucket{le=\"+Inf\"} 0\n"));
        assert!(text.contains("sana_query_seconds_count 0\n"));
    }
}
