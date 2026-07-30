use crate::dataset::Dataset;
use crate::normalize::ColumnType;

const COMPOUND_UNIFORM_NUMERATOR: usize = 3;
const COMPOUND_UNIFORM_DENOMINATOR: usize = 8;
const COMPOUND_SKEW_NUMERATOR: usize = 1;
const COMPOUND_SKEW_DENOMINATOR: usize = 4;
const NONSELECTIVE_NUMERATOR: usize = 39;
const NONSELECTIVE_DENOMINATOR: usize = 40;

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
pub struct Workload {
    pub name: &'static str,
    pub family: Family,
    pub sql: String,
    pub columns: Vec<(&'static str, ColumnType)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadSuite {
    pub workloads: Vec<Workload>,
    pub parameters: WorkloadParameters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkloadParameters {
    pub selected_id: i64,
    pub compound_flag: bool,
    pub compound_uniform_upper_bound: i64,
    pub compound_skew_lower_bound: i64,
    pub compound_target_rows: usize,
    pub compound_matched_rows: usize,
    pub nonselective_uniform_lower_bound: i64,
    pub nonselective_target_rows: usize,
    pub nonselective_matched_rows: usize,
}

pub fn workloads(seed: u64, dataset: &Dataset) -> Result<WorkloadSuite, String> {
    if dataset.rows.is_empty() {
        return Err("cannot resolve workloads for an empty dataset".to_owned());
    }

    let selected_index = (mix64(seed ^ 0x243f_6a88_85a3_08d3) as usize) % dataset.rows.len();
    let selected_id = dataset.rows[selected_index].id;
    let compound_flag = mix64(seed ^ 0x1319_8a2e_0370_7344) & 1 == 0;
    let compound_uniform_values = dataset
        .rows
        .iter()
        .filter(|row| row.flag == compound_flag)
        .map(|row| row.uniform_num)
        .collect::<Vec<_>>();
    let compound_skew_values = dataset
        .rows
        .iter()
        .filter(|row| row.flag != compound_flag)
        .map(|row| row.skewed_num)
        .collect::<Vec<_>>();
    if compound_uniform_values.is_empty() || compound_skew_values.is_empty() {
        return Err("compound workload requires rows for both Boolean values".to_owned());
    }

    let compound_uniform_target = fraction_count(
        compound_uniform_values.len(),
        COMPOUND_UNIFORM_NUMERATOR,
        COMPOUND_UNIFORM_DENOMINATOR,
    );
    let compound_skew_target = fraction_count(
        compound_skew_values.len(),
        COMPOUND_SKEW_NUMERATOR,
        COMPOUND_SKEW_DENOMINATOR,
    );
    let compound_uniform_upper_bound = closest_threshold(
        &compound_uniform_values,
        compound_uniform_target,
        Comparison::Less,
        seed ^ 0xa409_3822_299f_31d0,
    );
    let compound_skew_lower_bound = closest_threshold(
        &compound_skew_values,
        compound_skew_target,
        Comparison::GreaterOrEqual,
        seed ^ 0x082e_fa98_ec4e_6c89,
    );

    let uniform_values = dataset
        .rows
        .iter()
        .map(|row| row.uniform_num)
        .collect::<Vec<_>>();
    let nonselective_target_rows = fraction_count(
        dataset.rows.len(),
        NONSELECTIVE_NUMERATOR,
        NONSELECTIVE_DENOMINATOR,
    );
    let nonselective_uniform_lower_bound = closest_threshold(
        &uniform_values,
        nonselective_target_rows,
        Comparison::GreaterOrEqual,
        seed ^ 0x4528_21e6_38d0_1377,
    );

    let compound_matched_rows = dataset
        .rows
        .iter()
        .filter(|row| {
            (row.flag == compound_flag && row.uniform_num < compound_uniform_upper_bound)
                || (row.flag != compound_flag && row.skewed_num >= compound_skew_lower_bound)
        })
        .count();
    let nonselective_matched_rows = dataset
        .rows
        .iter()
        .filter(|row| row.uniform_num >= nonselective_uniform_lower_bound)
        .count();
    let parameters = WorkloadParameters {
        selected_id,
        compound_flag,
        compound_uniform_upper_bound,
        compound_skew_lower_bound,
        compound_target_rows: compound_uniform_target + compound_skew_target,
        compound_matched_rows,
        nonselective_uniform_lower_bound,
        nonselective_target_rows,
        nonselective_matched_rows,
    };
    let opposite_flag = !compound_flag;

    let workloads = vec![
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
            sql: format!(
                "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE (flag = {compound_flag} AND uniform_num < {compound_uniform_upper_bound}) OR (flag = {opposite_flag} AND skewed_num >= {compound_skew_lower_bound});"
            ),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "nonselective_filter_aggregate",
            family: Family::NonselectiveFilter,
            sql: format!(
                "SELECT COUNT(*) AS matched, SUM(skewed_num) AS total FROM parity_data WHERE uniform_num >= {nonselective_uniform_lower_bound};"
            ),
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
    ];

    Ok(WorkloadSuite {
        workloads,
        parameters,
    })
}

#[derive(Debug, Clone, Copy)]
enum Comparison {
    Less,
    GreaterOrEqual,
}

fn closest_threshold(values: &[i64], target_rows: usize, comparison: Comparison, seed: u64) -> i64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let replace_on_tie = mix64(seed) & 1 == 0;
    let mut best = sorted[0];
    let mut best_distance = usize::MAX;
    let mut index = 0;

    while index < sorted.len() {
        let candidate = sorted[index];
        let matched_rows = match comparison {
            Comparison::Less => index,
            Comparison::GreaterOrEqual => sorted.len() - index,
        };
        let distance = matched_rows.abs_diff(target_rows);
        if distance < best_distance || (distance == best_distance && replace_on_tie) {
            best = candidate;
            best_distance = distance;
        }
        while index < sorted.len() && sorted[index] == candidate {
            index += 1;
        }
    }
    best
}

fn fraction_count(total: usize, numerator: usize, denominator: usize) -> usize {
    ((total as u128 * numerator as u128 + (denominator / 2) as u128) / denominator as u128) as usize
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn workload_diversity_invariants_are_explicit() {
        let dataset = Dataset::generate(7, 10_000);
        let suite = workloads(11, &dataset).expect("workloads");
        let workloads = suite.workloads;
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
        assert_eq!(
            dataset
                .rows
                .iter()
                .filter(|row| row.id == suite.parameters.selected_id)
                .count(),
            1
        );
        assert!(
            suite
                .parameters
                .compound_matched_rows
                .abs_diff(suite.parameters.compound_target_rows)
                <= dataset.rows.len() / 20
        );
        assert!(
            suite
                .parameters
                .nonselective_matched_rows
                .abs_diff(suite.parameters.nonselective_target_rows)
                <= 1
        );
    }

    #[test]
    fn workload_resolution_is_reproducible() {
        let dataset = Dataset::generate(7, 1_000);
        assert_eq!(
            workloads(99, &dataset).expect("workloads"),
            workloads(99, &dataset).expect("workloads")
        );
    }

    #[test]
    fn runtime_seed_changes_literals_without_changing_family_shapes() {
        let left_dataset = Dataset::generate(41, 2_000);
        let right_dataset = Dataset::generate(42, 2_000);
        let left = workloads(41, &left_dataset).expect("left workloads");
        let right = workloads(42, &right_dataset).expect("right workloads");

        assert_ne!(left.parameters, right.parameters);
        assert_ne!(
            left.workloads
                .iter()
                .map(|workload| &workload.sql)
                .collect::<Vec<_>>(),
            right
                .workloads
                .iter()
                .map(|workload| &workload.sql)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            left.workloads
                .iter()
                .map(|workload| (workload.name, workload.family, &workload.columns))
                .collect::<Vec<_>>(),
            right
                .workloads
                .iter()
                .map(|workload| (workload.name, workload.family, &workload.columns))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn selectivity_and_cardinality_targets_hold_across_seeds_and_scales() {
        for seed in [0, 1, 99, 20_260_729, u64::MAX] {
            for row_count in [256, 2_048, 10_000] {
                let dataset = Dataset::generate(
                    seed ^ (row_count as u64).wrapping_mul(0xd6e8_feb8_6659_fd93),
                    row_count,
                );
                let suite = workloads(
                    seed ^ (row_count as u64).wrapping_mul(0x94d0_49bb_1331_11eb),
                    &dataset,
                )
                .expect("workloads");
                let parameters = suite.parameters;

                assert_eq!(
                    dataset
                        .rows
                        .iter()
                        .filter(|row| row.id == parameters.selected_id)
                        .count(),
                    1
                );
                assert!(
                    parameters
                        .compound_matched_rows
                        .abs_diff(parameters.compound_target_rows)
                        <= row_count / 20
                );
                assert!(
                    parameters
                        .nonselective_matched_rows
                        .abs_diff(parameters.nonselective_target_rows)
                        <= 1
                );
                assert_eq!(suite.workloads.len(), 8);
                assert_eq!(
                    suite
                        .workloads
                        .iter()
                        .map(|workload| workload.family)
                        .collect::<BTreeSet<_>>()
                        .len(),
                    7
                );
            }
        }
    }
}
