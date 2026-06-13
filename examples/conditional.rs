//! Conditional writes (compare-and-set) and idempotent retries — the two write
//! primitives a client needs to be safe against retries and racing updates.
//!
//!   cargo run --example conditional

use std::sync::Arc;

use sana::namespace::Namespace;
use sana::object_store::{FsObjectStore, ObjectStore};
use sana::query::FilterExpr;
use sana::value::{Document, Id, Value};
use sana::wal::WalOp;
use sana::write::ConditionalWriteOp;

#[tokio::main]
async fn main() -> sana::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let store: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::new(dir.path()));
    let ns = Namespace::create(store, "accounts").await?;

    // Idempotent retry: a client that times out and resends the same batch under
    // the same key gets the *original* commit cursor back, not a duplicate write.
    let ops = vec![WalOp::Upsert {
        id: Id::U64(1),
        document: account(1, 100),
    }];
    let first = ns.append(ops.clone(), Some("txn-42".into())).await?;
    let retry = ns.append(ops, Some("txn-42".into())).await?;
    println!("idempotent append: first={first:?} retry={retry:?} (equal: {})", first == retry);

    // Compare-and-set: raise the balance to 150 only while it is still 100.
    let applied = ns
        .conditional_write(vec![cas_set_balance(1, 150, 100)], None)
        .await?;
    println!(
        "\nCAS 100 -> 150: applied={:?} skipped={:?}",
        applied.outcome.applied_ids, applied.outcome.skipped_ids
    );

    // The same precondition again now fails — the balance is 150 — so the op is
    // skipped and nothing changes.
    let skipped = ns
        .conditional_write(vec![cas_set_balance(1, 999, 100)], None)
        .await?;
    println!(
        "CAS 100 -> 999 on a stale precondition: applied={:?} skipped={:?}",
        skipped.outcome.applied_ids, skipped.outcome.skipped_ids
    );

    let current = ns.lookup(&Id::U64(1)).await?.expect("account 1 exists");
    println!("\nfinal balance: {:?}", current.attributes["balance"]);
    Ok(())
}

/// Set account `id`'s balance to `to`, conditional on it currently being `if_`.
fn cas_set_balance(id: u64, to: i64, if_: i64) -> ConditionalWriteOp {
    ConditionalWriteOp {
        operation: WalOp::Upsert {
            id: Id::U64(id),
            document: account(id, to),
        },
        condition: Some(FilterExpr::Eq {
            column: "balance".into(),
            value: Value::Int(if_),
        }),
    }
}

fn account(id: u64, balance: i64) -> Document {
    let mut doc = Document::new(Id::U64(id));
    doc.attributes.insert("balance".into(), Value::Int(balance));
    doc
}
