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
    ScalarExpression,
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
            Self::ScalarExpression => "scalar_expression",
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
        Workload {
            name: "expression_projection_filter",
            family: Family::ScalarExpression,
            sql: "SELECT id, (uniform_num + skewed_num * 2) / 3 AS adjusted FROM parity_data WHERE (uniform_num + skewed_num * 2) >= -1000000 ORDER BY adjusted DESC, id LIMIT 25;".to_owned(),
            columns: vec![
                ("id", ColumnType::Integer),
                ("adjusted", ColumnType::Float),
            ],
        },
        Workload {
            name: "grouped_expression_aggregate",
            family: Family::ScalarExpression,
            sql: "SELECT skewed_num * 2 AS doubled, SUM(uniform_num + skewed_num) AS total, AVG(score / 2) AS mean_half_score FROM parity_data GROUP BY skewed_num ORDER BY doubled LIMIT 100;".to_owned(),
            columns: vec![
                ("doubled", ColumnType::Integer),
                ("total", ColumnType::Integer),
                ("mean_half_score", ColumnType::Float),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn workload_diversity_invariants_are_explicit() {
        let workloads = workloads(10_000);
        let families = workloads
            .iter()
            .map(|workload| workload.family)
            .collect::<BTreeSet<_>>();

        assert_eq!(families.len(), 8);
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
        assert!(
            workloads
                .iter()
                .any(|workload| workload.sql.contains("SUM(uniform_num + skewed_num)"))
        );
        assert!(
            workloads
                .iter()
                .any(|workload| workload.sql.contains("ORDER BY adjusted"))
        );
        assert!(workloads.iter().all(|workload| workload.sql.ends_with(';')));
    }

    #[test]
    fn selective_predicate_varies_with_row_count() {
        assert!(workloads(100)[1].sql.contains("id = 50"));
        assert!(workloads(1_000)[1].sql.contains("id = 500"));
    }
}
