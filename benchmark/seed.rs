const SPLITMIX_INCREMENT: u64 = 0x9e37_79b9_7f4a_7c15;

pub fn derive(seed: u64, domain: u64) -> u64 {
    mix(seed ^ domain)
}

pub fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub fn bounded(value: u64, upper_bound: usize) -> usize {
    if upper_bound == 0 {
        return 0;
    }
    ((value as u128 * upper_bound as u128) >> 64) as usize
}

#[derive(Debug, Clone, Copy)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(SPLITMIX_INCREMENT);
        mix(self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_values_stay_in_range() {
        for upper_bound in 1..100 {
            for value in [0, 1, u64::MAX / 2, u64::MAX] {
                assert!(bounded(value, upper_bound) < upper_bound);
            }
        }
        assert_eq!(bounded(u64::MAX, 0), 0);
    }

    #[test]
    fn domains_are_reproducible_and_distinct() {
        assert_eq!(derive(42, 1), derive(42, 1));
        assert_ne!(derive(42, 1), derive(42, 2));
        assert_ne!(derive(41, 1), derive(42, 1));
    }
}
