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
    pub rows: Vec<Row>,
}

impl Dataset {
    pub fn generate(seed: u64, row_count: usize) -> Self {
        let mut random = SplitMix64::new(seed);
        let mut ids = (0..row_count).map(|value| value as i64).collect::<Vec<_>>();
        let mut high_key_ordinals = (0..row_count).collect::<Vec<_>>();
        shuffle(&mut ids, SplitMix64::new(seed ^ 0xa076_1d64_78bd_642f));
        shuffle(
            &mut high_key_ordinals,
            SplitMix64::new(seed ^ 0xe703_7ed1_a0b4_28db),
        );
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
            let high_key = format!("entity_{:08}", high_key_ordinals[index]);
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
                id: ids[index],
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

fn shuffle<T>(values: &mut [T], mut random: SplitMix64) {
    for index in (1..values.len()).rev() {
        let other = (random.next() % (index as u64 + 1)) as usize;
        values.swap(index, other);
    }
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
        assert!(low_keys.len() <= 8);
        assert_eq!(high_keys.len(), dataset.rows.len());
        assert_eq!(ids.len(), dataset.rows.len());
        assert_eq!(ids.first(), Some(&0));
        assert_eq!(ids.last(), Some(&1_999));
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

    #[test]
    fn seed_shuffles_ids_and_high_keys_without_changing_cardinality() {
        let first = Dataset::generate(100, 256);
        let second = Dataset::generate(101, 256);
        let ids = |dataset: &Dataset| dataset.rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let high_keys = |dataset: &Dataset| {
            dataset
                .rows
                .iter()
                .map(|row| row.high_key.clone())
                .collect::<Vec<_>>()
        };

        assert_ne!(ids(&first), ids(&second));
        assert_ne!(high_keys(&first), high_keys(&second));
        assert_eq!(
            ids(&first)
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            ids(&second)
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert_eq!(
            high_keys(&first)
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            high_keys(&second)
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
    }
}
