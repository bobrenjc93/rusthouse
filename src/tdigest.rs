const MAX_CENTROIDS: usize = 256;
const COMPRESSION: f64 = 64.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Centroid {
    mean: f64,
    weight: u64,
}

impl Centroid {
    const EMPTY: Self = Self {
        mean: 0.0,
        weight: 0,
    };
}

/// A deterministic quantile digest whose storage does not grow with its input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TDigest {
    centroids: [Centroid; MAX_CENTROIDS],
    len: usize,
    total_weight: u64,
    min: f64,
    max: f64,
}

impl Default for TDigest {
    fn default() -> Self {
        Self {
            centroids: [Centroid::EMPTY; MAX_CENTROIDS],
            len: 0,
            total_weight: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
}

impl TDigest {
    pub(crate) fn add(&mut self, value: f64) -> bool {
        self.add_weighted(value, 1)
    }

    /// Merges another bounded digest without retaining any of its allocations.
    #[allow(dead_code)]
    pub(crate) fn merge(&mut self, other: &Self) -> bool {
        if other.len == 0 {
            return true;
        }
        if self.len == 0 {
            self.clone_from(other);
            return true;
        }
        if self.total_weight.checked_add(other.total_weight).is_none() {
            return false;
        }

        let min = self.min.min(other.min);
        let max = self.max.max(other.max);
        for centroid in &other.centroids[..other.len] {
            let added = self.add_weighted(centroid.mean, centroid.weight);
            debug_assert!(added, "total merge weight was checked up front");
        }
        self.min = min;
        self.max = max;
        true
    }

    pub(crate) fn quantile(&self, level: f64) -> Option<f64> {
        if self.len == 0 {
            return None;
        }
        if level <= 0.0 {
            return Some(self.min);
        }
        if level >= 1.0 {
            return Some(self.max);
        }

        let target = level * (self.total_weight - 1) as f64;
        let mut cumulative = 0_u64;
        let mut previous_position = 0.0;
        let mut previous_mean = self.min;

        for centroid in &self.centroids[..self.len] {
            let position = cumulative as f64 + (centroid.weight - 1) as f64 / 2.0;
            if target <= position {
                return Some(interpolate(
                    previous_mean,
                    centroid.mean,
                    previous_position,
                    position,
                    target,
                ));
            }
            cumulative += centroid.weight;
            previous_position = position;
            previous_mean = centroid.mean;
        }

        Some(interpolate(
            previous_mean,
            self.max,
            previous_position,
            (self.total_weight - 1) as f64,
            target,
        ))
    }

    fn add_weighted(&mut self, mean: f64, weight: u64) -> bool {
        if weight == 0 || !mean.is_finite() {
            return false;
        }
        let Some(total_weight) = self.total_weight.checked_add(weight) else {
            return false;
        };

        let incoming = Centroid { mean, weight };
        if self.len < MAX_CENTROIDS {
            self.insert(incoming);
        } else {
            self.compact_with(incoming, total_weight);
        }
        self.total_weight = total_weight;
        self.min = self.min.min(mean);
        self.max = self.max.max(mean);
        true
    }

    fn insert(&mut self, centroid: Centroid) {
        let position =
            self.centroids[..self.len].partition_point(|existing| existing.mean <= centroid.mean);
        self.centroids.copy_within(position..self.len, position + 1);
        self.centroids[position] = centroid;
        self.len += 1;
    }

    fn compact_with(&mut self, incoming: Centroid, total_weight: u64) {
        let incoming_position =
            self.centroids[..self.len].partition_point(|existing| existing.mean <= incoming.mean);
        let mut best_pair = 0;
        let mut best_gap = f64::INFINITY;
        let mut found_eligible = false;
        let mut cumulative = 0_u64;

        for pair in 0..MAX_CENTROIDS {
            let left = self.conceptual_centroid(pair, incoming_position, incoming);
            let right = self.conceptual_centroid(pair + 1, incoming_position, incoming);
            let midpoint = cumulative as f64 + (left.weight + right.weight) as f64 / 2.0;
            let quantile = midpoint / total_weight as f64;
            let weight_limit =
                (4.0 * total_weight as f64 * quantile * (1.0 - quantile) / COMPRESSION).max(1.0);
            let eligible = (left.weight + right.weight) as f64 <= weight_limit;
            let gap = right.mean - left.mean;

            if (eligible && !found_eligible)
                || (eligible == found_eligible && gap.total_cmp(&best_gap).is_lt())
            {
                best_pair = pair;
                best_gap = gap;
                found_eligible = eligible;
            }
            cumulative += left.weight;
        }

        if best_pair + 1 == incoming_position {
            let existing = best_pair;
            self.centroids[existing] = merge_centroids(self.centroids[existing], incoming);
        } else if best_pair == incoming_position {
            let existing = incoming_position;
            self.centroids[existing] = merge_centroids(incoming, self.centroids[existing]);
        } else {
            let existing = if best_pair < incoming_position {
                best_pair
            } else {
                best_pair - 1
            };
            self.merge_adjacent(existing);
            self.insert(incoming);
        }
    }

    fn conceptual_centroid(
        &self,
        position: usize,
        incoming_position: usize,
        incoming: Centroid,
    ) -> Centroid {
        if position == incoming_position {
            incoming
        } else if position < incoming_position {
            self.centroids[position]
        } else {
            self.centroids[position - 1]
        }
    }

    fn merge_adjacent(&mut self, left: usize) {
        self.centroids[left] = merge_centroids(self.centroids[left], self.centroids[left + 1]);
        self.centroids.copy_within(left + 2..self.len, left + 1);
        self.len -= 1;
        self.centroids[self.len] = Centroid::EMPTY;
    }
}

fn merge_centroids(left: Centroid, right: Centroid) -> Centroid {
    let weight = left.weight + right.weight;
    let right_fraction = right.weight as f64 / weight as f64;
    let mean = if left.mean <= 0.0 && right.mean >= 0.0 {
        let left_fraction = left.weight as f64 / weight as f64;
        left.mean * left_fraction + right.mean * right_fraction
    } else {
        left.mean + (right.mean - left.mean) * right_fraction
    };
    Centroid { mean, weight }
}

fn interpolate(left: f64, right: f64, start: f64, end: f64, target: f64) -> f64 {
    if start == end || left == right {
        return left;
    }
    let fraction = (target - start) / (end - start);
    let value = if left <= 0.0 && right >= 0.0 {
        left * (1.0 - fraction) + right * fraction
    } else {
        left + (right - left) * fraction
    };
    value.clamp(left, right)
}

#[cfg(test)]
mod tests {
    use std::mem::{needs_drop, size_of};

    use super::*;

    #[test]
    fn small_inputs_use_exact_linear_quantiles() {
        let mut digest = TDigest::default();
        for value in [0.0, 10.0, 20.0, 30.0] {
            assert!(digest.add(value));
        }

        assert_eq!(digest.quantile(0.0), Some(0.0));
        assert_eq!(digest.quantile(0.5), Some(15.0));
        assert_eq!(digest.quantile(1.0), Some(30.0));
    }

    #[test]
    fn state_size_and_centroid_count_are_input_independent() {
        assert!(!needs_drop::<TDigest>());
        assert!(size_of::<TDigest>() <= MAX_CENTROIDS * size_of::<Centroid>() + 40);

        let mut digest = TDigest::default();
        for index in 0..100_000 {
            assert!(digest.add(((index * 7_919) % 100_003) as f64));
            assert!(digest.len <= MAX_CENTROIDS);
        }
        assert_eq!(size_of_val(&digest), size_of::<TDigest>());
    }

    #[test]
    fn construction_and_merge_are_reproducible() {
        fn digest_range(start: usize, end: usize) -> TDigest {
            let mut digest = TDigest::default();
            for index in start..end {
                assert!(digest.add(((index * 104_729) % 65_537) as f64));
            }
            digest
        }

        assert_eq!(digest_range(0, 20_000), digest_range(0, 20_000));

        let mut first = digest_range(0, 10_000);
        let second = digest_range(10_000, 20_000);
        assert!(first.merge(&second));
        let merged_once = first.clone();

        let mut repeated = digest_range(0, 10_000);
        assert!(repeated.merge(&second));
        assert_eq!(repeated, merged_once);
    }

    #[test]
    fn merged_uniform_data_stays_within_rank_error_bound() {
        let mut left = TDigest::default();
        let mut right = TDigest::default();
        for value in 0..50_000 {
            let target = if value % 2 == 0 {
                &mut left
            } else {
                &mut right
            };
            assert!(target.add(value as f64));
        }
        assert!(left.merge(&right));

        for level in [0.01, 0.1, 0.5, 0.9, 0.99] {
            let expected = level * 49_999.0;
            let actual = left.quantile(level).expect("non-empty digest");
            assert!((actual - expected).abs() <= 500.0, "{level}: {actual}");
        }
    }

    #[test]
    fn skew_and_outliers_preserve_distribution_and_endpoints() {
        let mut digest = TDigest::default();
        for _ in 0..9_900 {
            assert!(digest.add(1.0));
        }
        for value in 2..=100 {
            assert!(digest.add(value as f64));
        }
        assert!(digest.add(1_000_000.0));

        assert_eq!(digest.quantile(0.5), Some(1.0));
        assert!(digest.quantile(0.99).expect("quantile") <= 2.0);
        assert_eq!(digest.quantile(1.0), Some(1_000_000.0));
    }
}
