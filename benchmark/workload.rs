use crate::normalize::ColumnType;
use crate::seed::{bounded, derive};

const POINT_FILTER_DOMAIN: u64 = 0x706f_696e_745f_6964;

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

pub fn workloads(seed: u64, row_count: usize) -> Vec<Workload> {
    let selected_id = selected_point_id(seed, row_count);
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

fn selected_point_id(seed: u64, row_count: usize) -> usize {
    bounded(derive(seed, POINT_FILTER_DOMAIN), row_count)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn workload_diversity_invariants_are_explicit() {
        let workloads = workloads(7, 10_000);
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
    fn point_filter_id_is_reproducible_seed_sensitive_and_in_range() {
        assert_eq!(selected_point_id(41, 10_000), selected_point_id(41, 10_000));
        assert_ne!(selected_point_id(41, 10_000), selected_point_id(42, 10_000));
        assert!(selected_point_id(41, 100) < 100);
        assert!(selected_point_id(41, 1_000) < 1_000);
    }

    #[test]
    fn only_the_point_filter_query_varies_with_seed() {
        let first = workloads(41, 1_000);
        let repeated = workloads(41, 1_000);
        let second = workloads(42, 1_000);

        assert_eq!(
            first
                .iter()
                .map(|workload| &workload.sql)
                .collect::<Vec<_>>(),
            repeated
                .iter()
                .map(|workload| &workload.sql)
                .collect::<Vec<_>>()
        );
        assert_ne!(first[1].sql, second[1].sql);
        for index in [0, 2, 3, 4, 5, 6, 7] {
            assert_eq!(first[index].sql, second[index].sql);
        }
    }
}
