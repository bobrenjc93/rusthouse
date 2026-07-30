use crate::error::{Error, Result};
use crate::value::ValueRef;

pub(crate) const DEFAULT_HLL_PRECISION: u8 = 12;
pub(crate) const MIN_HLL_PRECISION: u8 = 4;
pub(crate) const MAX_HLL_PRECISION: u8 = 18;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;

/// A fixed-size HyperLogLog register set.
///
/// Registers use one byte each. Updates and merges are order-independent, so
/// partial aggregation can combine states by taking each register's maximum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HyperLogLog {
    precision: u8,
    registers: Box<[u8]>,
}

impl HyperLogLog {
    pub(crate) fn new(precision: u8) -> Result<Self> {
        debug_assert!((MIN_HLL_PRECISION..=MAX_HLL_PRECISION).contains(&precision));
        let register_count = register_bytes(precision);
        let mut registers = Vec::new();
        registers.try_reserve_exact(register_count).map_err(|_| {
            Error::InvalidQuery(format!(
                "unable to allocate {register_count} bytes for APPROX_COUNT_DISTINCT state"
            ))
        })?;
        registers.resize(register_count, 0);
        Ok(Self {
            precision,
            registers: registers.into_boxed_slice(),
        })
    }

    pub(crate) fn insert(&mut self, value: ValueRef<'_>) {
        self.insert_hash(stable_scalar_hash(value));
    }

    fn insert_hash(&mut self, hash: u64) {
        let index = (hash >> (u64::BITS - u32::from(self.precision))) as usize;
        let remaining = hash << self.precision;
        let maximum_rank = u64::BITS - u32::from(self.precision) + 1;
        let rank = (remaining.leading_zeros() + 1).min(maximum_rank) as u8;
        self.registers[index] = self.registers[index].max(rank);
    }

    #[allow(
        dead_code,
        reason = "the serial executor does not merge partial states yet; this is the merge contract"
    )]
    pub(crate) fn merge(&mut self, other: &Self) -> Result<()> {
        if self.precision != other.precision {
            return Err(Error::InvalidQuery(format!(
                "cannot merge APPROX_COUNT_DISTINCT states with precisions {} and {}",
                self.precision, other.precision
            )));
        }
        for (register, incoming) in self.registers.iter_mut().zip(&other.registers) {
            *register = (*register).max(*incoming);
        }
        Ok(())
    }

    pub(crate) fn estimate(&self) -> Result<i64> {
        let register_count = self.registers.len() as f64;
        let harmonic_sum = self
            .registers
            .iter()
            .map(|register| 2.0_f64.powi(-i32::from(*register)))
            .sum::<f64>();
        let raw = alpha(self.registers.len()) * register_count * register_count / harmonic_sum;
        let zero_registers = self
            .registers
            .iter()
            .filter(|register| **register == 0)
            .count();
        let corrected = if raw <= 2.5 * register_count && zero_registers > 0 {
            register_count * (register_count / zero_registers as f64).ln()
        } else {
            raw
        };
        let rounded = corrected.round();
        if !rounded.is_finite() || !(0.0..I64_UPPER_EXCLUSIVE).contains(&rounded) {
            return Err(Error::NumericOverflow(
                "APPROX_COUNT_DISTINCT estimate".to_owned(),
            ));
        }
        Ok(rounded as i64)
    }

    #[cfg(test)]
    fn register_storage_bytes(&self) -> usize {
        self.registers.len() * std::mem::size_of::<u8>()
    }
}

pub(crate) fn register_bytes(precision: u8) -> usize {
    1_usize << precision
}

fn alpha(register_count: usize) -> f64 {
    match register_count {
        16 => 0.673,
        32 => 0.697,
        64 => 0.709,
        _ => 0.7213 / (1.0 + 1.079 / register_count as f64),
    }
}

/// Hashes the canonical byte representation of every physical scalar type.
/// Type tags prevent equal byte patterns from different SQL types colliding.
pub(crate) fn stable_scalar_hash(value: ValueRef<'_>) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    match value {
        ValueRef::Int64(value) => {
            write_hash(&mut hash, 0, &value.to_le_bytes());
        }
        ValueRef::Float64(value) => {
            let bits = if value == 0.0 { 0 } else { value.to_bits() };
            write_hash(&mut hash, 1, &bits.to_le_bytes());
        }
        ValueRef::Bool(value) => {
            write_hash(&mut hash, 2, &[u8::from(value)]);
        }
        ValueRef::String(value) => {
            write_hash(&mut hash, 3, value.as_bytes());
        }
    }

    // The FNV stream defines the byte-level contract; this finalizer gives HLL
    // well-distributed high bits even for sequential fixed-width integers.
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

fn write_hash(hash: &mut u64, type_tag: u8, bytes: &[u8]) {
    *hash ^= u64::from(type_tag);
    *hash = hash.wrapping_mul(FNV_PRIME);
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_hash_covers_all_scalar_types_and_canonicalizes_zero() {
        let hashes = [
            stable_scalar_hash(ValueRef::Int64(42)),
            stable_scalar_hash(ValueRef::Float64(42.0)),
            stable_scalar_hash(ValueRef::Bool(true)),
            stable_scalar_hash(ValueRef::String("42")),
        ];
        assert_eq!(
            hashes,
            [
                12_876_536_547_461_868_382,
                547_523_690_985_772_662,
                9_130_320_004_850_793_785,
                13_573_022_244_758_131_044,
            ]
        );
        assert_eq!(
            hashes
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4
        );
        assert_eq!(
            stable_scalar_hash(ValueRef::Float64(0.0)),
            stable_scalar_hash(ValueRef::Float64(-0.0))
        );
    }

    #[test]
    fn duplicates_and_small_cardinalities_use_linear_counting() {
        let mut state = HyperLogLog::new(DEFAULT_HLL_PRECISION).expect("state allocation");
        for _ in 0..100 {
            for value in [11, 22, 33] {
                state.insert(ValueRef::Int64(value));
            }
        }
        assert_eq!(state.estimate().expect("estimate"), 3);
        assert_eq!(
            HyperLogLog::new(DEFAULT_HLL_PRECISION)
                .expect("state allocation")
                .estimate()
                .expect("empty estimate"),
            0
        );
    }

    #[test]
    fn merge_matches_single_state_and_rejects_different_precision() {
        let mut combined = HyperLogLog::new(10).expect("state allocation");
        let mut left = HyperLogLog::new(10).expect("state allocation");
        let mut right = HyperLogLog::new(10).expect("state allocation");
        for value in 0..20_000 {
            combined.insert(ValueRef::Int64(value));
            if value % 2 == 0 {
                left.insert(ValueRef::Int64(value));
            } else {
                right.insert(ValueRef::Int64(value));
            }
        }
        left.merge(&right).expect("matching states merge");
        assert_eq!(left, combined);
        assert!(
            left.merge(&HyperLogLog::new(11).expect("state allocation"))
                .is_err()
        );
    }

    #[test]
    fn estimate_is_deterministic_and_within_the_expected_error_envelope() {
        let mut forward = HyperLogLog::new(DEFAULT_HLL_PRECISION).expect("state allocation");
        let mut reverse = HyperLogLog::new(DEFAULT_HLL_PRECISION).expect("state allocation");
        for value in 0..100_000 {
            forward.insert(ValueRef::Int64(value));
        }
        for value in (0..100_000).rev() {
            reverse.insert(ValueRef::Int64(value));
        }
        let estimate = forward.estimate().expect("estimate");
        assert_eq!(forward, reverse);
        assert_eq!(estimate, 96_337);
        assert_eq!(estimate, reverse.estimate().expect("reverse estimate"));
        assert!(
            (estimate - 100_000).abs() <= 5_000,
            "estimate {estimate} exceeds a 5% error envelope"
        );
    }

    #[test]
    fn register_storage_is_fixed_by_precision() {
        let mut state = HyperLogLog::new(9).expect("state allocation");
        let bytes = state.register_storage_bytes();
        assert_eq!(bytes, 1 << 9);
        for value in 0..1_000_000 {
            state.insert(ValueRef::Int64(value));
        }
        assert_eq!(state.register_storage_bytes(), bytes);
    }

    #[test]
    fn estimates_are_checked_before_int64_conversion() {
        let mut state = HyperLogLog::new(MIN_HLL_PRECISION).expect("state allocation");
        state.registers.fill(u8::MAX);
        assert!(matches!(state.estimate(), Err(Error::NumericOverflow(_))));
    }
}
