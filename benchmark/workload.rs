use crate::normalize::ColumnType;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParameterValue {
    Integer(i64),
    Boolean(bool),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantParameter {
    pub name: &'static str,
    pub value: ParameterValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryVariant {
    pub sql: String,
    pub parameters: Vec<VariantParameter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantSet {
    pub seed: u64,
    pub queries: Vec<QueryVariant>,
}

#[derive(Debug, Clone, Copy)]
enum VariantKind {
    FullScanAggregate,
    SelectivePointFilter,
    CompoundFilterAggregate,
    NonselectiveFilterAggregate,
    LowCardinalityGroupBy,
    HighCardinalityGroupBy,
    NumericOrderByLimit,
    StringOrderByLimit,
}

impl VariantKind {
    fn salt(self) -> u64 {
        match self {
            Self::FullScanAggregate => 0x42c5_ef71_5e05_661d,
            Self::SelectivePointFilter => 0xe48b_3a91_203d_729b,
            Self::CompoundFilterAggregate => 0x18af_28de_0c3e_6d45,
            Self::NonselectiveFilterAggregate => 0xb312_33f3_a9d7_8f6b,
            Self::LowCardinalityGroupBy => 0x7560_c8fe_4e6c_b77d,
            Self::HighCardinalityGroupBy => 0x8f9d_b85a_1f15_3c29,
            Self::NumericOrderByLimit => 0x2947_130d_1080_018b,
            Self::StringOrderByLimit => 0xd71f_8a2b_6ce4_95e3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workload {
    pub name: &'static str,
    pub family: Family,
    pub columns: Vec<(&'static str, ColumnType)>,
    row_count: usize,
    variant_kind: VariantKind,
}

impl Workload {
    pub fn variants(&self, seed: u64, count: usize) -> Result<VariantSet, String> {
        if count == 0 {
            return Err("query variant count must be positive".to_owned());
        }
        let row_count = i64::try_from(self.row_count)
            .map_err(|_| "row count exceeds the supported Int64 range".to_owned())?;
        if row_count == 0 {
            return Err("query variants require at least one row".to_owned());
        }

        let variant_seed = mix64(seed ^ self.variant_kind.salt());
        let mut queries = Vec::with_capacity(count);
        for index in 0..count {
            let index = u64::try_from(index)
                .map_err(|_| "query variant index exceeds the supported range".to_owned())?;
            queries.push(self.variant(variant_seed, index, row_count));
        }
        Ok(VariantSet {
            seed: variant_seed,
            queries,
        })
    }

    fn variant(&self, seed: u64, index: u64, row_count: i64) -> QueryVariant {
        match self.variant_kind {
            VariantKind::FullScanAggregate => {
                let minimum_id = -1 - permuted(seed, index, 1_000_003, 0);
                QueryVariant {
                    sql: format!(
                        "SELECT COUNT(*) AS row_count, SUM(uniform_num) AS uniform_total, SUM(skewed_num) AS skewed_total, MIN(large_int) AS large_min, MAX(large_int) AS large_max, AVG(score) AS score_mean FROM parity_data WHERE id >= {minimum_id};"
                    ),
                    parameters: vec![integer_parameter("minimum_id", minimum_id)],
                }
            }
            VariantKind::SelectivePointFilter => {
                let selected_id = permuted(seed, index, row_count, 0);
                let minimum_id = -1 - permuted(seed, index, 1_000_003, 1);
                QueryVariant {
                    sql: format!(
                        "SELECT id, payload, large_int, flag FROM parity_data WHERE id = {selected_id} AND id >= {minimum_id} ORDER BY id;"
                    ),
                    parameters: vec![
                        integer_parameter("selected_id", selected_id),
                        integer_parameter("minimum_id", minimum_id),
                    ],
                }
            }
            VariantKind::CompoundFilterAggregate => {
                let uniform_threshold = -375_000 + permuted(seed, index, 250_001, 0);
                let skewed_threshold = -4 + permuted(seed, index, 13, 1);
                let first_flag = permuted(seed, index, 2, 2) == 1;
                QueryVariant {
                    sql: format!(
                        "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE (flag = {first_flag} AND uniform_num < {uniform_threshold}) OR (flag = {} AND skewed_num >= {skewed_threshold});",
                        !first_flag
                    ),
                    parameters: vec![
                        boolean_parameter("first_flag", first_flag),
                        integer_parameter("uniform_threshold", uniform_threshold),
                        integer_parameter("skewed_threshold", skewed_threshold),
                    ],
                }
            }
            VariantKind::NonselectiveFilterAggregate => {
                let uniform_threshold = nonselective_threshold(seed, index);
                QueryVariant {
                    sql: format!(
                        "SELECT COUNT(*) AS matched, SUM(skewed_num) AS total FROM parity_data WHERE uniform_num >= {uniform_threshold};"
                    ),
                    parameters: vec![integer_parameter("uniform_threshold", uniform_threshold)],
                }
            }
            VariantKind::LowCardinalityGroupBy => {
                let uniform_threshold = nonselective_threshold(seed, index);
                QueryVariant {
                    sql: format!(
                        "SELECT low_key, flag, COUNT(*) AS row_count, SUM(uniform_num) AS total, AVG(score) AS mean_score FROM parity_data WHERE uniform_num >= {uniform_threshold} GROUP BY low_key, flag ORDER BY low_key, flag;"
                    ),
                    parameters: vec![integer_parameter("uniform_threshold", uniform_threshold)],
                }
            }
            VariantKind::HighCardinalityGroupBy => {
                let uniform_threshold = nonselective_threshold(seed, index);
                QueryVariant {
                    sql: format!(
                        "SELECT high_key, COUNT(*) AS row_count, SUM(skewed_num) AS total FROM parity_data WHERE uniform_num >= {uniform_threshold} GROUP BY high_key ORDER BY high_key LIMIT 100;"
                    ),
                    parameters: vec![integer_parameter("uniform_threshold", uniform_threshold)],
                }
            }
            VariantKind::NumericOrderByLimit => {
                let uniform_threshold = nonselective_threshold(seed, index);
                QueryVariant {
                    sql: format!(
                        "SELECT id, score, payload, uniform_num FROM parity_data WHERE uniform_num >= {uniform_threshold} ORDER BY score DESC, id LIMIT 25;"
                    ),
                    parameters: vec![integer_parameter("uniform_threshold", uniform_threshold)],
                }
            }
            VariantKind::StringOrderByLimit => {
                let token = format!("__variant_{:016x}", mix64(seed ^ index));
                QueryVariant {
                    sql: format!(
                        "SELECT payload, id, flag FROM parity_data WHERE payload != '{token}' ORDER BY payload, id DESC LIMIT 25;"
                    ),
                    parameters: vec![VariantParameter {
                        name: "excluded_payload",
                        value: ParameterValue::String(token),
                    }],
                }
            }
        }
    }
}

fn integer_parameter(name: &'static str, value: i64) -> VariantParameter {
    VariantParameter {
        name,
        value: ParameterValue::Integer(value),
    }
}

fn boolean_parameter(name: &'static str, value: bool) -> VariantParameter {
    VariantParameter {
        name,
        value: ParameterValue::Boolean(value),
    }
}

fn nonselective_threshold(seed: u64, index: u64) -> i64 {
    -1_000_000 + permuted(seed, index, 150_001, 0)
}

fn permuted(seed: u64, index: u64, modulus: i64, stream: u64) -> i64 {
    let modulus = modulus as u64;
    let stream_seed = mix64(seed ^ stream.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let offset = mix64(stream_seed) % modulus;
    let mut step = mix64(stream_seed ^ 0xa076_1d64_78bd_642f) % modulus;
    if step == 0 {
        step = 1;
    }
    while gcd(step, modulus) != 1 {
        step = (step + 1) % modulus;
        if step == 0 {
            step = 1;
        }
    }
    ((offset + (index % modulus) * step) % modulus) as i64
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn mix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub fn workloads(row_count: usize) -> Vec<Workload> {
    vec![
        Workload {
            name: "full_scan_aggregate",
            family: Family::FullScanAggregate,
            columns: vec![
                ("row_count", ColumnType::Integer),
                ("uniform_total", ColumnType::Integer),
                ("skewed_total", ColumnType::Integer),
                ("large_min", ColumnType::Integer),
                ("large_max", ColumnType::Integer),
                ("score_mean", ColumnType::Float),
            ],
            row_count,
            variant_kind: VariantKind::FullScanAggregate,
        },
        Workload {
            name: "selective_point_filter",
            family: Family::SelectiveFilter,
            columns: vec![
                ("id", ColumnType::Integer),
                ("payload", ColumnType::String),
                ("large_int", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
            ],
            row_count,
            variant_kind: VariantKind::SelectivePointFilter,
        },
        Workload {
            name: "compound_filter_aggregate",
            family: Family::CompoundFilter,
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
            row_count,
            variant_kind: VariantKind::CompoundFilterAggregate,
        },
        Workload {
            name: "nonselective_filter_aggregate",
            family: Family::NonselectiveFilter,
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
            row_count,
            variant_kind: VariantKind::NonselectiveFilterAggregate,
        },
        Workload {
            name: "low_cardinality_group_by",
            family: Family::LowCardinalityGroupBy,
            columns: vec![
                ("low_key", ColumnType::String),
                ("flag", ColumnType::Boolean),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
                ("mean_score", ColumnType::Float),
            ],
            row_count,
            variant_kind: VariantKind::LowCardinalityGroupBy,
        },
        Workload {
            name: "high_cardinality_group_by",
            family: Family::HighCardinalityGroupBy,
            columns: vec![
                ("high_key", ColumnType::String),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
            row_count,
            variant_kind: VariantKind::HighCardinalityGroupBy,
        },
        Workload {
            name: "numeric_order_by_limit",
            family: Family::OrderByLimit,
            columns: vec![
                ("id", ColumnType::Integer),
                ("score", ColumnType::Float),
                ("payload", ColumnType::String),
                ("uniform_num", ColumnType::Integer),
            ],
            row_count,
            variant_kind: VariantKind::NumericOrderByLimit,
        },
        Workload {
            name: "string_order_by_limit",
            family: Family::OrderByLimit,
            columns: vec![
                ("payload", ColumnType::String),
                ("id", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
            ],
            row_count,
            variant_kind: VariantKind::StringOrderByLimit,
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rusthouse::{Database, StatementResult};

    use super::*;
    use crate::dataset::Dataset;

    #[test]
    fn workload_diversity_invariants_are_explicit() {
        let workloads = workloads(10_000);
        let families = workloads
            .iter()
            .map(|workload| workload.family)
            .collect::<BTreeSet<_>>();
        let sql = workloads
            .iter()
            .map(|workload| {
                workload.variants(7, 1).expect("variant").queries[0]
                    .sql
                    .clone()
            })
            .collect::<Vec<_>>();

        assert_eq!(families.len(), 7);
        assert!(sql.iter().any(|query| query.contains("AVG(")));
        assert!(sql.iter().any(|query| query.contains(" AND ")));
        assert!(sql.iter().any(|query| query.contains(" OR ")));
        assert!(sql.iter().any(|query| query.contains("GROUP BY low_key")));
        assert!(sql.iter().any(|query| query.contains("GROUP BY high_key")));
        assert!(sql.iter().any(|query| query.contains("ORDER BY payload")));
        assert!(sql.iter().all(|query| query.ends_with(';')));
    }

    #[test]
    fn variants_are_seeded_reproducible_and_not_exact_repetitions() {
        for workload in workloads(2_048) {
            let first = workload.variants(42, 256).expect("variants");
            let repeated = workload.variants(42, 256).expect("variants");
            let other_seed = workload.variants(43, 256).expect("variants");

            assert_eq!(first, repeated, "{} reproducibility", workload.name);
            assert_ne!(first, other_seed, "{} seed variation", workload.name);
            assert_eq!(first.queries.len(), 256);
            assert_eq!(
                first
                    .queries
                    .iter()
                    .map(|variant| variant.sql.as_str())
                    .collect::<BTreeSet<_>>()
                    .len(),
                256,
                "{} unique SQL variants",
                workload.name
            );
            assert!(
                first
                    .queries
                    .iter()
                    .all(|variant| !variant.parameters.is_empty())
            );
        }
    }

    #[test]
    fn selective_parameters_stay_inside_the_dataset() {
        let workload = workloads(1_000).remove(1);
        let variants = workload.variants(99, 256).expect("variants");
        for variant in variants.queries {
            let ParameterValue::Integer(selected_id) = variant.parameters[0].value else {
                panic!("selected ID must be an integer");
            };
            assert!((0..1_000).contains(&selected_id));
        }
    }

    #[test]
    fn every_generated_variant_executes_with_the_declared_schema() {
        let row_count = 256;
        let dataset = Dataset::generate(17, row_count);
        for workload in workloads(row_count) {
            let variants = workload.variants(17, 256).expect("variants");
            let mut sql = dataset.setup_sql();
            for variant in variants.queries {
                sql.push_str(&variant.sql);
                sql.push('\n');
            }

            let results = Database::new()
                .execute(&sql)
                .expect("generated SQL executes");
            let queries = results
                .iter()
                .filter_map(|result| match result {
                    StatementResult::Query(query) => Some(query),
                    StatementResult::Command { .. } => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(queries.len(), 256, "{} result count", workload.name);
            let expected_columns = workload
                .columns
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>();
            for query in queries {
                assert_eq!(
                    query
                        .columns
                        .iter()
                        .map(|column| column.name.as_str())
                        .collect::<Vec<_>>(),
                    expected_columns,
                    "{} schema",
                    workload.name
                );
            }
        }
    }
}
