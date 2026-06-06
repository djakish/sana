//! Minimal Stage 1 CLI over a filesystem-backed namespace. This is a thin
//! harness to exercise the engine end to end; the real API surface (HTTP)
//! arrives later in the build plan.

use std::sync::Arc;

use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::query::{MultiQuery, Query, RecallRequest};
use sana::value::{Document, Id, Value};

type CliResult = Result<(), Box<dyn std::error::Error>>;

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
        Some("reconcile-indexing") => reconcile_indexing(&args).await,
        Some("work-indexing") => work_indexing(&args).await,
        Some("branch") => branch(&args).await,
        Some("copy") => copy(&args).await,
        Some("export") => export(&args).await,
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
    eprintln!("  sana reconcile-indexing <dir>   # restore missed indexing notifications");
    eprintln!("  sana work-indexing <dir> <worker-id>   # claim and run one indexing job");
    eprintln!("  sana branch <dir> <source-ns> <child-ns>   # zero-copy indexed snapshot");
    eprintln!("  sana copy <source-dir> <source-ns> <target-dir> <target-ns>");
    eprintln!("  sana export <source-dir> <ns> <target-dir> <prefix>");
    eprintln!("  sana demo    <dir>");
}

fn store(dir: &str) -> Arc<dyn ObjectStore> {
    Arc::new(FsObjectStore::new(dir))
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
    Namespace::create(store(dir), ns).await?;
    println!("created namespace {ns}");
    Ok(())
}

async fn upsert(args: &[String]) -> CliResult {
    let (dir, ns, id) = (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?);
    let namespace = Namespace::create_or_open(store(dir), ns).await?;

    let mut doc = Document::new(parse_id(id));
    for pair in &args[5.min(args.len())..] {
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
    let namespace = Namespace::open(store(dir), ns).await?;
    match namespace.lookup(&parse_id(id)).await? {
        Some(doc) => println!("{doc:#?}"),
        None => println!("not found: {id}"),
    }
    Ok(())
}

async fn delete(args: &[String]) -> CliResult {
    let (dir, ns, id) = (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?);
    let namespace = Namespace::open(store(dir), ns).await?;
    namespace.delete(parse_id(id)).await?;
    println!("deleted {id}");
    Ok(())
}

async fn list(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir), ns).await?;
    let docs = namespace.replay().await?;
    println!("{} document(s):", docs.len());
    for (id, doc) in &docs {
        println!("  {id:?} -> {} attr(s)", doc.attributes.len());
    }
    Ok(())
}

async fn query(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir), ns).await?;
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
    let namespace = Namespace::open(store(dir), ns).await?;
    let request = serde_json::from_str::<MultiQuery>(json)?;
    let result = namespace.multi_query(request).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn recall(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir), ns).await?;
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
    let namespace = Namespace::open(store(dir), ns).await?;
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
    let namespace = Namespace::open(store(dir), ns).await?;
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
    let namespace = Namespace::open(store(dir), ns).await?;
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
    let namespace = Namespace::open(store(dir), ns).await?;
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

async fn work_indexing(args: &[String]) -> CliResult {
    let (dir, worker_id) = (arg(args, 2)?, arg(args, 3)?);
    match sana::index_queue::run_worker_once(store(dir), worker_id, 30_000, 1_000).await? {
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

async fn reconcile_indexing(args: &[String]) -> CliResult {
    let dir = arg(args, 2)?;
    let report = sana::index_queue::reconcile_unindexed(store(dir)).await?;
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
    let source = Namespace::open(store(dir), source_name).await?;
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
    let source = Namespace::open(store(source_dir), source_name).await?;
    let report = source.copy_to(store(target_dir), target_name).await?;
    println!(
        "copied {source_name} generation {} to {target_name}: {} object(s), {} bytes",
        report.source_generation, report.object_count, report.copied_bytes
    );
    Ok(())
}

async fn export(args: &[String]) -> CliResult {
    let (source_dir, namespace_name, target_dir, prefix) =
        (arg(args, 2)?, arg(args, 3)?, arg(args, 4)?, arg(args, 5)?);
    let namespace = Namespace::open(store(source_dir), namespace_name).await?;
    let report = namespace.export_to(store(target_dir), prefix).await?;
    println!(
        "exported {namespace_name} generation {} to {}: {} object(s), {} bytes",
        report.source_generation, report.catalog_key, report.object_count, report.copied_bytes
    );
    Ok(())
}

async fn demo(args: &[String]) -> CliResult {
    let dir = arg(args, 2)?;
    let ns = Namespace::create_or_open(store(dir), "demo").await?;

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

fn arg(args: &[String], i: usize) -> Result<&str, String> {
    args.get(i)
        .map(String::as_str)
        .ok_or_else(|| format!("missing argument #{i}"))
}
