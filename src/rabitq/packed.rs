const QUERY_BITS: usize = 4;
const QUERY_LEVELS: u32 = (1 << QUERY_BITS) - 1;

#[derive(Clone, Debug)]
pub(super) struct PackedQuery {
    bit_planes: [Vec<u64>; QUERY_BITS],
    lower: f32,
    delta: f32,
    sum_quantized: u64,
    padded_dim: usize,
}

impl PackedQuery {
    pub(super) fn new(rotated: &[f32], seed: u64) -> Self {
        let (lower, upper) = rotated.iter().copied().fold(
            (f32::INFINITY, f32::NEG_INFINITY),
            |(lower, upper), value| (lower.min(value), upper.max(value)),
        );
        let delta = (upper - lower) / QUERY_LEVELS as f32;
        let mut bit_planes = std::array::from_fn(|_| vec![0u64; rotated.len().div_ceil(64)]);
        let mut sum_quantized = 0u64;

        if delta > 0.0 {
            for (dim, &value) in rotated.iter().enumerate() {
                let scaled = (value - lower) / delta;
                let random = uniform_01(seed ^ dim as u64);
                let quantized = (scaled + random).floor().clamp(0.0, QUERY_LEVELS as f32) as u32;
                sum_quantized += quantized as u64;
                for (bit, plane) in bit_planes.iter_mut().enumerate() {
                    if quantized & (1 << bit) != 0 {
                        plane[dim / 64] |= 1u64 << (dim % 64);
                    }
                }
            }
        }

        Self {
            bit_planes,
            lower,
            delta,
            sum_quantized,
            padded_dim: rotated.len(),
        }
    }

    /// Approximate `<sign(code)/sqrt(D), rotated_query>` using Equation 20.
    pub(super) fn code_dot(&self, code_words: &[u64]) -> f32 {
        debug_assert_eq!(code_words.len(), self.bit_planes[0].len());
        let (ones, selected_sum) = bit_counts(code_words, &self.bit_planes);
        let numerator = 2.0 * self.delta * selected_sum as f32 + 2.0 * self.lower * ones as f32
            - self.delta * self.sum_quantized as f32
            - self.padded_dim as f32 * self.lower;
        numerator / (self.padded_dim as f32).sqrt()
    }

    /// Hoeffding bound from Equation 66 for stochastic query quantization.
    pub(super) fn error_radius(&self, epsilon: f32) -> f32 {
        self.delta * epsilon
    }
}

fn bit_counts(code_words: &[u64], planes: &[Vec<u64>; QUERY_BITS]) -> (u64, u64) {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: feature detection guards the target-feature function and every
        // plane has the same word length as `code_words`.
        return unsafe { aarch64::bit_counts(code_words, planes) };
    }
    portable_bit_counts(code_words, planes)
}

fn portable_bit_counts(code_words: &[u64], planes: &[Vec<u64>; QUERY_BITS]) -> (u64, u64) {
    let mut ones = 0u64;
    let mut selected_sum = 0u64;
    for (word_idx, &code) in code_words.iter().enumerate() {
        ones += code.count_ones() as u64;
        for (bit, plane) in planes.iter().enumerate() {
            selected_sum += ((code & plane[word_idx]).count_ones() as u64) << bit;
        }
    }
    (ones, selected_sum)
}

fn uniform_01(seed: u64) -> f32 {
    (splitmix64(seed) >> 40) as f32 / (1u32 << 24) as f32
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = value;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use std::arch::aarch64::{vaddvq_u8, vandq_u8, vcntq_u8, vld1q_u8};

    use super::QUERY_BITS;

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn bit_counts(
        code_words: &[u64],
        planes: &[Vec<u64>; QUERY_BITS],
    ) -> (u64, u64) {
        let mut ones = 0u64;
        let mut selected_sum = 0u64;
        let mut word_idx = 0;
        while word_idx + 2 <= code_words.len() {
            let code = unsafe { vld1q_u8(code_words.as_ptr().add(word_idx).cast()) };
            ones += vaddvq_u8(vcntq_u8(code)) as u64;
            for (bit, plane) in planes.iter().enumerate() {
                let query = unsafe { vld1q_u8(plane.as_ptr().add(word_idx).cast()) };
                selected_sum += (vaddvq_u8(vcntq_u8(vandq_u8(code, query))) as u64) << bit;
            }
            word_idx += 2;
        }
        while word_idx < code_words.len() {
            let code = code_words[word_idx];
            ones += code.count_ones() as u64;
            for (bit, plane) in planes.iter().enumerate() {
                selected_sum += ((code & plane[word_idx]).count_ones() as u64) << bit;
            }
            word_idx += 1;
        }
        (ones, selected_sum)
    }
}

#[cfg(test)]
mod tests {
    use super::{PackedQuery, portable_bit_counts};

    #[test]
    fn packed_dot_matches_explicit_dequantization() {
        let rotated = [-0.8, -0.3, 0.1, 0.7, 0.4, -0.2, 0.9, -0.6];
        let query = PackedQuery::new(&rotated, 42);
        let code = [0b1011_0101u64];

        let packed = query.code_dot(&code);
        let explicit = (0..rotated.len())
            .map(|dim| {
                let quantized = query
                    .bit_planes
                    .iter()
                    .enumerate()
                    .map(|(bit, plane)| ((plane[0] >> dim) & 1) << bit)
                    .sum::<u64>();
                let value = query.lower + query.delta * quantized as f32;
                if code[0] & (1 << dim) != 0 {
                    value
                } else {
                    -value
                }
            })
            .sum::<f32>()
            / (rotated.len() as f32).sqrt();
        assert!((packed - explicit).abs() < 1e-6);
    }

    #[test]
    fn selected_bit_counts_match_portable_reference() {
        let rotated = (0..256)
            .map(|dim| (dim as f32 * 0.17).sin())
            .collect::<Vec<_>>();
        let query = PackedQuery::new(&rotated, 99);
        let code = [
            0x0123_4567_89ab_cdef,
            0xfedc_ba98_7654_3210,
            0xaaaa_5555_ffff_0000,
            0x1357_9bdf_2468_ace0,
        ];
        assert_eq!(
            super::bit_counts(&code, &query.bit_planes),
            portable_bit_counts(&code, &query.bit_planes)
        );
    }

    #[test]
    fn stochastic_query_quantization_is_unbiased_on_average() {
        let rotated = [-0.8, -0.3, 0.1, 0.7, 0.4, -0.2, 0.9, -0.6];
        let code = [0b1011_0101u64];
        let exact = rotated
            .iter()
            .enumerate()
            .map(|(dim, value)| {
                if code[0] & (1 << dim) != 0 {
                    *value
                } else {
                    -*value
                }
            })
            .sum::<f32>()
            / (rotated.len() as f32).sqrt();
        let mean = (0..4096u64)
            .map(|seed| PackedQuery::new(&rotated, seed).code_dot(&code))
            .sum::<f32>()
            / 4096.0;
        assert!((mean - exact).abs() < 0.002, "mean={mean}, exact={exact}");
    }
}
