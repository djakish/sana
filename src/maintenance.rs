//! Policy-driven background maintenance, so a single `sana serve` keeps its
//! namespaces tidy without operator cron jobs.
//!
//! One pass scans every namespace and, for fully indexed ones, runs the
//! existing maintenance primitives in priority order: full compaction when
//! run counts or vector append chains grow past the policy thresholds,
//! otherwise manifest-published vector split/merge/reassign work. Online GC is
//! disabled by default: deleting immutable objects safely in a multi-process
//! deployment requires durable reader/publisher watermarks, not a local timer.
//! Operators can still opt into the legacy two-pass GC while the CLI dry-run
//! remains available. Per-namespace failures are reported, not fatal — one
//! wedged namespace must not stall the fleet.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::error::Result;
use crate::index_queue::list_namespace_names;
use crate::indexer;
use crate::namespace::Namespace;
use crate::object_store::ObjectStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenancePolicy {
    /// Compact a namespace when its doc or attribute run count reaches this.
    pub compact_at_runs: usize,
    /// Compact when any vector column's append-delta chain reaches this.
    pub compact_at_vector_appends: usize,
    /// Execute manifest-published vector maintenance tasks.
    pub vector_maintenance: bool,
    /// Reclaim orphaned objects with legacy two-pass deferred deletion.
    ///
    /// This is intentionally off by default. It is safe only in controlled
    /// single-process/quiescent deployments; production GC needs a durable safe
    /// point over readers and publishers.
    pub gc: bool,
}

impl Default for MaintenancePolicy {
    fn default() -> Self {
        Self {
            compact_at_runs: 8,
            compact_at_vector_appends: 4,
            vector_maintenance: true,
            gc: false,
        }
    }
}

/// Cross-pass memory for opt-in deferred GC: the orphans each namespace showed
/// on the previous pass. An object is deleted only when two consecutive scans
/// agree.
#[derive(Debug, Default)]
pub struct MaintenanceState {
    gc_candidates: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub scanned_namespaces: usize,
    pub compacted: Vec<String>,
    pub vector_maintained: Vec<String>,
    pub gc_deleted_objects: usize,
    /// Fresh orphans observed this pass; they become deletable next pass.
    pub gc_pending_objects: usize,
    /// Per-namespace failures as `"{namespace}: {error}"`, isolated so the
    /// rest of the pass still runs.
    pub errors: Vec<String>,
}

/// Run one maintenance pass over every namespace in the store.
pub async fn run_once(
    store: Arc<dyn ObjectStore>,
    policy: &MaintenancePolicy,
    state: &mut MaintenanceState,
) -> Result<MaintenanceReport> {
    let names = list_namespace_names(&store).await?;
    let mut report = MaintenanceReport {
        scanned_namespaces: names.len(),
        ..MaintenanceReport::default()
    };

    for name in &names {
        let prior = state.gc_candidates.remove(name).unwrap_or_default();
        match maintain_namespace(&store, name, policy, prior, &mut report).await {
            Ok(pending) => {
                if !pending.is_empty() {
                    state.gc_candidates.insert(name.clone(), pending);
                }
            }
            Err(error) => report.errors.push(format!("{name}: {error}")),
        }
    }
    // Drop candidates for namespaces that disappeared since the last pass.
    state.gc_candidates.retain(|name, _| names.contains(name));
    Ok(report)
}

/// Maintain one namespace; returns the orphan keys to remember for next pass.
async fn maintain_namespace(
    store: &Arc<dyn ObjectStore>,
    name: &str,
    policy: &MaintenancePolicy,
    prior_candidates: BTreeSet<String>,
    report: &mut MaintenanceReport,
) -> Result<BTreeSet<String>> {
    let ns = Namespace::open(store.clone(), name).await?;
    let manifest = ns.load_manifest().await?;
    let commit = ns.commit_cursor().await?;
    let fully_indexed = manifest.indexed_cursor == Some(commit);

    // Index-shape work only on fully indexed namespaces: the flush worker owns
    // catching up, and compaction/maintenance on a lagging namespace would just
    // be repeated. Compaction subsumes vector maintenance (it rebuilds the
    // base and clears append chains), so at most one of the two runs per pass.
    if fully_indexed {
        let needs_compaction = manifest.doc_ssts.len() >= policy.compact_at_runs
            || manifest.attr_ssts.len() >= policy.compact_at_runs
            || manifest
                .vector_indexes
                .values()
                .any(|meta| meta.append_indexes.len() >= policy.compact_at_vector_appends);
        let has_vector_tasks = manifest.vector_indexes.values().any(|meta| {
            meta.maintenance_plan
                .as_ref()
                .is_some_and(|plan| !plan.tasks.is_empty())
        });

        if needs_compaction {
            if indexer::compact(&ns).await? {
                report.compacted.push(name.to_string());
            }
        } else if policy.vector_maintenance
            && has_vector_tasks
            && indexer::maintain_vectors(&ns).await?
        {
            report.vector_maintained.push(name.to_string());
        }
    }

    if !policy.gc {
        return Ok(BTreeSet::new());
    }

    // Deferred GC: scan now, but delete only what the *previous* pass already
    // reported orphaned. The scan after a compaction above sees the new
    // manifest, so the just-superseded runs enter the candidate set and are
    // reclaimed one interval later.
    let scan = indexer::gc(&ns, false).await?;
    let orphans: BTreeSet<String> = scan.orphan_keys.into_iter().collect();
    for key in orphans.intersection(&prior_candidates) {
        store.delete(key).await?;
        report.gc_deleted_objects += 1;
    }
    let pending: BTreeSet<String> = orphans.difference(&prior_candidates).cloned().collect();
    report.gc_pending_objects += pending.len();
    Ok(pending)
}
