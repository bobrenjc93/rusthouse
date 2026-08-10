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
fn fresh_empty_all_null_and_mixed_extrema_have_nullable_float_average_semantics() {
    for values in [Vec::new(), vec![None, None, None]] {
        let mut database = database_with_values(values);
        assert_eq!(
            query(&mut database, "SELECT AVG(v) AS mean FROM readings"),
            QueryResult {
                columns: vec![ResultColumn {
                    name: "mean".to_owned(),
                    data_type: DataType::Float64,
                }],
                rows: vec![vec![Value::Null(DataType::Float64)]],
            }
        );
    }

    let mut database = database_with_values(vec![Some(i64::MAX), None, Some(i64::MIN), None]);
    assert_eq!(
        query(&mut database, "SELECT AVG(v) FROM readings").rows,
        [vec![Value::Float64(-0.5)]]
    );
}

#[test]
fn nullable_average_composes_with_mixed_groups_and_other_aggregates() {
    let mut database = database_with_values(Vec::new());
    database
        .execute(
            "ALTER TABLE readings ADD COLUMN cohort String; \
             INSERT INTO readings VALUES \
                 (NULL, 'nulls'), (NULL, 'nulls'), \
                 (9223372036854775807, 'extreme'), \
                 (-9223372036854775808, 'extreme'), \
                 (2, 'low'), (4, 'low'), (NULL, 'low'), \
                 (10, 'high'), (14, 'high'), \
                 (NULL, 'ignored');",
        )
        .expect("mixed nullable groups are valid");

    let groups = query(
        &mut database,
        "SELECT cohort, COUNT(*) AS rows, COUNT(v) AS present, SUM(v) AS total, \
                MIN(v) AS minimum, MAX(v) AS maximum, AVG(v) AS mean \
         FROM readings WHERE cohort != 'ignored' GROUP BY cohort \
         HAVING mean IS NOT NULL ORDER BY mean DESC, cohort ASC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        groups.rows,
        [
            vec![
                Value::String("low".to_owned()),
                Value::Int64(3),
                Value::Int64(2),
                Value::Int64(6),
                Value::Int64(2),
                Value::Int64(4),
                Value::Float64(3.0),
            ],
            vec![
                Value::String("extreme".to_owned()),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(-1),
                Value::Int64(i64::MIN),
                Value::Int64(i64::MAX),
                Value::Float64(-0.5),
            ],
        ]
    );

    let null_group = query(
        &mut database,
        "SELECT cohort, COUNT(*) AS rows, COUNT(v) AS present, SUM(v) AS total, \
                MIN(v) AS minimum, MAX(v) AS maximum, AVG(v) AS mean \
         FROM readings GROUP BY cohort HAVING mean IS NULL \
         ORDER BY cohort ASC LIMIT 1 OFFSET 0",
    );
    assert_eq!(
        null_group.rows,
        [vec![
            Value::String("ignored".to_owned()),
            Value::Int64(1),
            Value::Int64(0),
            Value::Null(DataType::Int64),
            Value::Null(DataType::Int64),
            Value::Null(DataType::Int64),
            Value::Null(DataType::Float64),
        ]]
    );
}

#[test]
fn nullable_average_obeys_exact_and_exceeded_query_resource_limits() {
    let values = vec![None, Some(i64::MAX), Some(i64::MIN)];
    let exact_limits = QueryResultLimits {
        max_scan_rows: values.len(),
        max_rows: 1,
        max_values: 1,
        max_groups: 1,
        max_aggregate_state_cells: 1,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(exact_limits);
    exact
        .create_nullable_int64_table("readings", "v", values.clone())
        .unwrap();
    assert_eq!(
        query(&mut exact, "SELECT AVG(v) FROM readings").rows,
        [vec![Value::Float64(-0.5)]]
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
                max_rows: 0,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: 1,
                max: 0,
            },
        ),
        (
            QueryResultLimits {
                max_values: 0,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: 1,
                max: 0,
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
            database.execute("SELECT AVG(v) FROM readings"),
            Err(expected)
        );
    }
}
