//! In-process metrics registry. Dependency-free: counters and histogram
//! buckets are plain atomics and the only exposition format is Prometheus
//! text, rendered by hand.
//!
//! The registry is shared as `Arc<Metrics>`. The metered object-store decorator
//! increments it on the I/O path, namespace write/query paths record phase
//! latencies, and the `/metrics` endpoint renders a snapshot. Reads are
//! `Relaxed`: counters are independent and a metrics scrape never needs a
//! consistent cross-counter view.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

#[derive(Debug, Default)]
pub struct Metrics {
    pub object_store: ObjectStoreMetrics,
    pub latency: LatencyMetrics,
}

impl Metrics {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            object_store: self.object_store.snapshot(),
            latency: self.latency.snapshot(),
        }
    }
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
    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(counter: &AtomicU64, amount: u64) {
        counter.fetch_add(amount, Ordering::Relaxed);
    }

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
        self.buckets[slot].fetch_add(1, Ordering::Relaxed);
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
        for (slot, bucket) in buckets.iter_mut().enumerate() {
            *bucket = self.buckets[slot].load(Ordering::Relaxed);
        }
        HistogramSnapshot {
            buckets,
            sum_micros: self.sum_micros.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MetricsSnapshot {
    pub object_store: ObjectStoreSnapshot,
    pub latency: LatencySnapshot,
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
        self.latency.render(&mut out);
        out
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

struct Series {
    name: &'static str,
    help: &'static str,
    value: u64,
}

impl Series {
    fn new(name: &'static str, help: &'static str, value: u64) -> Self {
        Self { name, help, value }
    }

    fn render(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help);
        let _ = writeln!(out, "# TYPE {} counter", self.name);
        let _ = writeln!(out, "{} {}", self.name, self.value);
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
        for (slot, bound) in BUCKET_BOUNDS_US.iter().enumerate() {
            cumulative += histogram.buckets[slot];
            let _ = writeln!(
                out,
                "{name}_bucket{{{bucket_prefix}le=\"{}\"}} {cumulative}",
                seconds(*bound)
            );
        }
        cumulative += histogram.buckets[BUCKETS - 1];
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

fn seconds(micros: u64) -> f64 {
    micros as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_increments() {
        let metrics = Metrics::shared();
        ObjectStoreMetrics::incr(&metrics.object_store.gets);
        ObjectStoreMetrics::incr(&metrics.object_store.gets);
        ObjectStoreMetrics::add(&metrics.object_store.get_bytes, 4096);

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
        // Every family is emitted, even at zero.
        assert_eq!(text.matches(" counter\n").count(), 11);
        assert_eq!(text.matches(" histogram\n").count(), 5);
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
