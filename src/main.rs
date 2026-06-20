//! Minimal Stage 1 CLI over a filesystem-backed namespace. This is a thin
//! harness to exercise the engine end to end; the real API surface (HTTP)
//! arrives later in the build plan.

use std::sync::Arc;

use sana::metrics::Metrics;
use sana::namespace::Namespace;
use sana::object_store::{
    CachingObjectStore, FsObjectStore, MeteredObjectStore, ObjectStore, S3Config, S3ObjectStore,
};
use sana::query::{MultiQuery, Query, RecallRequest};
use sana::value::{Document, Id, Value};

type CliResult = Result<(), Box<dyn std::error::Error>>;

const INDEX_LEASE_MS: u64 = 30_000;
const INDEX_RETRY_MS: u64 = 1_000;
const INDEX_IDLE_MS: u64 = 100;
const INDEX_RECONCILE_MS: u64 = 30_000;
const MAINTENANCE_INTERVAL_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServeRole {
    All,
    Api,
}

#[tokio::main]
async fn main() -> CliResult {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str);

    match cmd {
        Some("create") => create(&args).await,
        Some("upsert") => upsert(&args).await,
        Some("get") => get(&args).await,
        Some("delete") => delete(&args).await,
        Some("list") => list(&args).await,
        Some("query") => query(&args).await,
        Some("multi-query") => multi_query(&args).await,
        Some("recall") => recall(&args).await,
        Some("flush") => flush(&args).await,
        Some("compact") => compact(&args).await,
        Some("gc") => gc(&args).await,
        Some("maintain-vectors") => maintain_vectors(&args).await,
        Some("maintain") => maintain(&args).await,
        Some("reconcile-indexing") => reconcile_indexing(&args).await,
        Some("work-indexing") => work_indexing(&args).await,
        Some("branch") => branch(&args).await,
        Some("copy") => copy(&args).await,
        Some("export") => export(&args).await,
        Some("pin") => pin(&args).await,
        Some("unpin") => unpin(&args).await,
        Some("pin-status") => pin_status(&args).await,
        Some("serve") => Box::pin(serve(&args)).await,
        Some("serve-api") => Box::pin(serve_api(&args)).await,
        Some("demo") => demo(&args).await,
        _ => {
            usage();
            Ok(())
        }
    }
}

fn usage() {
    eprintln!(
        "sana {} — object-storage-native search database\n",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("usage:");
    eprintln!("  sana create <dir> <ns>");
    eprintln!("  sana upsert <dir> <ns> <id> [key=value ...]");
    eprintln!("  sana get    <dir> <ns> <id>");
    eprintln!("  sana delete <dir> <ns> <id>");
    eprintln!("  sana list    <dir> <ns>");
    eprintln!("  sana query   <dir> <ns> [json-query]");
    eprintln!("  sana multi-query <dir> <ns> <json-multi-query>");
    eprintln!("  sana recall  <dir> <ns> [json-recall-request]");
    eprintln!("  sana flush   <dir> <ns>   # fold WAL into a document SST");
    eprintln!("  sana compact <dir> <ns>   # merge SSTs, drop tombstones");
    eprintln!("  sana gc      <dir> <ns> [--apply]   # report (or delete) orphaned objects");
    eprintln!("  sana maintain-vectors <dir> <ns>   # run one vector maintenance pass");
    eprintln!("  sana maintain <dir> [--loop]   # run all-namespace maintenance");
    eprintln!("  sana reconcile-indexing <dir>   # restore missed indexing notifications");
    eprintln!("  sana work-indexing <dir> [worker-id] [--loop]   # run indexing worker");
    eprintln!("  sana branch <dir> <source-ns> <child-ns>   # zero-copy indexed snapshot");
    eprintln!("  sana copy <source-dir> <source-ns> <target-dir> <target-ns>");
    eprintln!("  sana export <source-dir> <ns> <target-dir> <prefix>");
    eprintln!("  sana pin <dir> <ns> [replicas]");
    eprintln!("  sana unpin <dir> <ns>");
    eprintln!("  sana pin-status <dir> <ns>");
    eprintln!("  sana serve <dir> [address] [cache-bytes] [--role all|api]");
    eprintln!("  sana serve-api <dir> [address] [cache-bytes]");
    eprintln!("  sana demo    <dir>");
    eprintln!();
    eprintln!("  <dir> may be a directory or s3://bucket[/prefix]; S3 reads");
    eprintln!("  SANA_S3_ENDPOINT, AWS_REGION, and AWS credentials from the env.");
}

/// Open the object-store backing for a CLI location: a filesystem directory,
/// or `s3://bucket[/prefix]` configured through the environment (endpoint,
/// region, and credentials; see `S3Config::from_location`).
fn store(location: &str) -> Result<Arc<dyn ObjectStore>, Box<dyn std::error::Error>> {
    if location.starts_with("s3://") {
        let config = S3Config::from_location(location)?;
        Ok(Arc::new(S3ObjectStore::from_env(config)?))
    } else {
        Ok(Arc::new(FsObjectStore::new(location)))
    }
}

/// Parse an id token as u64 if possible, else treat it as a string id.
fn parse_id(token: &str) -> Id {
    token
        .parse::<u64>()
        .map(Id::U64)
        .unwrap_or_else(|_| Id::String(token.to_string()))
}

/// Parse `value` into the narrowest matching type: int, float, bool, else string.
fn parse_value(token: &str) -> Value {
    if let Ok(i) = token.parse::<i64>() {
        Value::Int(i)
    } else if let Ok(f) = token.parse::<f64>() {
        Value::Float(f)
    } else if token == "true" || token == "false" {
        Value::Bool(token == "true")
    } else {
        Value::String(token.to_string())
    }
}

async fn create(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    Namespace::create(store(dir)?, ns).await?;
    println!("created namespace {ns}");
    Ok(())
}

async fn upsert(args: &[String]) -> CliResult {
    let (dir, ns, id) = (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?);
    let namespace = Namespace::create_or_open(store(dir)?, ns).await?;

    let mut doc = Document::new(parse_id(id));
    for pair in args.get(5..).unwrap_or_default() {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("expected key=value, got '{pair}'"))?;
        doc.attributes.insert(k.to_string(), parse_value(v));
    }
    let cursor = namespace.upsert(doc).await?;
    println!("upserted {id} at seq {}", cursor.seq);
    Ok(())
}

async fn get(args: &[String]) -> CliResult {
    let (dir, ns, id) = (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    match namespace.lookup(&parse_id(id)).await? {
        Some(doc) => println!("{doc:#?}"),
        None => println!("not found: {id}"),
    }
    Ok(())
}

async fn delete(args: &[String]) -> CliResult {
    let (dir, ns, id) = (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    namespace.delete(parse_id(id)).await?;
    println!("deleted {id}");
    Ok(())
}

async fn list(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    let docs = namespace.replay().await?;
    println!("{} document(s):", docs.len());
    for (id, doc) in &docs {
        println!("  {id:?} -> {} attr(s)", doc.attributes.len());
    }
    Ok(())
}

async fn query(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    let query = match args.get(4) {
        Some(json) => serde_json::from_str::<Query>(json)?,
        None => Query::all(),
    };
    let result = namespace.query(query).await?;
    println!("{} row(s):", result.rows.len());
    for row in &result.rows {
        match row.score {
            Some(score) => println!(
                "  {:?} score={score:.6} -> {} attr(s), {} vector(s)",
                row.id,
                row.document.attributes.len(),
                row.document.vectors.len()
            ),
            None => println!(
                "  {:?} -> {} attr(s), {} vector(s)",
                row.id,
                row.document.attributes.len(),
                row.document.vectors.len()
            ),
        }
    }
    if !result.aggregates.is_empty() {
        println!("aggregates: {:?}", result.aggregates);
    }
    Ok(())
}

async fn multi_query(args: &[String]) -> CliResult {
    let (dir, ns, json) = (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    let request = serde_json::from_str::<MultiQuery>(json)?;
    let result = namespace.multi_query(request).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn recall(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    let request = match args.get(4) {
        Some(json) => serde_json::from_str::<RecallRequest>(json)?,
        None => RecallRequest::default(),
    };
    let result = namespace.recall(request).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn flush(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    let did = sana::indexer::flush(&namespace).await?;
    println!(
        "{}",
        if did {
            "flushed WAL into a new SST"
        } else {
            "nothing to flush"
        }
    );
    Ok(())
}

async fn compact(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    let did = sana::indexer::compact(&namespace).await?;
    println!(
        "{}",
        if did {
            "compacted SSTs"
        } else {
            "nothing to compact"
        }
    );
    Ok(())
}

async fn gc(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let apply = args.iter().any(|a| a == "--apply");
    let namespace = Namespace::open(store(dir)?, ns).await?;
    let report = sana::indexer::gc(&namespace, apply).await?;
    let verb = if report.applied {
        "deleted"
    } else {
        "reclaimable"
    };
    println!(
        "{} {} orphaned object(s), {} bytes {verb} ({} live)",
        report.orphan_keys.len(),
        if report.applied { "" } else { "(dry run)" },
        report.orphan_bytes,
        report.live_count,
    );
    if !report.applied && !report.orphan_keys.is_empty() {
        for key in &report.orphan_keys {
            println!("  {key}");
        }
        println!("re-run with --apply to delete");
    }
    Ok(())
}

async fn maintain_vectors(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir)?, ns).await?;
    let did = sana::indexer::maintain_vectors(&namespace).await?;
    println!(
        "{}",
        if did {
            "published vector maintenance deltas"
        } else {
            "nothing to maintain"
        }
    );
    Ok(())
}

async fn maintain(args: &[String]) -> CliResult {
    let values = positional_args(args, &[], &["--loop"])?;
    let dir = values
        .first()
        .copied()
        .ok_or_else(|| "missing argument #2".to_string())?;
    let store = store(dir)?;
    let policy = sana::maintenance::MaintenancePolicy::default();
    let mut state = sana::maintenance::MaintenanceState::default();
    if has_flag(args, "--loop") {
        println!("running maintenance loop every {MAINTENANCE_INTERVAL_MS} ms; Ctrl-C to stop");
        let mut shutdown = shutdown_watcher();
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            run_maintenance_pass(store.clone(), &policy, &mut state).await?;
            if sleep_or_shutdown(
                &mut shutdown,
                std::time::Duration::from_millis(MAINTENANCE_INTERVAL_MS),
            )
            .await
            {
                return Ok(());
            }
        }
    } else {
        run_maintenance_pass(store, &policy, &mut state).await
    }
}

async fn run_maintenance_pass(
    store: Arc<dyn ObjectStore>,
    policy: &sana::maintenance::MaintenancePolicy,
    state: &mut sana::maintenance::MaintenanceState,
) -> CliResult {
    let report = sana::maintenance::run_once(store, policy, state).await?;
    println!(
        "maintenance scanned {} namespace(s): compacted {}, vector-maintained {}, gc-deleted {}, gc-pending {}, errors {}",
        report.scanned_namespaces,
        report.compacted.len(),
        report.vector_maintained.len(),
        report.gc_deleted_objects,
        report.gc_pending_objects,
        report.errors.len()
    );
    if !report.compacted.is_empty() {
        println!("  compacted: {}", report.compacted.join(", "));
    }
    if !report.vector_maintained.is_empty() {
        println!(
            "  vector-maintained: {}",
            report.vector_maintained.join(", ")
        );
    }
    for error in &report.errors {
        eprintln!("  {error}");
    }
    Ok(())
}

async fn work_indexing(args: &[String]) -> CliResult {
    let values = positional_args(args, &[], &["--loop"])?;
    let dir = values
        .first()
        .copied()
        .ok_or_else(|| "missing argument #2".to_string())?;
    let worker_id = values
        .get(1)
        .map(|value| (*value).to_string())
        .unwrap_or_else(|| format!("cli-indexer-{}", std::process::id()));
    let store = store(dir)?;
    if has_flag(args, "--loop") {
        run_indexing_loop(store, worker_id).await
    } else {
        run_indexing_once(store, &worker_id).await
    }
}

async fn run_indexing_once(store: Arc<dyn ObjectStore>, worker_id: &str) -> CliResult {
    match sana::index_queue::run_worker_once(store, worker_id, INDEX_LEASE_MS, INDEX_RETRY_MS)
        .await?
    {
        Some(run) => println!(
            "completed job {} for {} through WAL seq {} ({})",
            run.job_id,
            run.namespace,
            run.target_cursor.seq,
            if run.did_flush {
                "index published"
            } else {
                "already indexed"
            }
        ),
        None => println!("no indexing jobs available"),
    }
    Ok(())
}

async fn run_indexing_loop(store: Arc<dyn ObjectStore>, worker_id: String) -> CliResult {
    let mut next_reconcile = tokio::time::Instant::now();
    let shutdown = shutdown_watcher();
    println!("running indexing worker {worker_id}; Ctrl-C to stop");
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        run_indexing_tick(store.clone(), &worker_id, &mut next_reconcile).await;
    }
}

async fn run_indexing_tick(
    store: Arc<dyn ObjectStore>,
    worker_id: &str,
    next_reconcile: &mut tokio::time::Instant,
) {
    if tokio::time::Instant::now() >= *next_reconcile {
        match sana::index_queue::reconcile_unindexed(store.clone()).await {
            Ok(report) => println!(
                "reconciled {} namespace(s): {} lagging, {} added, {} coalesced",
                report.scanned_namespaces,
                report.lagging_namespaces,
                report.notifications_added,
                report.notifications_coalesced
            ),
            Err(error) => eprintln!("index reconciliation failed: {error}"),
        }
        *next_reconcile =
            tokio::time::Instant::now() + std::time::Duration::from_millis(INDEX_RECONCILE_MS);
    }

    match sana::index_queue::run_worker_once(store, worker_id, INDEX_LEASE_MS, INDEX_RETRY_MS).await
    {
        Ok(Some(run)) => println!(
            "completed job {} for {} through WAL seq {} ({})",
            run.job_id,
            run.namespace,
            run.target_cursor.seq,
            if run.did_flush {
                "index published"
            } else {
                "already indexed"
            }
        ),
        Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(INDEX_IDLE_MS)).await,
        Err(error) => {
            eprintln!("index worker failed: {error}");
            tokio::time::sleep(std::time::Duration::from_millis(INDEX_RETRY_MS)).await;
        }
    }
}

async fn reconcile_indexing(args: &[String]) -> CliResult {
    let dir = arg(args, 2)?;
    let report = sana::index_queue::reconcile_unindexed(store(dir)?).await?;
    println!(
        "scanned {} namespace(s): {} lagging, {} notification(s) added, {} coalesced",
        report.scanned_namespaces,
        report.lagging_namespaces,
        report.notifications_added,
        report.notifications_coalesced
    );
    Ok(())
}

async fn branch(args: &[String]) -> CliResult {
    let (dir, source_name, child_name) = (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?);
    let source = Namespace::open(store(dir)?, source_name).await?;
    let child = source.branch(child_name).await?;
    let parent = child
        .load_manifest()
        .await?
        .branch_parent
        .expect("branch operation sets parent metadata");
    println!(
        "branched {source_name} generation {} to {child_name}",
        parent.generation
    );
    Ok(())
}

async fn copy(args: &[String]) -> CliResult {
    let (source_dir, source_name, target_dir, target_name) =
        (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?, arg(args, 5)?);
    let source = Namespace::open(store(source_dir)?, source_name).await?;
    let report = source.copy_to(store(target_dir)?, target_name).await?;
    println!(
        "copied {source_name} generation {} to {target_name}: {} object(s), {} bytes",
        report.source_generation, report.object_count, report.copied_bytes
    );
    Ok(())
}

async fn export(args: &[String]) -> CliResult {
    let (source_dir, namespace_name, target_dir, prefix) =
        (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?, arg(args, 5)?);
    let namespace = Namespace::open(store(source_dir)?, namespace_name).await?;
    let report = namespace.export_to(store(target_dir)?, prefix).await?;
    println!(
        "exported {namespace_name} generation {} to {}: {} object(s), {} bytes",
        report.source_generation, report.catalog_key, report.object_count, report.copied_bytes
    );
    Ok(())
}

async fn pin(args: &[String]) -> CliResult {
    let (dir, namespace_name) = (arg(args, 2)?, arg(args, 3)?);
    let replicas = args
        .get(4)
        .map(|value| value.parse::<u32>())
        .transpose()?
        .unwrap_or(1);
    sana::pinning::PinningController::new(store(dir)?)
        .configure(namespace_name, Some(replicas))
        .await?;
    println!("pinned {namespace_name} with {replicas} replica(s)");
    Ok(())
}

async fn unpin(args: &[String]) -> CliResult {
    let (dir, namespace_name) = (arg(args, 2)?, arg(args, 3)?);
    sana::pinning::PinningController::new(store(dir)?)
        .configure(namespace_name, None)
        .await?;
    println!("unpinned {namespace_name}");
    Ok(())
}

async fn pin_status(args: &[String]) -> CliResult {
    let (dir, namespace_name) = (arg(args, 2)?, arg(args, 3)?);
    match sana::pinning::PinningController::new(store(dir)?)
        .metadata(namespace_name)
        .await?
    {
        Some(metadata) => println!(
            "{namespace_name}: {} configured, {} assigned, {} ready, utilization {}",
            metadata.replicas,
            metadata.assigned_replicas,
            metadata.ready_replicas,
            metadata
                .average_utilization
                .map(|value| format!("{:.1}%", value * 100.0))
                .unwrap_or_else(|| "n/a".into())
        ),
        None => println!("{namespace_name}: not pinned"),
    }
    Ok(())
}

async fn serve(args: &[String]) -> CliResult {
    let values = positional_args(args, &["--role"], &[])?;
    let dir = values
        .first()
        .copied()
        .ok_or_else(|| "missing argument #2".to_string())?;
    let role = serve_role(args)?;
    Box::pin(run_serve_role(args, dir, role, "serve")).await
}

async fn serve_api(args: &[String]) -> CliResult {
    let values = positional_args(args, &[], &[])?;
    let dir = values
        .first()
        .copied()
        .ok_or_else(|| "missing argument #2".to_string())?;
    Box::pin(run_serve_role(args, dir, ServeRole::Api, "serve-api")).await
}

async fn run_serve_role(args: &[String], dir: &str, role: ServeRole, command: &str) -> CliResult {
    let flag_names: &[&str] = if command == "serve" { &["--role"] } else { &[] };
    let values = positional_args(args, flag_names, &[])?;
    let address = values.get(1).copied().unwrap_or("127.0.0.1:8080").parse()?;
    let cache_bytes = values
        .get(2)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(256 * 1024 * 1024);
    let metrics = Metrics::shared();
    let backing = store(dir)?;
    let metered: Arc<dyn ObjectStore> = Arc::new(MeteredObjectStore::new(backing, metrics.clone()));
    let cached: Arc<dyn ObjectStore> =
        Arc::new(CachingObjectStore::new(metered, cache_bytes).with_metrics(metrics.clone()));
    match role {
        ServeRole::All => {
            println!("serving Sana all-in-one on http://{address} with {cache_bytes} cache bytes");
            Box::pin(sana::api::serve_with_shutdown(
                cached,
                address,
                metrics,
                shutdown_signal(),
            ))
            .await?;
        }
        ServeRole::Api => {
            println!("serving Sana API on http://{address} with {cache_bytes} cache bytes");
            sana::api::serve_api_with_shutdown(cached, address, metrics, shutdown_signal()).await?;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                eprintln!("failed to install Ctrl-C handler: {error}");
                std::future::pending::<()>().await;
            }
        }
        () = terminate_signal() => {}
    }
    println!("shutting down");
}

#[cfg(unix)]
async fn terminate_signal() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut signal) => {
            signal.recv().await;
        }
        Err(error) => {
            eprintln!("failed to install SIGTERM handler: {error}");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn terminate_signal() {
    std::future::pending::<()>().await;
}

fn shutdown_watcher() -> tokio::sync::watch::Receiver<bool> {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = sender.send(true);
    });
    receiver
}

async fn sleep_or_shutdown(
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    duration: std::time::Duration,
) -> bool {
    tokio::select! {
        _ = shutdown.changed() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

async fn demo(args: &[String]) -> CliResult {
    let dir = arg(args, 2)?;
    let ns = Namespace::create_or_open(store(dir)?, "demo").await?;

    let mut a = Document::new(Id::U64(1));
    a.attributes
        .insert("title".into(), Value::String("alpha".into()));
    a.attributes.insert("score".into(), Value::Int(10));
    ns.upsert(a).await?;

    let mut b = Document::new(Id::U64(2));
    b.attributes
        .insert("title".into(), Value::String("beta".into()));
    ns.upsert(b).await?;

    ns.delete(Id::U64(1)).await?;

    let docs = ns.replay().await?;
    println!("demo replay -> {} document(s) (expected 1):", docs.len());
    for (id, doc) in &docs {
        println!("  {id:?} -> {:?}", doc.attributes);
    }
    Ok(())
}

fn serve_role(args: &[String]) -> Result<ServeRole, String> {
    match flag_value(args, "--role")? {
        Some("all") => Ok(ServeRole::All),
        Some("api") => Ok(ServeRole::Api),
        Some(other) => Err(format!("unknown serve role '{other}', expected all or api")),
        None => Ok(ServeRole::All),
    }
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().skip(2).any(|arg| arg == flag)
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>, String> {
    let mut i = 2;
    while let Some(arg) = args.get(i) {
        if arg == flag {
            return args
                .get(i + 1)
                .map(String::as_str)
                .map(Some)
                .ok_or_else(|| format!("{flag} requires a value"));
        }
        i += 1;
    }
    Ok(None)
}

fn positional_args<'a>(
    args: &'a [String],
    flags_with_value: &[&str],
    valueless_flags: &[&str],
) -> Result<Vec<&'a str>, String> {
    let mut out = Vec::new();
    let mut i = 2;
    while let Some(arg) = args.get(i) {
        let value = arg.as_str();
        if flags_with_value.contains(&value) {
            if i + 1 >= args.len() {
                return Err(format!("{value} requires a value"));
            }
            i += 2;
        } else if valueless_flags.contains(&value) {
            i += 1;
        } else {
            out.push(value);
            i += 1;
        }
    }
    Ok(out)
}

fn arg(args: &[String], i: usize) -> Result<&str, String> {
    args.get(i)
        .map(String::as_str)
        .ok_or_else(|| format!("missing argument #{i}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn serve_role_defaults_to_all_and_accepts_api() {
        assert_eq!(
            serve_role(&args(&["sana", "serve", "./data"])).unwrap(),
            ServeRole::All
        );
        assert_eq!(
            serve_role(&args(&["sana", "serve", "./data", "--role", "api"])).unwrap(),
            ServeRole::Api
        );
        assert!(serve_role(&args(&["sana", "serve", "./data", "--role", "bad"])).is_err());
    }

    #[test]
    fn positional_args_skip_loop_and_role_flags() {
        assert_eq!(
            positional_args(
                &args(&[
                    "sana",
                    "serve",
                    "./data",
                    "--role",
                    "api",
                    "127.0.0.1:8081",
                    "1024"
                ]),
                &["--role"],
                &[]
            )
            .unwrap(),
            vec!["./data", "127.0.0.1:8081", "1024"]
        );
        assert_eq!(
            positional_args(
                &args(&["sana", "work-indexing", "./data", "--loop", "worker-a"]),
                &[],
                &["--loop"]
            )
            .unwrap(),
            vec!["./data", "worker-a"]
        );
    }
}
