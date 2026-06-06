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
fn score_batch_matches_scalar_scores_for_all_metrics() {
    let query = [1.0, 2.0];
    let first = [1.0, 2.0];
    let second = [3.0, 4.0];
    let candidates = [&first[..], &second[..]];

    let mut l2 = [0.0; 2];
    sana::vector::score_batch(&query, &candidates, DistanceMetric::L2, &mut l2).unwrap();
    assert_eq!(l2, [0.0, -8.0]);

    let mut dot = [0.0; 2];
    sana::vector::score_batch(&query, &candidates, DistanceMetric::Dot, &mut dot).unwrap();
    assert_eq!(dot, [5.0, 11.0]);

    let mut cosine = [0.0; 2];
    sana::vector::score_batch(&query, &candidates, DistanceMetric::Cosine, &mut cosine).unwrap();
    assert!((cosine[0] - 1.0).abs() < 1e-6);
    assert!((cosine[1] - 0.983_869_9).abs() < 1e-6);
}

#[test]
fn score_batch_rejects_dimension_mismatch() {
    let query = [1.0, 2.0];
    let bad = [1.0];
    let candidates = [&bad[..]];
    let mut out = [0.0];

    assert!(sana::vector::score_batch(&query, &candidates, DistanceMetric::L2, &mut out).is_err());
}

#[test]
fn rabitq_code_generation_packs_cluster_residual_bits() {
    let dim = 70usize;
    let index = VectorIndex {
        format_version: 1,
        column: "embedding".into(),
        dim,
        metric: DistanceMetric::L2,
        centroids: vec![vec![0.0; dim]],
        postings: vec![VectorPosting {
            centroid_id: 0,
            vectors: vec![
                VectorEntry {
                    id: Id::U64(1),
                    vector: vec![1.0; dim],
                    local_id: 0,
                    version: 7,
                },
                VectorEntry {
                    id: Id::U64(2),
                    vector: vec![0.0; dim],
                    local_id: 1,
                    version: 7,
                },
            ],
        }],
        addresses: vec![
            VectorAddress {
                id: Id::U64(1),
                cluster_id: 0,
                local_id: 0,
                version: 7,
            },
            VectorAddress {
                id: Id::U64(2),
                cluster_id: 0,
                local_id: 1,
                version: 7,
            },
        ],
        filter_index: VectorFilterIndex::default(),
    };

    let rabitq = sana::rabitq::build_index(&index).unwrap();
    assert_eq!(rabitq.column, "embedding");
    assert_eq!(rabitq.dim, dim);
    assert_eq!(rabitq.padded_dim, 128); // dim 70 rounded up to a power of two
    assert_eq!(rabitq.clusters.len(), 1);

    let cluster = &rabitq.clusters[0];
    assert_eq!(cluster.centroid_id, 0);
    assert_eq!(cluster.codes.len(), 2);

    // Non-zero residual: code spans the padded dimension, norm and factor are set.
    let non_zero = &cluster.codes[0];
    assert_eq!(non_zero.id, Id::U64(1));
    assert_eq!(non_zero.local_id, 0);
    assert_eq!(non_zero.version, 7);
    assert_eq!(non_zero.code_words.len(), 128 / 64);
    assert!((non_zero.residual_norm - (dim as f32).sqrt()).abs() < 1e-6);
    // The correction factor ⟨ō_q, ō'⟩ lies in (0, 1] for any unit residual.
    assert!(non_zero.dot_code > 0.0 && non_zero.dot_code <= 1.0 + 1e-6);

    // Zero residual quantizes to the empty code with a zero norm and factor.
    let zero = &cluster.codes[1];
    assert_eq!(zero.id, Id::U64(2));
    assert_eq!(zero.residual_norm, 0.0);
    assert_eq!(zero.dot_code, 0.0);
    assert!(zero.code_words.iter().all(|word| *word == 0));
}

/// The estimator is only useful if its ranking tracks exact L2. Quantize a random
/// cloud and confirm RaBitQ recovers most of the true nearest neighbours — far
/// above the ~4% a random ranking would score.
#[test]
fn rabitq_estimate_recovers_nearest_neighbours() {
    let dim = 128usize;
    let n = 256usize;
    let mut rng = 0x5151_5151_5151_5151u64;
    let mut next = || {
        rng = splitmix(rng);
        (rng >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0 // uniform in [-1, 1)
    };

    let vectors: Vec<(Id, Vec<f32>)> = (0..n)
        .map(|i| {
            let v = (0..dim).map(|_| next()).collect::<Vec<_>>();
            (Id::U64(i as u64), v)
        })
        .collect();
    let query: Vec<f32> = (0..dim).map(|_| next()).collect();

    let entries = vectors
        .iter()
        .enumerate()
        .map(|(local_id, (id, v))| VectorEntry {
            id: id.clone(),
            vector: v.clone(),
            local_id: local_id as u32,
            version: 1,
        })
        .collect::<Vec<_>>();
    let index = VectorIndex::build(
        "embedding",
        DistanceMetric::L2,
        dim,
        entries,
        &BTreeMap::new(),
    )
    .unwrap()
    .unwrap();
    let rabitq = sana::rabitq::build_index(&index).unwrap();

    let exact: Vec<(Id, f32)> = vectors
        .iter()
        .map(|(id, v)| {
            let d = v.iter().zip(&query).map(|(a, b)| (a - b) * (a - b)).sum();
            (id.clone(), d)
        })
        .collect();

    let mut estimated: Vec<(Id, f32)> = Vec::with_capacity(n);
    for cluster in &rabitq.clusters {
        let centroid = &index.centroids[cluster.centroid_id as usize];
        let residual: Vec<f32> = query.iter().zip(centroid).map(|(q, c)| q - c).collect();
        let rotated = cluster.rotate_query(&residual, rabitq.padded_dim);
        for code in &cluster.codes {
            let est = code.estimate_l2_sq(&rotated);
            assert!(est.is_finite() && est >= 0.0);
            estimated.push((code.id.clone(), est));
        }
    }
    assert_eq!(estimated.len(), n);

    let top_k = 10;
    let truth: std::collections::BTreeSet<Id> = top_ids(exact, top_k);
    let approx: std::collections::BTreeSet<Id> = top_ids(estimated, top_k);
    let hits = truth.intersection(&approx).count();
    assert!(
        hits as f64 / top_k as f64 >= 0.6,
        "RaBitQ recall@{top_k} too low: {hits}/{top_k}"
    );
}

fn top_ids(mut scored: Vec<(Id, f32)>, k: usize) -> std::collections::BTreeSet<Id> {
    scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().take(k).map(|(id, _)| id).collect()
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
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

#[test]
fn local_rebuild_delta_splits_overfull_posting_topology() {
    let docs = [
        doc_with_vector(1, [0.0, 0.0]),
        doc_with_vector(2, [1.0, 0.0]),
        doc_with_vector(3, [10.0, 0.0]),
        doc_with_vector(4, [11.0, 0.0]),
    ]
    .into_iter()
    .map(|doc| (doc.id.clone(), doc))
    .collect::<BTreeMap<_, _>>();
    let index = VectorIndex {
        format_version: 1,
        column: "embedding".into(),
        dim: 2,
        metric: DistanceMetric::L2,
        centroids: vec![vec![5.5, 0.0]],
        postings: vec![VectorPosting {
            centroid_id: 0,
            vectors: (1..=4)
                .map(|id| VectorEntry {
                    id: Id::U64(id),
                    vector: match docs[&Id::U64(id)].vectors.get("embedding").unwrap() {
                        VectorValue::F32(vector) => vector.clone(),
                        VectorValue::F16(_) => unreachable!(),
                    },
                    local_id: (id - 1) as u32,
                    version: 1,
                })
                .collect(),
        }],
        addresses: (1..=4)
            .map(|id| VectorAddress {
                id: Id::U64(id),
                cluster_id: 0,
                local_id: (id - 1) as u32,
                version: 1,
            })
            .collect(),
        filter_index: VectorFilterIndex::default(),
    };
    let version_map = VectorVersionMap::from_index(&index);
    let task = VectorMaintenanceTask {
        action: VectorMaintenanceAction::Split,
        cluster_id: 0,
        partner_cluster_id: None,
        neighbor_cluster_ids: Vec::new(),
        live_rows: 4,
        stale_rows: 0,
        append_rows: 0,
        total_rows: 4,
    };

    let delta = index
        .build_local_rebuild_delta(&[], &version_map, &task, 2, &docs)
        .unwrap()
        .expect("overfull posting should be locally rebuilt");

    assert_eq!(
        delta.rebuilt_ids,
        vec![Id::U64(1), Id::U64(2), Id::U64(3), Id::U64(4)]
    );
    assert_eq!(delta.rebuilt_cluster_ids, vec![0]);
    assert_eq!(delta.index.row_count(), 4);
    assert_eq!(delta.index.centroids.len(), 2);
    assert_ne!(delta.index.centroids, index.centroids);
    for id in 1..=4 {
        assert_eq!(delta.version_map.live_version(&Id::U64(id)), Some(2));
    }
}
