use std::collections::BTreeMap;

use sana::manifest::{VectorMaintenanceAction, VectorMaintenanceTask, VectorMaintenanceThresholds};
use sana::schema::DistanceMetric;
use sana::value::{Document, Id, VectorValue};
use sana::vector::{
    VectorAddress, VectorEntry, VectorFilterIndex, VectorIndex, VectorPosting, VectorVersionMap,
};

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

#[test]
fn reassignment_delta_moves_live_vectors_within_bounded_neighborhood() {
    let docs = [
        doc_with_vector(1, [9.0, 0.0]),
        doc_with_vector(2, [0.5, 0.0]),
        doc_with_vector(3, [10.0, 0.0]),
    ]
    .into_iter()
    .map(|doc| (doc.id.clone(), doc))
    .collect::<BTreeMap<_, _>>();
    let index = VectorIndex {
        format_version: 1,
        column: "embedding".into(),
        dim: 2,
        metric: DistanceMetric::L2,
        centroids: vec![vec![0.0, 0.0], vec![10.0, 0.0]],
        postings: vec![
            VectorPosting {
                centroid_id: 0,
                vectors: vec![
                    VectorEntry {
                        id: Id::U64(1),
                        vector: vec![9.0, 0.0],
                        local_id: 0,
                        version: 1,
                    },
                    VectorEntry {
                        id: Id::U64(2),
                        vector: vec![0.5, 0.0],
                        local_id: 1,
                        version: 1,
                    },
                ],
            },
            VectorPosting {
                centroid_id: 1,
                vectors: vec![VectorEntry {
                    id: Id::U64(3),
                    vector: vec![10.0, 0.0],
                    local_id: 0,
                    version: 1,
                }],
            },
        ],
        addresses: vec![
            VectorAddress {
                id: Id::U64(1),
                cluster_id: 0,
                local_id: 0,
                version: 1,
            },
            VectorAddress {
                id: Id::U64(2),
                cluster_id: 0,
                local_id: 1,
                version: 1,
            },
            VectorAddress {
                id: Id::U64(3),
                cluster_id: 1,
                local_id: 0,
                version: 1,
            },
        ],
        filter_index: VectorFilterIndex::default(),
    };
    let version_map = VectorVersionMap::from_index(&index);
    let task = VectorMaintenanceTask {
        action: VectorMaintenanceAction::Merge,
        cluster_id: 0,
        partner_cluster_id: Some(1),
        neighbor_cluster_ids: vec![1],
        live_rows: 3,
        stale_rows: 0,
        append_rows: 0,
        total_rows: 3,
    };

    let delta = index
        .build_reassignment_delta(&[], &version_map, &task, 2, &docs)
        .unwrap()
        .expect("one vector is nearer to the partner cluster");

    assert_eq!(delta.reassignments.len(), 1);
    assert_eq!(delta.reassignments[0].id, Id::U64(1));
    assert_eq!(delta.reassignments[0].from_cluster_id, 0);
    assert_eq!(delta.reassignments[0].to_cluster_id, 1);
    assert_eq!(delta.version_map.live_version(&Id::U64(1)), Some(2));
    assert_eq!(delta.version_map.live_version(&Id::U64(2)), Some(1));

    assert_eq!(delta.index.row_count(), 1);
    assert!(delta.index.postings[0].vectors.is_empty());
    assert_eq!(delta.index.postings[1].vectors[0].id, Id::U64(1));
    assert_eq!(delta.index.postings[1].vectors[0].version, 2);
    assert_eq!(delta.index.addresses[0].cluster_id, 1);
}
