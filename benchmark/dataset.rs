use std::fmt::Write as _;

pub const TABLE_NAME: &str = "parity_data";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchemaProfile {
    NumericHeavy,
    StringHeavy,
    WideMixed,
}

impl SchemaProfile {
    pub const ALL: [Self; 3] = [Self::NumericHeavy, Self::StringHeavy, Self::WideMixed];

    pub fn name(self) -> &'static str {
        match self {
            Self::NumericHeavy => "numeric_heavy",
            Self::StringHeavy => "string_heavy",
            Self::WideMixed => "wide_mixed",
        }
    }

    pub fn seed_salt(self) -> u64 {
        match self {
            Self::NumericHeavy => 0x243f_6a88_85a3_08d3,
            Self::StringHeavy => 0x1319_8a2e_0370_7344,
            Self::WideMixed => 0xa409_3822_299f_31d0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: i64,
    pub uniform_num: i64,
    pub skewed_num: i64,
    pub large_int: i64,
    pub aux_int_a: i64,
    pub aux_int_b: i64,
    pub score: f64,
    pub aux_score: f64,
    pub bucket: i64,
    pub low_key: String,
    pub high_key: String,
    pub payload: String,
    pub region: String,
    pub code: String,
    pub description: String,
    pub label: String,
    pub flag: bool,
    pub secondary_flag: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dataset {
    pub profile: SchemaProfile,
    pub seed: u64,
    pub rows: Vec<Row>,
}

impl Dataset {
    pub fn generate(profile: SchemaProfile, seed: u64, row_count: usize) -> Self {
        let mut random = SplitMix64::new(seed);
        let low_keys = [
            "amber", "blue", "coral", "green", "indigo", "red", "silver", "violet",
        ];
        let regions = ["east", "north", "south", "west"];
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
            let high_nonce = random.next() % 1_000_000;
            let high_key = format!("entity_{high_nonce:06}_{index:08}");
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
            let aux_int_a = (random.next() % 1_000_001) as i64 - 500_000;
            let aux_int_b = if random.next() % 5 < 4 {
                (random.next() % 65) as i64 - 32
            } else {
                (random.next() % 4_000_001) as i64 - 2_000_000
            };
            let aux_score = ((random.next() % 80_001) as i64 - 40_000) as f64 / 4.0;
            let bucket = (random.next() % 32) as i64;
            let region = regions[(random.next() as usize) % regions.len()].to_owned();
            let code = format!("code_{:03}", random.next() % 257);
            let description_word = words[(random.next() as usize) % words.len()];
            let description = format!("{description_word}:{}", random.next() % 10_000);
            let label = format!("label_{:03}", random.next() % 193);
            let secondary_flag = random.next() & 3 == 0;

            rows.push(Row {
                id: index as i64,
                uniform_num,
                skewed_num,
                large_int,
                aux_int_a,
                aux_int_b,
                score,
                aux_score,
                bucket,
                low_key,
                high_key,
                payload,
                region,
                code,
                description,
                label,
                flag,
                secondary_flag,
            });
        }

        Self {
            profile,
            seed,
            rows,
        }
    }

    pub fn setup_sql(&self) -> String {
        let mut sql = String::with_capacity(self.rows.len().saturating_mul(300));
        writeln!(sql, "{}", self.create_table_sql()).expect("writing to String cannot fail");
        write!(sql, "INSERT INTO {TABLE_NAME} VALUES ").expect("writing to String cannot fail");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                sql.push(',');
            }
            match self.profile {
                SchemaProfile::NumericHeavy => write_numeric_row(&mut sql, row),
                SchemaProfile::StringHeavy => write_string_row(&mut sql, row),
                SchemaProfile::WideMixed => write_wide_row(&mut sql, row),
            }
            .expect("writing to String cannot fail");
        }
        sql.push_str(";\n");
        sql
    }

    pub fn create_table_sql(&self) -> &'static str {
        match self.profile {
            SchemaProfile::NumericHeavy => {
                "CREATE TABLE parity_data (id Int64, uniform_num Int64, skewed_num Int64, large_int Int64, aux_int_a Int64, aux_int_b Int64, score Float64, aux_score Float64, bucket Int64, flag Bool, label String);"
            }
            SchemaProfile::StringHeavy => {
                "CREATE TABLE parity_data (id Int64, low_key String, high_key String, payload String, region String, code String, description String, label String, flag Bool, uniform_num Int64, score Float64);"
            }
            SchemaProfile::WideMixed => {
                "CREATE TABLE parity_data (id Int64, uniform_num Int64, skewed_num Int64, large_int Int64, aux_int_a Int64, aux_int_b Int64, score Float64, aux_score Float64, bucket Int64, low_key String, high_key String, payload String, region String, code String, description String, label String, flag Bool, secondary_flag Bool);"
            }
        }
    }
}

fn write_numeric_row(output: &mut String, row: &Row) -> std::fmt::Result {
    write!(
        output,
        "({},{},{},{},{},{},{:.3},{:.3},{},{},'{}')",
        row.id,
        row.uniform_num,
        row.skewed_num,
        row.large_int,
        row.aux_int_a,
        row.aux_int_b,
        row.score,
        row.aux_score,
        row.bucket,
        row.flag,
        escape_sql_string(&row.label),
    )
}

fn write_string_row(output: &mut String, row: &Row) -> std::fmt::Result {
    write!(
        output,
        "({},'{}','{}','{}','{}','{}','{}','{}',{},{},{:.3})",
        row.id,
        escape_sql_string(&row.low_key),
        escape_sql_string(&row.high_key),
        escape_sql_string(&row.payload),
        escape_sql_string(&row.region),
        escape_sql_string(&row.code),
        escape_sql_string(&row.description),
        escape_sql_string(&row.label),
        row.flag,
        row.uniform_num,
        row.score,
    )
}

fn write_wide_row(output: &mut String, row: &Row) -> std::fmt::Result {
    write!(
        output,
        "({},{},{},{},{},{},{:.3},{:.3},{},'{}','{}','{}','{}','{}','{}','{}',{},{})",
        row.id,
        row.uniform_num,
        row.skewed_num,
        row.large_int,
        row.aux_int_a,
        row.aux_int_b,
        row.score,
        row.aux_score,
        row.bucket,
        escape_sql_string(&row.low_key),
        escape_sql_string(&row.high_key),
        escape_sql_string(&row.payload),
        escape_sql_string(&row.region),
        escape_sql_string(&row.code),
        escape_sql_string(&row.description),
        escape_sql_string(&row.label),
        row.flag,
        row.secondary_flag,
    )
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
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_profile_is_reproducible_and_profile_specific() {
        for profile in SchemaProfile::ALL {
            assert_eq!(
                Dataset::generate(profile, 42, 128),
                Dataset::generate(profile, 42, 128)
            );
        }
        let schemas = SchemaProfile::ALL
            .into_iter()
            .map(|profile| Dataset::generate(profile, 42, 1).create_table_sql())
            .collect::<BTreeSet<_>>();
        assert_eq!(schemas.len(), SchemaProfile::ALL.len());
    }

    #[test]
    fn runtime_seed_changes_generated_data_for_every_profile() {
        for profile in SchemaProfile::ALL {
            assert_ne!(
                Dataset::generate(profile, 41, 64),
                Dataset::generate(profile, 42, 64)
            );
        }
    }

    #[test]
    fn generated_shapes_include_required_extremes_and_cardinalities() {
        let dataset = Dataset::generate(SchemaProfile::WideMixed, 7, 2_000);
        let near_zero = dataset
            .rows
            .iter()
            .filter(|row| (-8..=8).contains(&row.skewed_num))
            .count();
        let low_keys = dataset
            .rows
            .iter()
            .map(|row| &row.low_key)
            .collect::<BTreeSet<_>>();
        let high_keys = dataset
            .rows
            .iter()
            .map(|row| &row.high_key)
            .collect::<BTreeSet<_>>();

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
                .collect::<BTreeSet<_>>()
                .len()
                > 10
        );
        assert!(dataset.rows.iter().any(|row| row.payload.contains(',')));
        assert!(dataset.rows.iter().any(|row| row.payload.contains('\'')));
    }

    #[test]
    fn profile_schemas_have_deliberately_different_shapes() {
        let numeric = Dataset::generate(SchemaProfile::NumericHeavy, 9, 2);
        let strings = Dataset::generate(SchemaProfile::StringHeavy, 9, 2);
        let wide = Dataset::generate(SchemaProfile::WideMixed, 9, 2);

        assert_eq!(numeric.create_table_sql().matches("Int64").count(), 7);
        assert_eq!(numeric.create_table_sql().matches("String").count(), 1);
        assert_eq!(strings.create_table_sql().matches("String").count(), 7);
        assert_eq!(strings.create_table_sql().matches("Int64").count(), 2);
        assert_eq!(
            wide.create_table_sql().matches(" Int64").count()
                + wide.create_table_sql().matches(" Float64").count()
                + wide.create_table_sql().matches(" String").count()
                + wide.create_table_sql().matches(" Bool").count(),
            18
        );
        assert_eq!(wide.create_table_sql().matches("Bool").count(), 2);
    }

    #[test]
    fn sql_escapes_strings_for_every_profile() {
        for profile in SchemaProfile::ALL {
            let dataset = Dataset::generate(profile, 9, 32);
            let sql = dataset.setup_sql();
            if profile != SchemaProfile::NumericHeavy {
                assert!(sql.contains("quote''s payload"));
            }
            assert!(sql.starts_with("CREATE TABLE parity_data"));
            assert!(sql.ends_with(";\n"));
        }
    }
}
