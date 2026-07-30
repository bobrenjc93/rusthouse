use std::fmt::Write as _;

use crate::seed::{SplitMix64, bounded, derive, mix};

pub const TABLE_NAME: &str = "parity_data";

const VALUE_DOMAIN: u64 = 0x7661_6c75_6573_5f31;
const HIGH_KEY_DOMAIN: u64 = 0x6869_6768_6b65_7931;
const ROW_ORDER_DOMAIN: u64 = 0x726f_775f_6f72_6431;

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
    pub rows: Vec<Row>,
}

impl Dataset {
    pub fn generate(seed: u64, row_count: usize) -> Self {
        let mut random = SplitMix64::new(derive(seed, VALUE_DOMAIN));
        let high_key_salt = derive(seed, HIGH_KEY_DOMAIN);
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
            // XOR followed by SplitMix's bijective finalizer maps every logical
            // index to a unique, seed-sensitive fixed-width key.
            let high_key = format!("entity_{:016x}", mix(index as u64 ^ high_key_salt));
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
                id: index as i64,
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

        let mut order_random = SplitMix64::new(derive(seed, ROW_ORDER_DOMAIN));
        for index in (1..rows.len()).rev() {
            let swap_index = bounded(order_random.next(), index + 1);
            rows.swap(index, swap_index);
        }

        Self { seed, rows }
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

fn escape_sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_reproducible() {
        assert_eq!(Dataset::generate(42, 128), Dataset::generate(42, 128));
    }

    #[test]
    fn generated_values_are_reproducible_and_seed_sensitive() {
        let values_by_id = |seed| {
            let mut rows = Dataset::generate(seed, 64).rows;
            rows.sort_by_key(|row| row.id);
            for row in &mut rows {
                row.high_key.clear();
            }
            rows
        };

        assert_eq!(values_by_id(41), values_by_id(41));
        assert_ne!(values_by_id(41), values_by_id(42));
    }

    #[test]
    fn physical_order_is_reproducible_and_seed_sensitive() {
        let ids = |seed| {
            Dataset::generate(seed, 128)
                .rows
                .into_iter()
                .map(|row| row.id)
                .collect::<Vec<_>>()
        };

        assert_eq!(ids(41), ids(41));
        assert_ne!(ids(41), ids(42));
        assert_ne!(ids(41), (0..128).collect::<Vec<_>>());
    }

    #[test]
    fn high_keys_are_fixed_width_unique_reproducible_and_seed_sensitive() {
        let keys_by_id = |seed| {
            Dataset::generate(seed, 128)
                .rows
                .into_iter()
                .map(|row| (row.id, row.high_key))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let first = keys_by_id(41);
        let repeated = keys_by_id(41);
        let second = keys_by_id(42);

        assert_eq!(first, repeated);
        assert_ne!(first, second);
        assert!(first.values().all(|key| key.len() == "entity_".len() + 16));
        assert_eq!(
            first
                .values()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            first.len()
        );
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
        let ids = dataset
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<std::collections::BTreeSet<_>>();

        assert!(near_zero > dataset.rows.len() * 4 / 5);
        assert_eq!(low_keys.len(), 8);
        assert_eq!(high_keys.len(), dataset.rows.len());
        assert_eq!(ids, (0..dataset.rows.len() as i64).collect());
        assert!(high_keys.iter().all(|key| key.len() == 23));
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
