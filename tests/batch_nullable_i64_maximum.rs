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
fn fresh_empty_all_null_and_mixed_extreme_inputs_have_sql_maximum_semantics() {
    for values in [Vec::new(), vec![None, None, None]] {
        let mut database = database_with_values(values);
        assert_eq!(
            query(&mut database, "SELECT MAX(v) AS maximum FROM readings"),
            QueryResult {
                columns: vec![ResultColumn {
                    name: "maximum".to_owned(),
                    data_type: DataType::Int64,
                }],
                rows: vec![vec![Value::Null(DataType::Int64)]],
            }
        );
    }

    let mut database = database_with_values(vec![
        Some(i64::MIN),
        None,
        Some(-7),
        Some(i64::MAX),
        None,
        Some(i64::MAX),
    ]);
    assert_eq!(
        query(&mut database, "SELECT MAX(v) FROM readings").rows,
        [vec![Value::Int64(i64::MAX)]]
    );
}

#[test]
fn nullable_maximum_keeps_filters_grouping_having_ordering_pagination_and_count() {
    let mut database = database_with_values(vec![
        None,
        Some(i64::MAX),
        Some(7),
        Some(i64::MIN),
        None,
        Some(7),
        Some(-1),
    ]);

    let global = query(
        &mut database,
        "SELECT COUNT(*) AS rows, COUNT(v) AS present, MAX(v) AS maximum FROM readings",
    );
    assert_eq!(
        global.rows,
        [vec![
            Value::Int64(7),
            Value::Int64(5),
            Value::Int64(i64::MAX),
        ]]
    );

    let paged_groups = query(
        &mut database,
        "SELECT v, MAX(v) AS maximum, COUNT(v) AS present FROM readings WHERE v >= -1 \
         GROUP BY v HAVING maximum IS NOT NULL \
         ORDER BY maximum DESC, present DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        paged_groups.rows,
        [
            vec![Value::Int64(7), Value::Int64(7), Value::Int64(2)],
            vec![Value::Int64(-1), Value::Int64(-1), Value::Int64(1)],
        ]
    );

    let null_group = query(
        &mut database,
        "SELECT v, MAX(v) AS maximum, COUNT(*) AS rows FROM readings GROUP BY v \
         HAVING maximum IS NULL ORDER BY maximum LIMIT 1",
    );
    assert_eq!(
        null_group.rows,
        [vec![
            Value::Null(DataType::Int64),
            Value::Null(DataType::Int64),
            Value::Int64(2),
        ]]
    );
}

#[test]
fn nullable_maximum_obeys_exact_and_exceeded_query_working_limits() {
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
        query(&mut exact, "SELECT MAX(v) FROM readings").rows,
        [vec![Value::Int64(4)]]
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
            database.execute("SELECT MAX(v) FROM readings"),
            Err(expected)
        );
    }
}
