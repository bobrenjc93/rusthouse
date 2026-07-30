use crate::dataset::SchemaProfile;
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

#[derive(Debug, Clone)]
pub struct Workload {
    pub name: &'static str,
    pub family: Family,
    pub sql: String,
    pub columns: Vec<(&'static str, ColumnType)>,
}

pub fn workloads(profile: SchemaProfile, row_count: usize) -> Vec<Workload> {
    match profile {
        SchemaProfile::NumericHeavy => numeric_workloads(row_count),
        SchemaProfile::StringHeavy => string_workloads(row_count),
        SchemaProfile::WideMixed => wide_workloads(row_count),
    }
}

fn numeric_workloads(row_count: usize) -> Vec<Workload> {
    let selected_id = row_count / 2;
    vec![
        Workload {
            name: "full_scan_aggregate",
            family: Family::FullScanAggregate,
            sql: "SELECT COUNT(*) AS row_count, SUM(uniform_num) AS uniform_total, SUM(skewed_num) AS skewed_total, SUM(aux_int_a) AS aux_total, MIN(large_int) AS large_min, MAX(large_int) AS large_max, AVG(score) AS score_mean, AVG(aux_score) AS aux_mean FROM parity_data;".to_owned(),
            columns: vec![
                ("row_count", ColumnType::Integer),
                ("uniform_total", ColumnType::Integer),
                ("skewed_total", ColumnType::Integer),
                ("aux_total", ColumnType::Integer),
                ("large_min", ColumnType::Integer),
                ("large_max", ColumnType::Integer),
                ("score_mean", ColumnType::Float),
                ("aux_mean", ColumnType::Float),
            ],
        },
        Workload {
            name: "selective_point_filter",
            family: Family::SelectiveFilter,
            sql: format!("SELECT id, uniform_num, skewed_num, large_int, aux_int_a, aux_int_b, score, aux_score, bucket, flag, label FROM parity_data WHERE id = {selected_id} ORDER BY id;"),
            columns: vec![
                ("id", ColumnType::Integer),
                ("uniform_num", ColumnType::Integer),
                ("skewed_num", ColumnType::Integer),
                ("large_int", ColumnType::Integer),
                ("aux_int_a", ColumnType::Integer),
                ("aux_int_b", ColumnType::Integer),
                ("score", ColumnType::Float),
                ("aux_score", ColumnType::Float),
                ("bucket", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
                ("label", ColumnType::String),
            ],
        },
        Workload {
            name: "compound_filter_aggregate",
            family: Family::CompoundFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(aux_int_a) AS total FROM parity_data WHERE (flag = true AND uniform_num < -250000) OR (flag = false AND skewed_num >= 5);".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "nonselective_filter_aggregate",
            family: Family::NonselectiveFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(aux_int_b) AS total FROM parity_data WHERE aux_int_a >= -475000;".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "low_cardinality_group_by",
            family: Family::LowCardinalityGroupBy,
            sql: "SELECT bucket, flag, COUNT(*) AS row_count, SUM(uniform_num) AS total, AVG(score) AS mean_score FROM parity_data GROUP BY bucket, flag ORDER BY bucket, flag;".to_owned(),
            columns: vec![
                ("bucket", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
                ("mean_score", ColumnType::Float),
            ],
        },
        Workload {
            name: "high_cardinality_group_by",
            family: Family::HighCardinalityGroupBy,
            sql: "SELECT id, COUNT(*) AS row_count, SUM(skewed_num) AS total FROM parity_data GROUP BY id ORDER BY id LIMIT 100;".to_owned(),
            columns: vec![
                ("id", ColumnType::Integer),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "primary_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT id, score, uniform_num, skewed_num, large_int, aux_int_a, aux_int_b, flag, label FROM parity_data ORDER BY score DESC, id LIMIT 25;".to_owned(),
            columns: vec![
                ("id", ColumnType::Integer),
                ("score", ColumnType::Float),
                ("uniform_num", ColumnType::Integer),
                ("skewed_num", ColumnType::Integer),
                ("large_int", ColumnType::Integer),
                ("aux_int_a", ColumnType::Integer),
                ("aux_int_b", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
                ("label", ColumnType::String),
            ],
        },
        Workload {
            name: "secondary_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT label, bucket, aux_score, id FROM parity_data ORDER BY label, id DESC LIMIT 25;".to_owned(),
            columns: vec![
                ("label", ColumnType::String),
                ("bucket", ColumnType::Integer),
                ("aux_score", ColumnType::Float),
                ("id", ColumnType::Integer),
            ],
        },
    ]
}

fn string_workloads(row_count: usize) -> Vec<Workload> {
    let selected_id = row_count / 2;
    vec![
        Workload {
            name: "full_scan_aggregate",
            family: Family::FullScanAggregate,
            sql: "SELECT COUNT(*) AS row_count, MIN(low_key) AS low_min, MAX(high_key) AS high_max, MIN(payload) AS payload_min, MAX(description) AS description_max, SUM(uniform_num) AS numeric_total, AVG(score) AS score_mean FROM parity_data;".to_owned(),
            columns: vec![
                ("row_count", ColumnType::Integer),
                ("low_min", ColumnType::String),
                ("high_max", ColumnType::String),
                ("payload_min", ColumnType::String),
                ("description_max", ColumnType::String),
                ("numeric_total", ColumnType::Integer),
                ("score_mean", ColumnType::Float),
            ],
        },
        Workload {
            name: "selective_point_filter",
            family: Family::SelectiveFilter,
            sql: format!("SELECT id, low_key, high_key, payload, region, code, description, label, flag, uniform_num, score FROM parity_data WHERE id = {selected_id} ORDER BY id;"),
            columns: vec![
                ("id", ColumnType::Integer),
                ("low_key", ColumnType::String),
                ("high_key", ColumnType::String),
                ("payload", ColumnType::String),
                ("region", ColumnType::String),
                ("code", ColumnType::String),
                ("description", ColumnType::String),
                ("label", ColumnType::String),
                ("flag", ColumnType::Boolean),
                ("uniform_num", ColumnType::Integer),
                ("score", ColumnType::Float),
            ],
        },
        Workload {
            name: "compound_filter_aggregate",
            family: Family::CompoundFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE (flag = true AND low_key = 'amber') OR (flag = false AND region = 'north');".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "nonselective_filter_aggregate",
            family: Family::NonselectiveFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE high_key >= 'entity_000000_00000000';".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "low_cardinality_group_by",
            family: Family::LowCardinalityGroupBy,
            sql: "SELECT low_key, region, flag, COUNT(*) AS row_count, SUM(uniform_num) AS total, AVG(score) AS mean_score FROM parity_data GROUP BY low_key, region, flag ORDER BY low_key, region, flag;".to_owned(),
            columns: vec![
                ("low_key", ColumnType::String),
                ("region", ColumnType::String),
                ("flag", ColumnType::Boolean),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
                ("mean_score", ColumnType::Float),
            ],
        },
        Workload {
            name: "high_cardinality_group_by",
            family: Family::HighCardinalityGroupBy,
            sql: "SELECT high_key, COUNT(*) AS row_count, MIN(payload) AS first_payload FROM parity_data GROUP BY high_key ORDER BY high_key LIMIT 100;".to_owned(),
            columns: vec![
                ("high_key", ColumnType::String),
                ("row_count", ColumnType::Integer),
                ("first_payload", ColumnType::String),
            ],
        },
        Workload {
            name: "primary_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT payload, description, low_key, region, code, label, id, flag FROM parity_data ORDER BY payload, id DESC LIMIT 25;".to_owned(),
            columns: vec![
                ("payload", ColumnType::String),
                ("description", ColumnType::String),
                ("low_key", ColumnType::String),
                ("region", ColumnType::String),
                ("code", ColumnType::String),
                ("label", ColumnType::String),
                ("id", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
            ],
        },
        Workload {
            name: "secondary_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT high_key, code, region, score, id FROM parity_data ORDER BY high_key DESC, id LIMIT 25;".to_owned(),
            columns: vec![
                ("high_key", ColumnType::String),
                ("code", ColumnType::String),
                ("region", ColumnType::String),
                ("score", ColumnType::Float),
                ("id", ColumnType::Integer),
            ],
        },
    ]
}

fn wide_workloads(row_count: usize) -> Vec<Workload> {
    let selected_id = row_count / 2;
    vec![
        Workload {
            name: "full_scan_aggregate",
            family: Family::FullScanAggregate,
            sql: "SELECT COUNT(*) AS row_count, SUM(uniform_num) AS uniform_total, SUM(skewed_num) AS skewed_total, SUM(aux_int_a) AS aux_total, MIN(large_int) AS large_min, MAX(large_int) AS large_max, MIN(payload) AS payload_min, MAX(description) AS description_max, AVG(score) AS score_mean, AVG(aux_score) AS aux_mean FROM parity_data;".to_owned(),
            columns: vec![
                ("row_count", ColumnType::Integer),
                ("uniform_total", ColumnType::Integer),
                ("skewed_total", ColumnType::Integer),
                ("aux_total", ColumnType::Integer),
                ("large_min", ColumnType::Integer),
                ("large_max", ColumnType::Integer),
                ("payload_min", ColumnType::String),
                ("description_max", ColumnType::String),
                ("score_mean", ColumnType::Float),
                ("aux_mean", ColumnType::Float),
            ],
        },
        Workload {
            name: "selective_point_filter",
            family: Family::SelectiveFilter,
            sql: format!("SELECT id, uniform_num, skewed_num, large_int, aux_int_a, aux_int_b, score, aux_score, bucket, low_key, high_key, payload, region, code, description, label, flag, secondary_flag FROM parity_data WHERE id = {selected_id} ORDER BY id;"),
            columns: vec![
                ("id", ColumnType::Integer),
                ("uniform_num", ColumnType::Integer),
                ("skewed_num", ColumnType::Integer),
                ("large_int", ColumnType::Integer),
                ("aux_int_a", ColumnType::Integer),
                ("aux_int_b", ColumnType::Integer),
                ("score", ColumnType::Float),
                ("aux_score", ColumnType::Float),
                ("bucket", ColumnType::Integer),
                ("low_key", ColumnType::String),
                ("high_key", ColumnType::String),
                ("payload", ColumnType::String),
                ("region", ColumnType::String),
                ("code", ColumnType::String),
                ("description", ColumnType::String),
                ("label", ColumnType::String),
                ("flag", ColumnType::Boolean),
                ("secondary_flag", ColumnType::Boolean),
            ],
        },
        Workload {
            name: "compound_filter_aggregate",
            family: Family::CompoundFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(aux_int_a) AS total FROM parity_data WHERE (flag = true AND uniform_num < -250000 AND region = 'west') OR (secondary_flag = false AND skewed_num >= 5);".to_owned(),
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
            sql: "SELECT low_key, bucket, flag, secondary_flag, COUNT(*) AS row_count, SUM(uniform_num) AS total, AVG(score) AS mean_score FROM parity_data GROUP BY low_key, bucket, flag, secondary_flag ORDER BY low_key, bucket, flag, secondary_flag;".to_owned(),
            columns: vec![
                ("low_key", ColumnType::String),
                ("bucket", ColumnType::Integer),
                ("flag", ColumnType::Boolean),
                ("secondary_flag", ColumnType::Boolean),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
                ("mean_score", ColumnType::Float),
            ],
        },
        Workload {
            name: "high_cardinality_group_by",
            family: Family::HighCardinalityGroupBy,
            sql: "SELECT high_key, COUNT(*) AS row_count, SUM(skewed_num) AS total, MIN(description) AS first_description FROM parity_data GROUP BY high_key ORDER BY high_key LIMIT 100;".to_owned(),
            columns: vec![
                ("high_key", ColumnType::String),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
                ("first_description", ColumnType::String),
            ],
        },
        Workload {
            name: "primary_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT id, score, aux_score, uniform_num, skewed_num, large_int, payload, region, flag, secondary_flag FROM parity_data ORDER BY score DESC, id LIMIT 25;".to_owned(),
            columns: vec![
                ("id", ColumnType::Integer),
                ("score", ColumnType::Float),
                ("aux_score", ColumnType::Float),
                ("uniform_num", ColumnType::Integer),
                ("skewed_num", ColumnType::Integer),
                ("large_int", ColumnType::Integer),
                ("payload", ColumnType::String),
                ("region", ColumnType::String),
                ("flag", ColumnType::Boolean),
                ("secondary_flag", ColumnType::Boolean),
            ],
        },
        Workload {
            name: "secondary_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT payload, description, high_key, code, label, aux_int_a, aux_int_b, aux_score, flag, id FROM parity_data ORDER BY payload, id DESC LIMIT 25;".to_owned(),
            columns: vec![
                ("payload", ColumnType::String),
                ("description", ColumnType::String),
                ("high_key", ColumnType::String),
                ("code", ColumnType::String),
                ("label", ColumnType::String),
                ("aux_int_a", ColumnType::Integer),
                ("aux_int_b", ColumnType::Integer),
                ("aux_score", ColumnType::Float),
                ("flag", ColumnType::Boolean),
                ("id", ColumnType::Integer),
            ],
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
    fn every_profile_has_complete_family_coverage() {
        for profile in SchemaProfile::ALL {
            let workloads = workloads(profile, 10_000);
            let families = workloads
                .iter()
                .map(|workload| workload.family)
                .collect::<BTreeSet<_>>();

            assert_eq!(workloads.len(), 8, "profile {}", profile.name());
            assert_eq!(families.len(), 7, "profile {}", profile.name());
            assert!(workloads.iter().all(|workload| workload.sql.ends_with(';')));
            assert!(
                workloads
                    .iter()
                    .all(|workload| !workload.columns.is_empty())
            );
        }
    }

    #[test]
    fn profile_queries_exercise_distinct_shapes_and_projection_widths() {
        let numeric = workloads(SchemaProfile::NumericHeavy, 10_000);
        let strings = workloads(SchemaProfile::StringHeavy, 10_000);
        let wide = workloads(SchemaProfile::WideMixed, 10_000);

        assert!(
            numeric
                .iter()
                .any(|workload| workload.sql.contains("SUM(aux_int_a)"))
        );
        assert!(
            strings
                .iter()
                .any(|workload| workload.sql.contains("MIN(payload)"))
        );
        assert!(
            wide.iter()
                .any(|workload| workload.sql.contains("secondary_flag"))
        );
        assert_eq!(numeric[1].columns.len(), 11);
        assert_eq!(strings[1].columns.len(), 11);
        assert_eq!(wide[1].columns.len(), 18);
    }

    #[test]
    fn selective_predicate_varies_with_row_count_for_every_profile() {
        for profile in SchemaProfile::ALL {
            assert!(workloads(profile, 100)[1].sql.contains("id = 50"));
            assert!(workloads(profile, 1_000)[1].sql.contains("id = 500"));
        }
    }

    #[test]
    fn every_profile_query_executes_with_its_declared_output_columns() {
        for profile in SchemaProfile::ALL {
            let dataset = Dataset::generate(profile, 42 ^ profile.seed_salt(), 64);
            let setup_sql = dataset.setup_sql();
            for workload in workloads(profile, dataset.rows.len()) {
                let mut database = Database::new();
                let results = database
                    .execute(&format!("{setup_sql}{}", workload.sql))
                    .unwrap_or_else(|error| {
                        panic!(
                            "{} / {} failed to execute: {error}",
                            profile.name(),
                            workload.name
                        )
                    });
                let Some(StatementResult::Query(result)) = results.last() else {
                    panic!("{} / {} returned no query", profile.name(), workload.name);
                };
                let actual = result
                    .columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>();
                let expected = workload
                    .columns
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected, "{} / {}", profile.name(), workload.name);
            }
        }
    }
}
