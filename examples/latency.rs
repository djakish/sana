//! End-to-end latency harness over the filesystem object store.
//!
//! Runs the same decorator stack as `sana serve` — `Caching(Metered(Fs))` — and
//! measures the write, flush, lookup, and query paths, then prints the
//! object-store traffic the run actually generated.
//!
//!   cargo run --release --example latency
//!   cargo run --release --example latency -- <dir> <writes> <dim> <queries>
//!
//! With no <dir> it uses a fresh temp directory. Numbers are local-disk and not
//! comparable to S3; they exist to track regressions and exercise `/metrics`.

use std::sync::Arc;
use std::time::Instant;

use sana::indexer;
use sana::metrics::Metrics;
use sana::object_store::{
    CachingObjectStore, FsObjectStore, MeteredObjectStore, ObjectStore, S3Config, S3ObjectStore,
};
use sana::query::{ApproxVectorQuery, FilterExpr, Query};
use sana::value::{Document, Id, Value, VectorValue};
use sana::wal::WalOp;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let writes: u64 = arg(&args, 2).unwrap_or(5_000);
    let dim: usize = arg(&args, 3).unwrap_or(64);
    let queries: u64 = arg(&args, 4).unwrap_or(1_000);
    let batch: u64 = arg(&args, 5).unwrap_or(100);

    let _temp = tempfile::tempdir().expect("temp dir");
    let dir = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| _temp.path().to_string_lossy().into_owned());

    let metrics = Metrics::shared();
    let backing: Arc<dyn ObjectStore> = if dir.starts_with("s3://") {
        let config = S3Config::from_location(&dir).expect("valid s3:// location");
        Arc::new(S3ObjectStore::from_env(config).expect("s3 store from environment"))
    } else {
        Arc::new(FsObjectStore::new(&dir))
    };
    let metered: Arc<dyn ObjectStore> = Arc::new(MeteredObjectStore::new(backing, metrics.clone()));
    let store: Arc<dyn ObjectStore> =
        Arc::new(CachingObjectStore::new(metered, 256 * 1024 * 1024).with_metrics(metrics.clone()));

    println!("sana latency harness");
    println!("  store={dir}");
    println!("  writes={writes} dim={dim} queries={queries}\n");

    let ns = sana::namespace::Namespace::create_or_open(store.clone(), "bench")
        .await
        .expect("create namespace")
        .with_metrics(metrics.clone());

    let mut write_us = Vec::with_capacity(writes as usize);
    let mut rng = Rng::new(0x5a5a_5eed_d00d_0001);
    for i in 0..writes {
        let document = sample_document(i, dim, &mut rng);
        let start = Instant::now();
        ns.upsert(document).await.expect("upsert");
        write_us.push(start.elapsed().as_micros());
    }
    report("write (1 doc / WAL commit)", &write_us);

    let mut batch_us = Vec::new();
    let mut next_id = writes;
    let batch_start = Instant::now();
    while next_id < writes * 2 {
        let n = batch.min(writes * 2 - next_id);
        let ops: Vec<WalOp> = (0..n)
            .map(|j| WalOp::Upsert {
                id: Id::U64(next_id + j),
                document: sample_document(next_id + j, dim, &mut rng),
            })
            .collect();
        let start = Instant::now();
        ns.append(ops, None).await.expect("append batch");
        batch_us.push(start.elapsed().as_micros());
        next_id += n;
    }
    report(
        &format!("\nbatched write ({batch} docs / WAL commit)"),
        &batch_us,
    );
    println!(
        "  amortized: {:.3} ms/doc  ({:.0} docs/s) \u{2014} vs {:.3} ms/doc single",
        batch_start.elapsed().as_secs_f64() * 1000.0 / writes as f64,
        writes as f64 / batch_start.elapsed().as_secs_f64(),
        write_us.iter().sum::<u128>() as f64 / 1000.0 / writes as f64
    );

    let start = Instant::now();
    let flushed = indexer::flush(&ns).await.expect("flush");
    println!(
        "\nflush (index {} docs): {:.1} ms  (work_done={flushed})",
        writes * 2,
        start.elapsed().as_secs_f64() * 1000.0
    );

    let mut lookup_us = Vec::with_capacity(queries as usize);
    for _ in 0..queries {
        let id = Id::U64(rng.next() % writes);
        let start = Instant::now();
        ns.lookup(&id).await.expect("lookup");
        lookup_us.push(start.elapsed().as_micros());
    }
    report("\npoint lookup", &lookup_us);

    let mut ann_us = Vec::with_capacity(queries as usize);
    for _ in 0..queries {
        let query = Query {
            approx_vector: Some(ApproxVectorQuery {
                column: "embedding".into(),
                vector: random_vector(dim, &mut rng),
                k: 10,
                probes: None,
                metric: None,
            }),
            ..Query::all()
        };
        let start = Instant::now();
        ns.query(query).await.expect("ann query");
        ann_us.push(start.elapsed().as_micros());
    }
    report("\nANN vector query (k=10)", &ann_us);

    let mut filter_us = Vec::with_capacity(queries as usize);
    for _ in 0..queries {
        let query = Query {
            filter: Some(FilterExpr::Eq {
                column: "bucket".into(),
                value: Value::Int((rng.next() % 10) as i64),
            }),
            limit: Some(10),
            ..Query::all()
        };
        let start = Instant::now();
        ns.query(query).await.expect("filter query");
        filter_us.push(start.elapsed().as_micros());
    }
    report("\nfilter query (eq, limit 10)", &filter_us);

    let os = metrics.snapshot().object_store;
    println!("\nobject-store traffic (true backend round trips, below the cache):");
    println!(
        "  gets={}  get_ranges={}  lists={}",
        os.gets, os.get_ranges, os.lists
    );
    println!(
        "  puts={}  puts_if_absent={}  compare_and_sets={}  cas_mismatches={}",
        os.puts, os.puts_if_absent, os.compare_and_sets, os.cas_mismatches
    );
    println!(
        "  get_bytes={:.1} MiB  put_bytes={:.1} MiB",
        os.get_bytes as f64 / (1024.0 * 1024.0),
        os.put_bytes as f64 / (1024.0 * 1024.0)
    );
}

fn report(label: &str, samples_us: &[u128]) {
    if samples_us.is_empty() {
        println!("{label}: no samples");
        return;
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let total: u128 = sorted.iter().sum();
    let pct = |p: f64| sorted[((p * (n - 1) as f64).round() as usize).min(n - 1)];
    let ms = |us: u128| us as f64 / 1000.0;
    let seconds = total as f64 / 1_000_000.0;
    println!("{label}: n={n}");
    println!(
        "  p50={:.3}ms  p90={:.3}ms  p99={:.3}ms  max={:.3}ms  mean={:.3}ms  {:.0} ops/s",
        ms(pct(0.50)),
        ms(pct(0.90)),
        ms(pct(0.99)),
        ms(sorted[n - 1]),
        ms(total / n as u128),
        n as f64 / seconds
    );
}

fn sample_document(i: u64, dim: usize, rng: &mut Rng) -> Document {
    let mut document = Document::new(Id::U64(i));
    document
        .attributes
        .insert("bucket".into(), Value::Int((i % 10) as i64));
    document
        .attributes
        .insert("title".into(), Value::String(format!("doc-{i}")));
    document.vectors.insert(
        "embedding".into(),
        VectorValue::F32(random_vector(dim, rng)),
    );
    document
}

fn random_vector(dim: usize, rng: &mut Rng) -> Vec<f32> {
    (0..dim)
        .map(|_| (rng.next() % 2_000) as f32 / 1_000.0 - 1.0)
        .collect()
}

fn arg<T: std::str::FromStr>(args: &[String], i: usize) -> Option<T> {
    args.get(i).and_then(|value| value.parse().ok())
}

/// Tiny xorshift64* so the harness needs no `rand` dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}
