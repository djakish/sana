use std::hint::black_box;
use std::time::{Duration, Instant};

use sana::schema::DistanceMetric;
use sana::vector::{
    AutoDistanceKernel, DistanceKernel, ScalarDistanceKernel, distance_kernel_kind,
};

fn main() {
    println!("runtime kernel: {:?}", distance_kernel_kind());
    println!("logical GiB/s over candidate vectors (higher is better)");
    run_suite("cache-hot 4 MiB", 1 << 20, 32, &[128, 768, 1536]);
    run_suite("DRAM 64 MiB", 1 << 24, 4, &[768]);
}

fn run_suite(label: &str, target_floats: usize, iterations: usize, dimensions: &[usize]) {
    println!("{label}:");
    for &dim in dimensions {
        let count = (target_floats / dim).max(1);
        let mut seed = 0x1234_5678_9abc_def0;
        let query = random_vector(dim, &mut seed);
        let owned = (0..count)
            .map(|_| random_vector(dim, &mut seed))
            .collect::<Vec<_>>();
        let candidates = owned.iter().map(Vec::as_slice).collect::<Vec<_>>();

        for metric in [
            DistanceMetric::L2,
            DistanceMetric::Dot,
            DistanceMetric::Cosine,
        ] {
            let scalar = measure::<ScalarDistanceKernel>(
                &query,
                &candidates,
                metric,
                iterations,
                dim,
                count,
            );
            let runtime =
                measure::<AutoDistanceKernel>(&query, &candidates, metric, iterations, dim, count);
            println!(
                "dim={dim:4} metric={metric:?} scalar={:6.2} runtime={:6.2} speedup={:4.2}x",
                scalar.1,
                runtime.1,
                scalar.0.as_secs_f64() / runtime.0.as_secs_f64()
            );
        }
    }
}

fn measure<K: DistanceKernel>(
    query: &[f32],
    candidates: &[&[f32]],
    metric: DistanceMetric,
    iterations: usize,
    dim: usize,
    count: usize,
) -> (Duration, f64) {
    let mut out = vec![0.0; candidates.len()];
    for _ in 0..3 {
        run::<K>(query, candidates, metric, &mut out);
    }

    let start = Instant::now();
    for _ in 0..iterations {
        run::<K>(
            black_box(query),
            black_box(candidates),
            metric,
            black_box(&mut out),
        );
    }
    let elapsed = start.elapsed();
    black_box(out);
    let bytes = dim as f64 * count as f64 * size_of::<f32>() as f64 * iterations as f64;
    let gib_per_second = bytes / elapsed.as_secs_f64() / (1u64 << 30) as f64;
    (elapsed, gib_per_second)
}

fn run<K: DistanceKernel>(
    query: &[f32],
    candidates: &[&[f32]],
    metric: DistanceMetric,
    out: &mut [f32],
) {
    match metric {
        DistanceMetric::L2 => K::l2_f32_batch(query, candidates, out),
        DistanceMetric::Dot => K::dot_f32_batch(query, candidates, out),
        DistanceMetric::Cosine => K::cosine_f32_batch(query, candidates, out),
    }
    .unwrap();
}

fn random_vector(dim: usize, seed: &mut u64) -> Vec<f32> {
    (0..dim)
        .map(|_| {
            *seed = splitmix(*seed);
            (*seed >> 40) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        })
        .collect()
}

fn splitmix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = value;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}
