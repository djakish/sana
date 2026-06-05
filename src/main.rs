//! Minimal Stage 1 CLI over a filesystem-backed namespace. This is a thin
//! harness to exercise the engine end to end; the real API surface (HTTP)
//! arrives later in the build plan.

use std::sync::Arc;

use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
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
        Some("flush") => flush(&args).await,
        Some("compact") => compact(&args).await,
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
    eprintln!("  sana flush   <dir> <ns>   # fold WAL into a document SST");
    eprintln!("  sana compact <dir> <ns>   # merge SSTs, drop tombstones");
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

async fn flush(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir), ns).await?;
    let did = sana::indexer::flush(&namespace).await?;
    println!("{}", if did { "flushed WAL into a new SST" } else { "nothing to flush" });
    Ok(())
}

async fn compact(args: &[String]) -> CliResult {
    let (dir, ns) = (arg(args, 2)?, arg(args, 3)?);
    let namespace = Namespace::open(store(dir), ns).await?;
    let did = sana::indexer::compact(&namespace).await?;
    println!("{}", if did { "compacted SSTs" } else { "nothing to compact" });
    Ok(())
}

async fn demo(args: &[String]) -> CliResult {
    let dir = arg(args, 2)?;
    let ns = Namespace::create_or_open(store(dir), "demo").await?;

    let mut a = Document::new(Id::U64(1));
    a.attributes.insert("title".into(), Value::String("alpha".into()));
    a.attributes.insert("score".into(), Value::Int(10));
    ns.upsert(a).await?;

    let mut b = Document::new(Id::U64(2));
    b.attributes.insert("title".into(), Value::String("beta".into()));
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
