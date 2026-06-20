use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::schema::DistanceMetric;

pub trait DistanceKernel {
    fn l2_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()>;
    fn dot_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()>;
    fn cosine_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistanceKernelKind {
    Scalar,
    #[cfg(target_arch = "aarch64")]
    Neon,
    #[cfg(target_arch = "x86_64")]
    Avx2,
}

pub struct ScalarDistanceKernel;
pub struct AutoDistanceKernel;

impl DistanceKernel for ScalarDistanceKernel {
    fn l2_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        scalar_l2_batch(query, candidates, out);
        Ok(())
    }

    fn dot_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        scalar_dot_batch(query, candidates, out);
        Ok(())
    }

    fn cosine_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        scalar_cosine_batch(query, candidates, out)
    }
}

impl DistanceKernel for AutoDistanceKernel {
    fn l2_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        match distance_kernel_kind() {
            DistanceKernelKind::Scalar => scalar_l2_batch(query, candidates, out),
            #[cfg(target_arch = "aarch64")]
            DistanceKernelKind::Neon => {
                // SAFETY: dispatch checked NEON support and validation checked all lengths.
                unsafe { aarch64::l2_batch(query, candidates, out) };
            }
            #[cfg(target_arch = "x86_64")]
            DistanceKernelKind::Avx2 => {
                // SAFETY: dispatch checked AVX2 support and validation checked all lengths.
                unsafe { x86_64::l2_batch(query, candidates, out) };
            }
        }
        Ok(())
    }

    fn dot_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        match distance_kernel_kind() {
            DistanceKernelKind::Scalar => scalar_dot_batch(query, candidates, out),
            #[cfg(target_arch = "aarch64")]
            DistanceKernelKind::Neon => {
                // SAFETY: dispatch checked NEON support and validation checked all lengths.
                unsafe { aarch64::dot_batch(query, candidates, out) };
            }
            #[cfg(target_arch = "x86_64")]
            DistanceKernelKind::Avx2 => {
                // SAFETY: dispatch checked AVX2 support and validation checked all lengths.
                unsafe { x86_64::dot_batch(query, candidates, out) };
            }
        }
        Ok(())
    }

    fn cosine_f32_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
        validate_batch(query, candidates, out)?;
        match distance_kernel_kind() {
            DistanceKernelKind::Scalar => scalar_cosine_batch(query, candidates, out),
            #[cfg(target_arch = "aarch64")]
            DistanceKernelKind::Neon => {
                // SAFETY: dispatch checked NEON support and validation checked all lengths.
                unsafe { aarch64::cosine_batch(query, candidates, out) }
            }
            #[cfg(target_arch = "x86_64")]
            DistanceKernelKind::Avx2 => {
                // SAFETY: dispatch checked AVX2 support and validation checked all lengths.
                unsafe { x86_64::cosine_batch(query, candidates, out) }
            }
        }
    }
}

pub fn distance_kernel_kind() -> DistanceKernelKind {
    static KIND: OnceLock<DistanceKernelKind> = OnceLock::new();
    *KIND.get_or_init(detect_kernel)
}

pub fn score_batch(
    query: &[f32],
    candidates: &[&[f32]],
    metric: DistanceMetric,
    out: &mut [f32],
) -> Result<()> {
    match metric {
        DistanceMetric::L2 => AutoDistanceKernel::l2_f32_batch(query, candidates, out),
        DistanceMetric::Dot => AutoDistanceKernel::dot_f32_batch(query, candidates, out),
        DistanceMetric::Cosine => AutoDistanceKernel::cosine_f32_batch(query, candidates, out),
    }
}

fn detect_kernel() -> DistanceKernelKind {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        return DistanceKernelKind::Neon;
    }
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        return DistanceKernelKind::Avx2;
    }
    DistanceKernelKind::Scalar
}

fn validate_batch(query: &[f32], candidates: &[&[f32]], out: &[f32]) -> Result<()> {
    if candidates.len() != out.len() {
        return Err(Error::InvalidQuery(format!(
            "score output has len {}, expected {}",
            out.len(),
            candidates.len()
        )));
    }
    if candidates
        .iter()
        .any(|candidate| candidate.len() != query.len())
    {
        return Err(Error::InvalidQuery(
            "query and candidate vectors must have matching dimensions".into(),
        ));
    }
    if query.iter().any(|v| !v.is_finite())
        || candidates
            .iter()
            .flat_map(|candidate| candidate.iter())
            .any(|v| !v.is_finite())
    {
        return Err(Error::InvalidQuery(
            "query and candidate vectors must contain only finite values".into(),
        ));
    }
    Ok(())
}

fn scalar_l2_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) {
    for (candidate, score) in candidates.iter().zip(out) {
        *score = -query
            .iter()
            .zip(*candidate)
            .map(|(a, b)| {
                let d = a - b;
                d * d
            })
            .sum::<f32>();
    }
}

fn scalar_dot_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) {
    for (candidate, score) in candidates.iter().zip(out) {
        *score = query.iter().zip(*candidate).map(|(a, b)| a * b).sum();
    }
}

fn scalar_cosine_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) -> Result<()> {
    let q_norm = squared_norm(query).sqrt();
    if q_norm == 0.0 {
        return Err(Error::InvalidQuery(
            "cosine query and candidate vectors must be non-zero".into(),
        ));
    }
    for (candidate, score) in candidates.iter().zip(out) {
        let c_norm = squared_norm(candidate).sqrt();
        if c_norm == 0.0 {
            return Err(Error::InvalidQuery(
                "cosine query and candidate vectors must be non-zero".into(),
            ));
        }
        let dot: f32 = query.iter().zip(*candidate).map(|(a, b)| a * b).sum();
        *score = dot / (q_norm * c_norm);
    }
    Ok(())
}

fn squared_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|v| v * v).sum()
}

fn tail_pairs<'a>(
    left: &'a [f32],
    right: &'a [f32],
    start: usize,
) -> impl Iterator<Item = (f32, f32)> + 'a {
    debug_assert!(start <= left.len());
    debug_assert!(start <= right.len());
    let (_, left_tail) = left.split_at(start);
    let (_, right_tail) = right.split_at(start);
    left_tail.iter().copied().zip(right_tail.iter().copied())
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use std::arch::aarch64::{
        float32x4_t, vaddvq_f32, vdupq_n_f32, vld1q_f32, vmlaq_f32, vsubq_f32,
    };

    use crate::error::{Error, Result};

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn l2_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) {
        for (candidate, score) in candidates.iter().zip(out) {
            // SAFETY: caller validated NEON support and matching slice lengths.
            *score = -unsafe { l2(query, candidate) };
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn dot_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) {
        for (candidate, score) in candidates.iter().zip(out) {
            // SAFETY: caller validated NEON support and matching slice lengths.
            *score = unsafe { dot(query, candidate) };
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn cosine_batch(
        query: &[f32],
        candidates: &[&[f32]],
        out: &mut [f32],
    ) -> Result<()> {
        // SAFETY: caller validated NEON support; using the same slice for both
        // operands trivially satisfies the matching-length precondition.
        let q_norm = unsafe { dot(query, query) }.sqrt();
        if q_norm == 0.0 {
            return Err(Error::InvalidQuery(
                "cosine query and candidate vectors must be non-zero".into(),
            ));
        }
        for (candidate, score) in candidates.iter().zip(out) {
            // SAFETY: caller validated NEON support and matching slice lengths.
            let (dot, norm_sq) = unsafe { dot_and_norm(query, candidate) };
            let c_norm = norm_sq.sqrt();
            if c_norm == 0.0 {
                return Err(Error::InvalidQuery(
                    "cosine query and candidate vectors must be non-zero".into(),
                ));
            }
            *score = dot / (q_norm * c_norm);
        }
        Ok(())
    }

    #[target_feature(enable = "neon")]
    unsafe fn l2(left: &[f32], right: &[f32]) -> f32 {
        let mut acc = vdupq_n_f32(0.0);
        let mut i = 0;
        while i + 4 <= left.len() {
            // SAFETY: the loop condition guarantees four `left` lanes.
            let a = unsafe { vld1q_f32(left.as_ptr().add(i)) };
            // SAFETY: callers validate `right.len() == left.len()`.
            let b = unsafe { vld1q_f32(right.as_ptr().add(i)) };
            let diff = vsubq_f32(a, b);
            acc = vmlaq_f32(acc, diff, diff);
            i += 4;
        }
        let mut sum = vaddvq_f32(acc);
        for (left, right) in super::tail_pairs(left, right, i) {
            let diff = left - right;
            sum += diff * diff;
        }
        sum
    }

    #[target_feature(enable = "neon")]
    unsafe fn dot(left: &[f32], right: &[f32]) -> f32 {
        let mut acc = vdupq_n_f32(0.0);
        let mut i = 0;
        while i + 4 <= left.len() {
            // SAFETY: the loop condition guarantees four `left` lanes.
            let a = unsafe { vld1q_f32(left.as_ptr().add(i)) };
            // SAFETY: callers validate `right.len() == left.len()`.
            let b = unsafe { vld1q_f32(right.as_ptr().add(i)) };
            acc = vmlaq_f32(acc, a, b);
            i += 4;
        }
        let mut sum = vaddvq_f32(acc);
        for (left, right) in super::tail_pairs(left, right, i) {
            sum += left * right;
        }
        sum
    }

    #[target_feature(enable = "neon")]
    unsafe fn dot_and_norm(left: &[f32], right: &[f32]) -> (f32, f32) {
        let mut dot_acc: float32x4_t = vdupq_n_f32(0.0);
        let mut norm_acc: float32x4_t = vdupq_n_f32(0.0);
        let mut i = 0;
        while i + 4 <= left.len() {
            // SAFETY: the loop condition guarantees four `left` lanes.
            let a = unsafe { vld1q_f32(left.as_ptr().add(i)) };
            // SAFETY: callers validate `right.len() == left.len()`.
            let b = unsafe { vld1q_f32(right.as_ptr().add(i)) };
            dot_acc = vmlaq_f32(dot_acc, a, b);
            norm_acc = vmlaq_f32(norm_acc, b, b);
            i += 4;
        }
        let mut dot = vaddvq_f32(dot_acc);
        let mut norm = vaddvq_f32(norm_acc);
        for (left, right) in super::tail_pairs(left, right, i) {
            dot += left * right;
            norm += right * right;
        }
        (dot, norm)
    }
}

#[cfg(target_arch = "x86_64")]
mod x86_64 {
    use std::arch::x86_64::{
        __m256, _mm256_add_ps, _mm256_loadu_ps, _mm256_mul_ps, _mm256_setzero_ps, _mm256_storeu_ps,
        _mm256_sub_ps,
    };

    use crate::error::{Error, Result};

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn l2_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) {
        for (candidate, score) in candidates.iter().zip(out) {
            // SAFETY: caller validated AVX2 support and matching slice lengths.
            *score = -unsafe { l2(query, candidate) };
        }
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn dot_batch(query: &[f32], candidates: &[&[f32]], out: &mut [f32]) {
        for (candidate, score) in candidates.iter().zip(out) {
            // SAFETY: caller validated AVX2 support and matching slice lengths.
            *score = unsafe { dot(query, candidate) };
        }
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn cosine_batch(
        query: &[f32],
        candidates: &[&[f32]],
        out: &mut [f32],
    ) -> Result<()> {
        // SAFETY: caller validated AVX2 support; using the same slice for both
        // operands trivially satisfies the matching-length precondition.
        let q_norm = unsafe { dot(query, query) }.sqrt();
        if q_norm == 0.0 {
            return Err(Error::InvalidQuery(
                "cosine query and candidate vectors must be non-zero".into(),
            ));
        }
        for (candidate, score) in candidates.iter().zip(out) {
            // SAFETY: caller validated AVX2 support and matching slice lengths.
            let (dot, norm_sq) = unsafe { dot_and_norm(query, candidate) };
            let c_norm = norm_sq.sqrt();
            if c_norm == 0.0 {
                return Err(Error::InvalidQuery(
                    "cosine query and candidate vectors must be non-zero".into(),
                ));
            }
            *score = dot / (q_norm * c_norm);
        }
        Ok(())
    }

    #[target_feature(enable = "avx2")]
    unsafe fn l2(left: &[f32], right: &[f32]) -> f32 {
        let mut acc = _mm256_setzero_ps();
        let mut i = 0;
        while i + 8 <= left.len() {
            // SAFETY: the loop condition guarantees eight `left` lanes.
            let a = unsafe { _mm256_loadu_ps(left.as_ptr().add(i)) };
            // SAFETY: callers validate `right.len() == left.len()`.
            let b = unsafe { _mm256_loadu_ps(right.as_ptr().add(i)) };
            let diff = _mm256_sub_ps(a, b);
            acc = _mm256_add_ps(acc, _mm256_mul_ps(diff, diff));
            i += 8;
        }
        // SAFETY: `horizontal_sum` only stores the local vector into stack lanes.
        let mut sum = unsafe { horizontal_sum(acc) };
        for (left, right) in super::tail_pairs(left, right, i) {
            let diff = left - right;
            sum += diff * diff;
        }
        sum
    }

    #[target_feature(enable = "avx2")]
    unsafe fn dot(left: &[f32], right: &[f32]) -> f32 {
        let mut acc = _mm256_setzero_ps();
        let mut i = 0;
        while i + 8 <= left.len() {
            // SAFETY: the loop condition guarantees eight `left` lanes.
            let a = unsafe { _mm256_loadu_ps(left.as_ptr().add(i)) };
            // SAFETY: callers validate `right.len() == left.len()`.
            let b = unsafe { _mm256_loadu_ps(right.as_ptr().add(i)) };
            acc = _mm256_add_ps(acc, _mm256_mul_ps(a, b));
            i += 8;
        }
        // SAFETY: `horizontal_sum` only stores the local vector into stack lanes.
        let mut sum = unsafe { horizontal_sum(acc) };
        for (left, right) in super::tail_pairs(left, right, i) {
            sum += left * right;
        }
        sum
    }

    #[target_feature(enable = "avx2")]
    unsafe fn dot_and_norm(left: &[f32], right: &[f32]) -> (f32, f32) {
        let mut dot_acc = _mm256_setzero_ps();
        let mut norm_acc = _mm256_setzero_ps();
        let mut i = 0;
        while i + 8 <= left.len() {
            // SAFETY: the loop condition guarantees eight `left` lanes.
            let a = unsafe { _mm256_loadu_ps(left.as_ptr().add(i)) };
            // SAFETY: callers validate `right.len() == left.len()`.
            let b = unsafe { _mm256_loadu_ps(right.as_ptr().add(i)) };
            dot_acc = _mm256_add_ps(dot_acc, _mm256_mul_ps(a, b));
            norm_acc = _mm256_add_ps(norm_acc, _mm256_mul_ps(b, b));
            i += 8;
        }
        // SAFETY: `horizontal_sum` only stores the local vector into stack lanes.
        let mut dot = unsafe { horizontal_sum(dot_acc) };
        // SAFETY: `horizontal_sum` only stores the local vector into stack lanes.
        let mut norm = unsafe { horizontal_sum(norm_acc) };
        for (left, right) in super::tail_pairs(left, right, i) {
            dot += left * right;
            norm += right * right;
        }
        (dot, norm)
    }

    unsafe fn horizontal_sum(value: __m256) -> f32 {
        let mut lanes = [0.0f32; 8];
        // SAFETY: `lanes` has exactly eight contiguous f32 elements.
        unsafe { _mm256_storeu_ps(lanes.as_mut_ptr(), value) };
        lanes.into_iter().sum()
    }
}
