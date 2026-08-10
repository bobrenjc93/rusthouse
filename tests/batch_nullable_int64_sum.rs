use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result")
    };
    result.clone()
}

fn database_with_values(values: Vec<Option<i64>>) -> Database {
    let mut database = Database::new();
    database
        .create_nullable_int64_table("Readings", "v", values)
        .expect("nullable table is valid");
    database
}

#[test]
fn fresh_empty_all_null_and_mixed_inputs_have_sql_sum_semantics() {
    for values in [Vec::new(), vec![None, None, None]] {
        let mut database = database_with_values(values);
        assert_eq!(
            query(&mut database, "SELECT SUM(v) AS total FROM readings"),
            QueryResult {
                columns: vec![ResultColumn {
                    name: "total".to_owned(),
                    data_type: DataType::Int64,
                }],
                rows: vec![vec![Value::Null(DataType::Int64)]],
            }
        );
    }

    let mut database = database_with_values(vec![Some(4), None, Some(-1), None]);
    assert_eq!(
        query(&mut database, "SELECT SUM(v) FROM readings").rows,
        [vec![Value::Int64(3)]]
    );
}

#[test]
fn nullable_sum_preserves_filters_grouping_having_ordering_and_pagination() {
    let mut database = database_with_values(vec![
        None,
        Some(3),
        Some(3),
        Some(-2),
        Some(-2),
        Some(5),
        None,
    ]);

    let groups = query(
        &mut database,
        "SELECT v, SUM(v) AS total FROM readings WHERE v >= -2 \
         GROUP BY v HAVING total >= -4 \
         ORDER BY total DESC, v ASC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        groups.rows,
        [
            vec![Value::Int64(5), Value::Int64(5)],
            vec![Value::Int64(-2), Value::Int64(-4)],
        ]
    );

    let null_group = query(
        &mut database,
        "SELECT v, SUM(v) AS total FROM readings GROUP BY v \
         HAVING total IS NULL ORDER BY total LIMIT 1",
    );
    assert_eq!(
        null_group.rows,
        [vec![
            Value::Null(DataType::Int64),
            Value::Null(DataType::Int64),
        ]]
    );
}

#[test]
fn nullable_sum_obeys_query_working_limits() {
    let values = vec![None, Some(4), Some(-2)];
    let exact_limits = QueryResultLimits {
        max_scan_rows: values.len(),
        max_groups: 1,
        max_aggregate_state_cells: 1,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    exact
        .create_nullable_int64_table("readings", "v", values.clone())
        .unwrap();
    assert_eq!(
        query(&mut exact, "SELECT SUM(v) FROM readings").rows,
        [vec![Value::Int64(2)]]
    );

    for (limits, expected) in [
        (
            QueryResultLimits {
                max_scan_rows: values.len() - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT scanned rows",
                actual: values.len(),
                max: values.len() - 1,
            },
        ),
        (
            QueryResultLimits {
                max_groups: 0,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT groups",
                actual: 1,
                max: 0,
            },
        ),
        (
            QueryResultLimits {
                max_aggregate_state_cells: 0,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT aggregate state cells",
                actual: 1,
                max: 0,
            },
        ),
    ] {
        let mut database = Database::with_query_result_limits(limits);
        database
            .create_nullable_int64_table("readings", "v", values.clone())
            .unwrap();
        assert_eq!(
            database.execute("SELECT SUM(v) FROM readings"),
            Err(expected)
        );
    }
}

#[test]
fn nullable_sum_checks_positive_and_negative_final_overflow() {
    for values in [
        vec![Some(i64::MAX), None, Some(1)],
        vec![Some(i64::MIN), None, Some(-1)],
    ] {
        let mut database = database_with_values(values);
        assert_eq!(
            database.execute("SELECT SUM(v) FROM readings"),
            Err(Error::NumericOverflow("SUM(Int64)".to_owned()))
        );
    }

    let mut cancellation = database_with_values(vec![Some(i64::MAX), Some(1), None, Some(-1)]);
    assert_eq!(
        query(&mut cancellation, "SELECT SUM(v) FROM readings").rows,
        [vec![Value::Int64(i64::MAX)]]
    );
}
