use crate::normalize::{ColumnSpec, ColumnType};

const INT64: &[&str] = &["Int64"];
const UINT64: &[&str] = &["UInt64"];
const FLOAT64: &[&str] = &["Float64"];
const BOOL: &[&str] = &["Bool"];
const STRING: &[&str] = &["String"];

const fn integer(name: &'static str) -> ColumnSpec {
    ColumnSpec::new(name, ColumnType::Integer, INT64, INT64)
}

const fn count(name: &'static str) -> ColumnSpec {
    ColumnSpec::new(name, ColumnType::Integer, INT64, UINT64)
}

const fn float(name: &'static str) -> ColumnSpec {
    ColumnSpec::new(name, ColumnType::Float, FLOAT64, FLOAT64)
}

const fn boolean(name: &'static str) -> ColumnSpec {
    ColumnSpec::new(name, ColumnType::Boolean, BOOL, BOOL)
}

const fn string(name: &'static str) -> ColumnSpec {
    ColumnSpec::new(name, ColumnType::String, STRING, STRING)
}

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
    pub columns: Vec<ColumnSpec>,
}

pub fn workloads(row_count: usize) -> Vec<Workload> {
    let selected_id = row_count / 2;
    vec![
        Workload {
            name: "full_scan_aggregate",
            family: Family::FullScanAggregate,
            sql: "SELECT COUNT(*) AS row_count, SUM(uniform_num) AS uniform_total, SUM(skewed_num) AS skewed_total, MIN(large_int) AS large_min, MAX(large_int) AS large_max, AVG(score) AS score_mean FROM parity_data;".to_owned(),
            columns: vec![
                count("row_count"),
                integer("uniform_total"),
                integer("skewed_total"),
                integer("large_min"),
                integer("large_max"),
                float("score_mean"),
            ],
        },
        Workload {
            name: "selective_point_filter",
            family: Family::SelectiveFilter,
            sql: format!("SELECT id, payload, large_int, flag FROM parity_data WHERE id = {selected_id} ORDER BY id;"),
            columns: vec![
                integer("id"),
                string("payload"),
                integer("large_int"),
                boolean("flag"),
            ],
        },
        Workload {
            name: "compound_filter_aggregate",
            family: Family::CompoundFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(uniform_num) AS total FROM parity_data WHERE (flag = true AND uniform_num < -250000) OR (flag = false AND skewed_num >= 5);".to_owned(),
            columns: vec![
                count("matched"),
                integer("total"),
            ],
        },
        Workload {
            name: "nonselective_filter_aggregate",
            family: Family::NonselectiveFilter,
            sql: "SELECT COUNT(*) AS matched, SUM(skewed_num) AS total FROM parity_data WHERE uniform_num >= -950000;".to_owned(),
            columns: vec![
                count("matched"),
                integer("total"),
            ],
        },
        Workload {
            name: "low_cardinality_group_by",
            family: Family::LowCardinalityGroupBy,
            sql: "SELECT low_key, flag, COUNT(*) AS row_count, SUM(uniform_num) AS total, AVG(score) AS mean_score FROM parity_data GROUP BY low_key, flag ORDER BY low_key, flag;".to_owned(),
            columns: vec![
                string("low_key"),
                boolean("flag"),
                count("row_count"),
                integer("total"),
                float("mean_score"),
            ],
        },
        Workload {
            name: "high_cardinality_group_by",
            family: Family::HighCardinalityGroupBy,
            sql: "SELECT high_key, COUNT(*) AS row_count, SUM(skewed_num) AS total FROM parity_data GROUP BY high_key ORDER BY high_key LIMIT 100;".to_owned(),
            columns: vec![
                string("high_key"),
                count("row_count"),
                integer("total"),
            ],
        },
        Workload {
            name: "numeric_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT id, score, payload, uniform_num FROM parity_data ORDER BY score DESC, id LIMIT 25;".to_owned(),
            columns: vec![
                integer("id"),
                float("score"),
                string("payload"),
                integer("uniform_num"),
            ],
        },
        Workload {
            name: "string_order_by_limit",
            family: Family::OrderByLimit,
            sql: "SELECT payload, id, flag FROM parity_data ORDER BY payload, id DESC LIMIT 25;".to_owned(),
            columns: vec![
                string("payload"),
                integer("id"),
                boolean("flag"),
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
    fn emitted_type_compatibility_is_expression_specific() {
        let workloads = workloads(1_000);
        let count = workloads[0].columns[0];
        let projected_id = workloads[1].columns[0];

        assert_eq!(count.compatible_types.rusthouse, &["Int64"]);
        assert_eq!(count.compatible_types.clickhouse, &["UInt64"]);
        assert_eq!(projected_id.compatible_types.rusthouse, &["Int64"]);
        assert_eq!(projected_id.compatible_types.clickhouse, &["Int64"]);
    }
}
