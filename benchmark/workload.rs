use crate::normalize::ColumnType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    FullScanAggregate,
    SelectiveFilter,
    CompoundFilter,
    NonselectiveFilter,
    LowCardinalityGroupBy,
    IntermediateCardinalityGroupBy,
    HighCardinalityGroupBy,
    OrderByLimit,
    SelectivitySweep,
    StringPredicate,
    ProjectionScan,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Self::FullScanAggregate => "full_scan_aggregate",
            Self::SelectiveFilter => "selective_filter",
            Self::CompoundFilter => "compound_filter",
            Self::NonselectiveFilter => "nonselective_filter",
            Self::LowCardinalityGroupBy => "low_cardinality_group_by",
            Self::IntermediateCardinalityGroupBy => "intermediate_cardinality_group_by",
            Self::HighCardinalityGroupBy => "high_cardinality_group_by",
            Self::OrderByLimit => "order_by_limit",
            Self::SelectivitySweep => "selectivity_sweep",
            Self::StringPredicate => "string_predicate",
            Self::ProjectionScan => "projection_scan",
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
    ]
}

pub fn audit_workloads(row_count: usize) -> Vec<Workload> {
    let mut audit = workloads(row_count);
    audit.extend([
        Workload {
            name: "float_full_scan_aggregate",
            family: Family::FullScanAggregate,
            sql: "SELECT COUNT(*) AS row_count, SUM(score) AS score_total, MIN(score) AS score_min, MAX(score) AS score_max, AVG(score) AS score_mean FROM parity_data;".to_owned(),
            columns: vec![
                ("row_count", ColumnType::Integer),
                ("score_total", ColumnType::Float),
                ("score_min", ColumnType::Float),
                ("score_max", ColumnType::Float),
                ("score_mean", ColumnType::Float),
            ],
        },
        Workload {
            name: "numeric_selectivity_1pct",
            family: Family::SelectivitySweep,
            sql: "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE uniform_num < -980000;".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "numeric_selectivity_10pct",
            family: Family::SelectivitySweep,
            sql: "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE uniform_num < -800000;".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "numeric_selectivity_50pct",
            family: Family::SelectivitySweep,
            sql: "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE uniform_num < 0;".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "string_equality_aggregate",
            family: Family::StringPredicate,
            sql: "SELECT COUNT(*) AS matched, SUM(skewed_num) AS total, AVG(score) AS mean_score FROM parity_data WHERE low_key = 'amber';".to_owned(),
            columns: vec![
                ("matched", ColumnType::Integer),
                ("total", ColumnType::Integer),
                ("mean_score", ColumnType::Float),
            ],
        },
        Workload {
            name: "intermediate_cardinality_group_by",
            family: Family::IntermediateCardinalityGroupBy,
            sql: "SELECT payload, flag, COUNT(*) AS row_count, SUM(uniform_num) AS total FROM parity_data GROUP BY payload, flag ORDER BY payload, flag;".to_owned(),
            columns: vec![
                ("payload", ColumnType::String),
                ("flag", ColumnType::Boolean),
                ("row_count", ColumnType::Integer),
                ("total", ColumnType::Integer),
            ],
        },
        Workload {
            name: "numeric_projection_scan",
            family: Family::ProjectionScan,
            sql: "SELECT id, uniform_num, score, payload FROM parity_data WHERE uniform_num < -990000 ORDER BY id;".to_owned(),
            columns: vec![
                ("id", ColumnType::Integer),
                ("uniform_num", ColumnType::Integer),
                ("score", ColumnType::Float),
                ("payload", ColumnType::String),
            ],
        },
        Workload {
            name: "string_projection_scan",
            family: Family::ProjectionScan,
            sql: "SELECT id, low_key, payload, flag FROM parity_data WHERE low_key = 'amber' AND uniform_num < -950000 ORDER BY id;".to_owned(),
            columns: vec![
                ("id", ColumnType::Integer),
                ("low_key", ColumnType::String),
                ("payload", ColumnType::String),
                ("flag", ColumnType::Boolean),
            ],
        },
    ]);
    audit
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
    fn audit_adds_required_query_shapes_without_changing_default_workloads() {
        let default = workloads(10_000);
        let audit = audit_workloads(10_000);

        assert_eq!(default.len(), 8);
        assert_eq!(audit.len(), 16);
        assert!(
            audit
                .iter()
                .any(|workload| workload.sql.contains("SUM(score)"))
        );
        assert!(audit.iter().any(|workload| {
            workload.family == Family::IntermediateCardinalityGroupBy
                && !workload.sql.contains(" LIMIT ")
        }));
        assert!(audit.iter().any(|workload| {
            workload.family == Family::StringPredicate && workload.sql.contains("low_key = 'amber'")
        }));
        assert_eq!(
            audit
                .iter()
                .filter(|workload| workload.family == Family::ProjectionScan)
                .filter(|workload| !workload.sql.contains(" LIMIT "))
                .count(),
            2
        );
        assert_eq!(
            audit
                .iter()
                .filter(|workload| workload.family == Family::SelectivitySweep)
                .count(),
            3
        );
    }
}
