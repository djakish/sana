use std::collections::BTreeMap;

use sana::manifest::{VectorMaintenanceAction, VectorMaintenanceThresholds};
use sana::schema::DistanceMetric;
use sana::value::{Document, Id, VectorValue};
use sana::vector::{VectorEntry, VectorIndex, VectorVersionMap};

fn doc_with_vector(id: u64, vector: [f32; 2]) -> Document {
    let mut doc = Document::new(Id::U64(id));
    doc.vectors
        .insert("embedding".into(), VectorValue::F32(vector.to_vec()));
    doc
}

#[test]
fn maintenance_plan_merges_underfull_posting() {
    let docs = [
        doc_with_vector(1, [0.0, 0.0]),
        doc_with_vector(2, [100.0, 0.0]),
        doc_with_vector(3, [101.0, 0.0]),
        doc_with_vector(4, [102.0, 0.0]),
    ]
    .into_iter()
    .map(|doc| (doc.id.clone(), doc))
    .collect::<BTreeMap<_, _>>();
    let entries = docs
        .iter()
        .map(|(id, doc)| VectorEntry {
            id: id.clone(),
            vector: match doc.vectors.get("embedding").unwrap() {
                VectorValue::F32(vector) => vector.clone(),
                VectorValue::F16(_) => unreachable!(),
            },
            local_id: 0,
            version: 1,
        })
        .collect();

    let index = VectorIndex::build("embedding", DistanceMetric::L2, 2, entries, &docs)
        .unwrap()
        .unwrap();
    let version_map = VectorVersionMap::from_index(&index);
    let thresholds = VectorMaintenanceThresholds {
        min_posting_rows: 2,
        max_posting_rows: 100,
        reassign_neighborhood: 8,
    };

    let plan = index
        .plan_maintenance(&[], Some(&version_map), thresholds)
        .unwrap();
    let merge = plan
        .tasks
        .iter()
        .find(|task| task.action == VectorMaintenanceAction::Merge)
        .expect("underfull posting should be planned for merge");

    assert!(merge.live_rows < thresholds.min_posting_rows);
    assert!(merge.partner_cluster_id.is_some());
    assert!(!merge.neighbor_cluster_ids.is_empty());
}
