use std::fmt::Write as _;

pub const TABLE_NAME: &str = "parity_data";

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: i64,
    pub uniform_num: i64,
    pub skewed_num: i64,
    pub score: f64,
    pub low_key: String,
    pub high_key: String,
    pub payload: String,
    pub flag: bool,
    pub large_int: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    pub seed: u64,
    pub parameters: DatasetParameters,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatasetParameters {
    pub id_permutation: PermutationParameters,
    pub high_key_permutation: PermutationParameters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermutationParameters {
    pub multiplier: usize,
    pub offset: usize,
}

impl PermutationParameters {
    fn derive(seed: u64, row_count: usize, domain: u64) -> Self {
        if row_count <= 1 {
            return Self {
                multiplier: 1,
                offset: 0,
            };
        }

        let mut multiplier = (mix64(seed ^ domain) as usize) % row_count;
        if multiplier == 0 {
            multiplier = 1;
        }
        while greatest_common_divisor(multiplier, row_count) != 1 {
            multiplier += 1;
            if multiplier == row_count {
                multiplier = 1;
            }
        }
        let offset = (mix64(seed ^ domain.rotate_left(29)) as usize) % row_count;
        Self { multiplier, offset }
    }

    fn apply(self, index: usize, row_count: usize) -> usize {
        if row_count <= 1 {
            return 0;
        }
        ((self.multiplier as u128 * index as u128 + self.offset as u128) % row_count as u128)
            as usize
    }
}

impl Dataset {
    pub fn generate(seed: u64, row_count: usize) -> Self {
        let mut random = SplitMix64::new(seed);
        let parameters = DatasetParameters {
            id_permutation: PermutationParameters::derive(seed, row_count, 0xa076_1d64_78bd_642f),
            high_key_permutation: PermutationParameters::derive(
                seed,
                row_count,
                0xe703_7ed1_a0b4_28db,
            ),
        };
        let low_keys = [
            "amber", "blue", "coral", "green", "indigo", "red", "silver", "violet",
        ];
        let words = [
            "a",
            "medium",
            "a longer payload",
            "comma,inside",
            "quote's payload",
            "symbols-_!",
            "repeated words repeated words",
            "z",
        ];
        let mut rows = Vec::with_capacity(row_count);

        for index in 0..row_count {
            let id = parameters.id_permutation.apply(index, row_count);
            let high_key = parameters.high_key_permutation.apply(index, row_count);
            let uniform_num = if index == 0 {
                -1_000_000
            } else if index == 1 {
                1_000_000
            } else {
                (random.next() % 2_000_001) as i64 - 1_000_000
            };
            let skewed_num = if index == 0 {
                -750_000
            } else if random.next() % 10 < 9 {
                (random.next() % 17) as i64 - 8
            } else {
                (random.next() % 1_500_001) as i64 - 750_000
            };
            let score = ((random.next() % 160_001) as i64 - 80_000) as f64 / 8.0;
            let low_key = low_keys[(random.next() as usize) % low_keys.len()].to_owned();
            let high_key = format!("entity_{high_key:08}");
            let word = words[(random.next() as usize) % words.len()];
            let suffix_len = (random.next() % 13) as usize;
            let payload = if index == 0 {
                "quote's payload-0".to_owned()
            } else if index == 1 {
                "comma,inside-and-a-much-longer-payload-1xxxxxxxx".to_owned()
            } else {
                format!("{word}-{}{}", index % 97, "x".repeat(suffix_len))
            };
            let flag = if index == 0 {
                false
            } else if index == 1 {
                true
            } else {
                random.next() & 1 == 0
            };
            let magnitude = 4_000_000_000_000_000_i64 + (random.next() % 1_000_000) as i64;
            let large_int = if index == 0 || (index > 1 && random.next() & 1 == 0) {
                -magnitude
            } else {
                magnitude
            };

            rows.push(Row {
                id: id as i64,
                uniform_num,
                skewed_num,
                score,
                low_key,
                high_key,
                payload,
                flag,
                large_int,
            });
        }

        Self {
            seed,
            parameters,
            rows,
        }
    }

    pub fn setup_sql(&self) -> String {
        let mut sql = String::with_capacity(self.rows.len().saturating_mul(150));
        writeln!(
            sql,
            "CREATE TABLE {TABLE_NAME} (id Int64, uniform_num Int64, skewed_num Int64, score Float64, low_key String, high_key String, payload String, flag Bool, large_int Int64);"
        )
        .expect("writing to String cannot fail");
        write!(sql, "INSERT INTO {TABLE_NAME} VALUES ").expect("writing to String cannot fail");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                sql.push(',');
            }
            write!(
                sql,
                "({},{},{},{:.3},'{}','{}','{}',{},{})",
                row.id,
                row.uniform_num,
                row.skewed_num,
                row.score,
                escape_sql_string(&row.low_key),
                escape_sql_string(&row.high_key),
                escape_sql_string(&row.payload),
                row.flag,
                row.large_int,
            )
            .expect("writing to String cannot fail");
        }
        sql.push_str(";\n");
        sql
    }
}

fn greatest_common_divisor(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn escape_sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

#[derive(Debug, Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_reproducible() {
        assert_eq!(Dataset::generate(42, 128), Dataset::generate(42, 128));
    }

    #[test]
    fn runtime_seed_changes_generated_data() {
        assert_ne!(Dataset::generate(41, 64), Dataset::generate(42, 64));
    }

    #[test]
    fn seed_permutations_change_structure_without_changing_key_sets() {
        let left = Dataset::generate(41, 257);
        let right = Dataset::generate(42, 257);
        let ids = |dataset: &Dataset| dataset.rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let high_keys = |dataset: &Dataset| {
            dataset
                .rows
                .iter()
                .map(|row| row.high_key.clone())
                .collect::<Vec<_>>()
        };

        assert_ne!(left.parameters, right.parameters);
        assert_ne!(ids(&left), ids(&right));
        assert_ne!(high_keys(&left), high_keys(&right));

        let mut left_ids = ids(&left);
        let mut right_ids = ids(&right);
        left_ids.sort_unstable();
        right_ids.sort_unstable();
        assert_eq!(left_ids, (0..257).map(i64::from).collect::<Vec<_>>());
        assert_eq!(right_ids, left_ids);

        let mut left_high_keys = high_keys(&left);
        let mut right_high_keys = high_keys(&right);
        left_high_keys.sort_unstable();
        right_high_keys.sort_unstable();
        assert_eq!(left_high_keys, right_high_keys);
        assert_eq!(left_high_keys.len(), 257);
        assert_eq!(
            left_high_keys.first().map(String::as_str),
            Some("entity_00000000")
        );
        assert_eq!(
            left_high_keys.last().map(String::as_str),
            Some("entity_00000256")
        );
    }

    #[test]
    fn permutations_are_bijective_at_composite_benchmark_scales() {
        for seed in [0, 1, 20_260_729, u64::MAX] {
            for row_count in [256, 1_000, 2_048] {
                let dataset = Dataset::generate(seed, row_count);
                let ids = dataset
                    .rows
                    .iter()
                    .map(|row| row.id)
                    .collect::<std::collections::BTreeSet<_>>();
                let high_keys = dataset
                    .rows
                    .iter()
                    .map(|row| &row.high_key)
                    .collect::<std::collections::BTreeSet<_>>();

                assert_eq!(ids.len(), row_count);
                assert_eq!(high_keys.len(), row_count);
                assert_eq!(
                    greatest_common_divisor(
                        dataset.parameters.id_permutation.multiplier,
                        row_count
                    ),
                    1
                );
                assert_eq!(
                    greatest_common_divisor(
                        dataset.parameters.high_key_permutation.multiplier,
                        row_count
                    ),
                    1
                );
            }
        }
    }

    #[test]
    fn generated_shapes_include_required_extremes_and_cardinalities() {
        let dataset = Dataset::generate(7, 2_000);
        let near_zero = dataset
            .rows
            .iter()
            .filter(|row| (-8..=8).contains(&row.skewed_num))
            .count();
        let low_keys = dataset
            .rows
            .iter()
            .map(|row| &row.low_key)
            .collect::<std::collections::BTreeSet<_>>();
        let high_keys = dataset
            .rows
            .iter()
            .map(|row| &row.high_key)
            .collect::<std::collections::BTreeSet<_>>();

        assert!(near_zero > dataset.rows.len() * 4 / 5);
        assert!(low_keys.len() <= 8);
        assert_eq!(high_keys.len(), dataset.rows.len());
        assert!(dataset.rows.iter().any(|row| row.uniform_num < 0));
        assert!(dataset.rows.iter().any(|row| row.uniform_num > 0));
        assert!(
            dataset
                .rows
                .iter()
                .any(|row| row.large_int < -1_000_000_000_000)
        );
        assert!(
            dataset
                .rows
                .iter()
                .any(|row| row.large_int > 1_000_000_000_000)
        );
        assert!(dataset.rows.iter().any(|row| row.flag));
        assert!(dataset.rows.iter().any(|row| !row.flag));
        assert!(
            dataset
                .rows
                .iter()
                .map(|row| row.payload.len())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                > 10
        );
        assert!(dataset.rows.iter().any(|row| row.payload.contains(',')));
        assert!(dataset.rows.iter().any(|row| row.payload.contains('\'')));
    }

    #[test]
    fn sql_escapes_strings_for_both_engines() {
        let dataset = Dataset::generate(9, 32);
        let sql = dataset.setup_sql();
        assert!(sql.contains("quote''s payload"));
        assert!(sql.starts_with("CREATE TABLE parity_data"));
        assert!(sql.ends_with(";\n"));
    }
}
