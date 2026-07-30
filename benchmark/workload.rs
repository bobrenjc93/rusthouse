use std::fmt::Write as _;

use crate::digest::sha256_hex;
use crate::normalize::ColumnType;

pub const QUERY_SEQUENCE_METHODOLOGY: &str = "query_diverse_amplification_v3";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    FullScanAggregate,
    SelectiveFilter,
    CompoundFilter,
    NonselectiveFilter,
    LowCardinalityGroupBy,
    HighCardinalityGroupBy,
    OrderByLimit,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Self::FullScanAggregate => "full_scan_aggregate",
            Self::SelectiveFilter => "selective_filter",
            Self::CompoundFilter => "compound_filter",
            Self::NonselectiveFilter => "nonselective_filter",
            Self::LowCardinalityGroupBy => "low_cardinality_group_by",
            Self::HighCardinalityGroupBy => "high_cardinality_group_by",
            Self::OrderByLimit => "order_by_limit",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workload {
    pub name: &'static str,
    pub family: Family,
    pub sql: String,
    pub columns: Vec<(&'static str, ColumnType)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVariant {
    pub ordinal: usize,
    pub parameters: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySequence {
    pub seed: u64,
    pub sql: String,
    pub sha256: String,
    pub variants: Vec<ResolvedVariant>,
}

impl QuerySequence {
    pub fn query_count(&self) -> usize {
        self.variants.len()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.variants.is_empty() {
            return Err("query sequence must not be empty".to_owned());
        }
        if sha256_hex(self.sql.as_bytes()) != self.sha256 {
            return Err("query sequence SHA-256 does not match its SQL bytes".to_owned());
        }
        let statements = self.sql.lines().collect::<Vec<_>>();
        if statements.len() != self.variants.len()
            || statements
                .iter()
                .any(|statement| statement.is_empty() || !statement.ends_with(';'))
        {
            return Err(format!(
                "query sequence SQL does not contain exactly {} one-line statements",
                self.variants.len()
            ));
        }
        if self
            .variants
            .iter()
            .enumerate()
            .any(|(ordinal, variant)| variant.ordinal != ordinal)
        {
            return Err("query sequence ordinals are not contiguous".to_owned());
        }
        Ok(())
    }
}

impl Workload {
    pub fn query_sequence(
        &self,
        row_count: usize,
        runtime_seed: u64,
        count: usize,
    ) -> QuerySequence {
        assert!(row_count > 0, "query sequences require a nonempty dataset");
        assert!(count > 0, "query sequences require at least one query");
        let seed = derive_sequence_seed(runtime_seed, row_count, self.name);
        let mut random = SplitMix64::new(seed);
        let mut sql = String::new();
        let mut variants = Vec::with_capacity(count);

        for ordinal in 0..count {
            let (query, parameters) = self.resolve_variant(row_count, ordinal, &mut random);
            writeln!(sql, "{query}").expect("writing to String cannot fail");
            variants.push(ResolvedVariant {
                ordinal,
                parameters,
            });
        }
        let sha256 = sha256_hex(sql.as_bytes());
        QuerySequence {
            seed,
            sql,
            sha256,
            variants,
        }
    }

    fn resolve_variant(
        &self,
        row_count: usize,
        ordinal: usize,
        random: &mut SplitMix64,
    ) -> (String, Vec<(&'static str, String)>) {
        match self.name {
            "full_scan_aggregate" => {
                let lower_id = -(random.inclusive(1, 4_096) as i64);
                let upper_id = row_count as i64 + random.inclusive(1, 4_096) as i64;
                (
                    format!(
                        "SELECT COUNT(*) AS row_count, SUM(uniform_num) AS uniform_total, SUM(skewed_num) AS skewed_total, MIN(large_int) AS large_min, MAX(large_int) AS large_max, AVG(score) AS score_mean FROM parity_data WHERE id >= {lower_id} AND id < {upper_id};"
                    ),
                    parameters(&[
                        ("lower_id", lower_id.to_string()),
                        ("upper_id", upper_id.to_string()),
                    ]),
                )
            }
            "selective_point_filter" => {
                let start = random.below(row_count as u64) as usize;
                let selected_id = (start + ordinal) % row_count;
                (
                    format!(
                        "SELECT id, payload, large_int, flag FROM parity_data WHERE id = {selected_id} ORDER BY id;"
                    ),
                    parameters(&[("id", selected_id.to_string())]),
                )
            }
            "compound_filter_aggregate" => {
                let flag = random.next() & 1 == 0;
                let uniform_lt = random.inclusive(250_000, 1_250_000) as i64 - 1_000_000;
                let skewed_ge = random.inclusive(0, 9) as i64 - 4;
                (
                    format!(
                        "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE (flag = {flag} AND uniform_num < {uniform_lt}) OR (flag = {} AND skewed_num >= {skewed_ge});",
                        !flag
                    ),
                    parameters(&[
                        ("first_flag", flag.to_string()),
                        ("uniform_lt", uniform_lt.to_string()),
                        ("second_flag", (!flag).to_string()),
                        ("skewed_ge", skewed_ge.to_string()),
                    ]),
                )
            }
            "nonselective_filter_aggregate" => {
                let uniform_ge = random.inclusive(10_000, 250_000) as i64 - 1_000_000;
                (
                    format!(
                        "SELECT COUNT(*) AS matched, SUM(skewed_num) AS total FROM parity_data WHERE uniform_num >= {uniform_ge};"
                    ),
                    parameters(&[("uniform_ge", uniform_ge.to_string())]),
                )
            }
            "low_cardinality_group_by" => {
                const LOW_KEYS: [&str; 8] = [
                    "amber", "blue", "coral", "green", "indigo", "red", "silver", "violet",
                ];
                let low_key_ge = LOW_KEYS[random.below(LOW_KEYS.len() as u64) as usize];
                let id_ge = random.inclusive(0, row_count as u64 / 4);
                (
                    format!(
                        "SELECT low_key, flag, COUNT(*) AS row_count, SUM(uniform_num) AS total, AVG(score) AS mean_score FROM parity_data WHERE low_key >= '{low_key_ge}' AND id >= {id_ge} GROUP BY low_key, flag ORDER BY low_key, flag;"
                    ),
                    parameters(&[
                        ("low_key_ge", low_key_ge.to_owned()),
                        ("id_ge", id_ge.to_string()),
                    ]),
                )
            }
            "high_cardinality_group_by" => {
                let first_index = random.inclusive(0, row_count as u64 / 2) as usize;
                let high_key_ge = format!("entity_{first_index:08}");
                let limit = random.inclusive(64, 100);
                (
                    format!(
                        "SELECT high_key, COUNT(*) AS row_count, SUM(skewed_num) AS total FROM parity_data WHERE high_key >= '{high_key_ge}' GROUP BY high_key ORDER BY high_key LIMIT {limit};"
                    ),
                    parameters(&[("high_key_ge", high_key_ge), ("limit", limit.to_string())]),
                )
            }
            "numeric_order_by_limit" => {
                let score_eighths = random.inclusive(0, 80_000) as i64 - 80_000;
                let score_ge = format_eighths(score_eighths);
                let limit = random.inclusive(16, 32);
                (
                    format!(
                        "SELECT id, score, payload, uniform_num FROM parity_data WHERE score >= {score_ge} ORDER BY score DESC, id LIMIT {limit};"
                    ),
                    parameters(&[("score_ge", score_ge), ("limit", limit.to_string())]),
                )
            }
            "string_order_by_limit" => {
                const PAYLOAD_KEYS: [&str; 4] = ["a", "comma,inside", "medium", "quote's payload"];
                let payload_ge = PAYLOAD_KEYS[random.below(PAYLOAD_KEYS.len() as u64) as usize];
                let limit = random.inclusive(16, 32);
                (
                    format!(
                        "SELECT payload, id, flag FROM parity_data WHERE payload >= '{}' ORDER BY payload, id DESC LIMIT {limit};",
                        escape_sql_string(payload_ge)
                    ),
                    parameters(&[
                        ("payload_ge", payload_ge.to_owned()),
                        ("limit", limit.to_string()),
                    ]),
                )
            }
            _ => unreachable!("all workloads have a variant generator"),
        }
    }
}

pub fn repeated_query_sequence(query: &str, count: usize) -> QuerySequence {
    assert!(count > 0, "query sequences require at least one query");
    let mut sql = String::with_capacity(query.len().saturating_add(1).saturating_mul(count));
    for _ in 0..count {
        sql.push_str(query);
        sql.push('\n');
    }
    QuerySequence {
        seed: 0,
        sha256: sha256_hex(sql.as_bytes()),
        sql,
        variants: (0..count)
            .map(|ordinal| ResolvedVariant {
                ordinal,
                parameters: Vec::new(),
            })
            .collect(),
    }
}

fn derive_sequence_seed(runtime_seed: u64, row_count: usize, workload: &str) -> u64 {
    let mut value = runtime_seed ^ (row_count as u64).wrapping_mul(0xa076_1d64_78bd_642f);
    for byte in workload.bytes() {
        value ^= byte as u64;
        value = value.wrapping_mul(0x100_0000_01b3);
    }
    SplitMix64::new(value).next()
}

fn parameters(values: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    values.to_vec()
}

fn format_eighths(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    format!("{sign}{}.{:03}", magnitude / 8, magnitude % 8 * 125)
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

    fn below(&mut self, exclusive_upper: u64) -> u64 {
        self.next() % exclusive_upper
    }

    fn inclusive(&mut self, lower: u64, upper: u64) -> u64 {
        lower + self.below(upper - lower + 1)
    }
}

pub fn workloads(row_count: usize) -> Vec<Workload> {
    let selected_id = row_count / 2;
    vec![
        Workload {
            name: "full_scan_aggregate",
            family: Family::FullScanAggregate,
            sql: "SELECT COUNT(*) AS row_count, SUM(uniform_num) AS uniform_total, SUM(skewed_num) AS skewed_total, MIN(large_int) AS large_min, MAX(large_int) AS large_max, AVG(score) AS score_mean FROM parity_data;".to_owned(),
            columns: vec![
                ("row_count", ColumnType::Integer),
                ("uniform_total", ColumnType::Integer),
                ("skewed_total", ColumnType::Integer),
                ("large_min", ColumnType::Integer),
                ("large_max", ColumnType::Integer),
                ("score_mean", ColumnType::Float),
            ],
        },
        Workload {
            name: "selective_point_filter",
            family: Family::SelectiveFilter,
            sql: format!("SELECT id, payload, large_int, flag FROM parity_data WHERE id = {selected_id} ORDER BY id;"),
            columns: vec![
                ("id", ColumnType::Integer),
                ("payload", ColumnType::String),
                ("large_int", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
            ],
        },
        Workload {
            name: "compound_filter_aggregate",
            family: Family::CompoundFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE (flag = true AND uniform_num < -250000) OR (flag = false AND skewed_num >= 5);".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "nonselective_filter_aggregate",
            family: Family::NonselectiveFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(skewed_num) AS total FROM parity_data WHERE uniform_num >= -950000;".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "low_cardinality_group_by",
            family: Family::LowCardinalityGroupBy,
            sql: "SELECT low_key, flag, COUNT(*) AS row_count, SUM(uniform_num) AS total, AVG(score) AS mean_score FROM parity_data GROUP BY low_key, flag ORDER BY low_key, flag;".to_owned(),
            columns: vec![
                ("low_key", ColumnType::String),
                ("flag", ColumnType::Boolean),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
                ("mean_score", ColumnType::Float),
            ],
        },
        Workload {
            name: "high_cardinality_group_by",
            family: Family::HighCardinalityGroupBy,
            sql: "SELECT high_key, COUNT(*) AS row_count, SUM(skewed_num) AS total FROM parity_data GROUP BY high_key ORDER BY high_key LIMIT 100;".to_owned(),
            columns: vec![
                ("high_key", ColumnType::String),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "numeric_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT id, score, payload, uniform_num FROM parity_data ORDER BY score DESC, id LIMIT 25;".to_owned(),
            columns: vec![
                ("id", ColumnType::Integer),
                ("score", ColumnType::Float),
                ("payload", ColumnType::String),
                ("uniform_num", ColumnType::Integer),
            ],
        },
        Workload {
            name: "string_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT payload, id, flag FROM parity_data ORDER BY payload, id DESC LIMIT 25;".to_owned(),
            columns: vec![
                ("payload", ColumnType::String),
                ("id", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::dataset::Dataset;
    use rusthouse::Database;

    #[test]
    fn workload_diversity_invariants_are_explicit() {
        let workloads = workloads(10_000);
        let families = workloads
            .iter()
            .map(|workload| workload.family)
            .collect::<BTreeSet<_>>();

        assert_eq!(families.len(), 7);
        assert!(
            workloads
                .iter()
                .any(|workload| workload.sql.contains("AVG("))
        );
        assert!(
            workloads
                .iter()
                .any(|workload| workload.sql.contains(" AND "))
        );
        assert!(
            workloads
                .iter()
                .any(|workload| workload.sql.contains(" OR "))
        );
        assert!(
            workloads
                .iter()
                .any(|workload| workload.sql.contains("GROUP BY low_key"))
        );
        assert!(
            workloads
                .iter()
                .any(|workload| workload.sql.contains("GROUP BY high_key"))
        );
        assert!(
            workloads
                .iter()
                .any(|workload| workload.sql.contains("ORDER BY payload"))
        );
        assert!(workloads.iter().all(|workload| workload.sql.ends_with(';')));
    }

    #[test]
    fn selective_predicate_varies_with_row_count() {
        assert!(workloads(100)[1].sql.contains("id = 50"));
        assert!(workloads(1_000)[1].sql.contains("id = 500"));
    }

    #[test]
    fn query_sequences_are_seeded_reproducible_and_diverse() {
        for workload in workloads(10_000) {
            let first = workload.query_sequence(10_000, 77, 256);
            let repeat = workload.query_sequence(10_000, 77, 256);
            let other_seed = workload.query_sequence(10_000, 78, 256);

            assert_eq!(first, repeat);
            assert_ne!(first.sha256, other_seed.sha256, "{}", workload.name);
            assert_eq!(first.query_count(), 256);
            assert_eq!(first.sha256.len(), 64);
            first.validate().expect("sequence invariants");
            assert!(first.sql.lines().collect::<BTreeSet<_>>().len() > 8);
            assert!(first.sql.lines().all(|query| query.ends_with(';')));
        }
    }

    #[test]
    fn resolved_parameters_stay_within_documented_bounds() {
        let sequences = workloads(256)
            .into_iter()
            .map(|workload| (workload.name, workload.query_sequence(256, 9, 256)))
            .collect::<std::collections::BTreeMap<_, _>>();

        let nonselective = &sequences["nonselective_filter_aggregate"];
        for variant in &nonselective.variants {
            let threshold = variant.parameters[0].1.parse::<i64>().expect("integer");
            assert!((-990_000..=-750_000).contains(&threshold));
        }
        let numeric_order = &sequences["numeric_order_by_limit"];
        for variant in &numeric_order.variants {
            let limit = variant.parameters[1].1.parse::<u64>().expect("integer");
            assert!((16..=32).contains(&limit));
        }
    }

    #[test]
    fn repeated_transition_sequence_preserves_the_old_batch_bytes() {
        let sequence = repeated_query_sequence("SELECT 1;", 3);
        sequence.validate().expect("sequence invariants");
        assert_eq!(sequence.sql, "SELECT 1;\nSELECT 1;\nSELECT 1;\n");
        assert_eq!(sequence.query_count(), 3);
        assert!(
            sequence
                .variants
                .iter()
                .all(|variant| variant.parameters.is_empty())
        );
    }

    #[test]
    fn sequence_validation_fails_closed_on_metadata_changes() {
        let mut sequence = repeated_query_sequence("SELECT 1;", 3);
        sequence.sql.push_str("SELECT 2;\n");
        assert!(sequence.validate().is_err());

        let mut sequence = repeated_query_sequence("SELECT 1;", 3);
        sequence.variants[2].ordinal = 9;
        assert!(sequence.validate().is_err());
    }

    #[test]
    fn every_generated_query_shape_executes_at_the_smallest_scale() {
        let dataset = Dataset::generate(17, 256);
        for workload in workloads(256) {
            let sequence = workload.query_sequence(256, 91, 256);
            let sql = format!("{}{}", dataset.setup_sql(), sequence.sql);
            let results = Database::new().execute(&sql).expect(workload.name);
            assert_eq!(results.len(), 258, "{}", workload.name);
        }
    }
}
