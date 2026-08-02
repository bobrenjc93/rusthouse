use rusthouse::{Database, DatabaseError, ExecutionResult, LimitKind, Limits, Value};

fn query(database: &mut Database, sql: &str) -> rusthouse::QueryResult {
    match database.execute_one(sql).unwrap() {
        ExecutionResult::Query(result) => result,
        result => panic!("expected query result, got {result:?}"),
    }
}

#[test]
fn varied_identifiers_types_and_scalar_projections_execute_generally() {
    let mut database = Database::new();
    let results = database
        .execute(
            "CREATE TABLE `Campaign Facts`
                (`Segment Name` String, impressions Int64, rate Float64, enabled Bool);
             INSERT INTO `Campaign Facts` (`enabled`, `rate`, `Segment Name`, `impressions`) VALUES
                (true, 0.25, 'north', 12),
                (false, 1.5, 'contains,comma', 7),
                (true, 2.0, 'north', 8);
             SELECT `Segment Name` AS segment, impressions * rate AS weighted, enabled
             FROM `Campaign Facts`
             WHERE (enabled = true AND impressions >= 8) OR rate > 3.0
             ORDER BY weighted DESC LIMIT 2;",
        )
        .unwrap();
    assert_eq!(results.len(), 3);
    let ExecutionResult::Query(result) = &results[2] else {
        panic!("third statement was not a query");
    };
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("north".into()),
                Value::Float64(16.0),
                Value::Bool(true),
            ],
            vec![
                Value::String("north".into()),
                Value::Float64(3.0),
                Value::Bool(true),
            ],
        ]
    );
}

#[test]
fn all_aggregates_work_with_multiple_schema_shapes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (site String, sample Int64, measurement Float64);
             INSERT INTO readings VALUES
               ('west', 1, 2.5), ('east', 4, 9.0),
               ('west', 3, 7.5), ('east', 2, 1.0);",
        )
        .unwrap();
    let result = query(
        &mut database,
        "SELECT site AS location, count(*) n, sum(sample) samples, min(measurement) low,
                max(measurement) high, avg(measurement) mean
         FROM readings GROUP BY location ORDER BY location",
    );
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Value::String("east".into()));
    assert_eq!(result.rows[0][2], Value::Int64(6));
    assert_eq!(result.rows[1][5], Value::Float64(5.0));
}

#[test]
fn failed_late_row_does_not_partially_append() {
    let mut database = Database::new();
    database
        .execute_one("CREATE TABLE atomicity (a Int64, b Bool)")
        .unwrap();
    database
        .execute_one("INSERT INTO atomicity VALUES (1, true)")
        .unwrap();
    assert!(
        database
            .execute_one("INSERT INTO atomicity VALUES (2, false), (3, 'wrong')")
            .is_err()
    );
    assert_eq!(database.table_row_count("atomicity").unwrap(), 1);
}

#[test]
fn input_column_row_result_and_string_limits_are_enforced() {
    let base = Limits {
        max_input_bytes: 1_000,
        max_rows_per_insert: 2,
        max_rows_per_table: 2,
        max_result_rows: 1,
        max_columns_per_table: 2,
        max_string_bytes: 4,
        ..Limits::default()
    };
    let mut database = Database::with_limits(base.clone());
    database
        .execute("CREATE TABLE bounded (id Int64, tag String); INSERT INTO bounded VALUES (1, 'aa'), (2, 'bb')")
        .unwrap();
    let result_error = database.execute_one("SELECT * FROM bounded").unwrap_err();
    assert!(matches!(
        result_error,
        DatabaseError::LimitExceeded {
            kind: LimitKind::ResultRows,
            ..
        }
    ));
    assert_eq!(
        query(&mut database, "SELECT * FROM bounded LIMIT 1")
            .rows
            .len(),
        1
    );

    let mut columns = Database::with_limits(base.clone());
    assert!(matches!(
        columns.execute_one("CREATE TABLE too_wide (a Int64, b Int64, c Int64)"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ColumnsPerTable,
            ..
        })
    ));

    let mut input = Database::with_limits(Limits {
        max_input_bytes: 5,
        ..base
    });
    assert!(matches!(
        input.execute_one("SELECT 1"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::InputBytes,
            ..
        })
    ));
}

#[test]
fn deeply_nested_and_flat_expressions_return_typed_errors_without_crashing() {
    let mut database = Database::new();
    let nested = format!("SELECT {}1{}", "(".repeat(50_000), ")".repeat(50_000));
    assert!(matches!(
        database.execute_one(&nested),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ExpressionDepth,
            ..
        })
    ));

    let flat = format!("SELECT {}", vec!["1"; 1_000].join(" + "));
    assert!(matches!(
        database.execute_one(&flat),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ExpressionDepth,
            ..
        })
    ));
}

#[test]
fn execute_one_rejects_multiple_statements_before_mutating_catalog() {
    let mut database = Database::new();
    let error = database
        .execute_one("CREATE TABLE first (id Int64); CREATE TABLE second (id Int64)")
        .unwrap_err();
    assert!(matches!(error, DatabaseError::InvalidQuery(_)));
    assert!(matches!(
        database.schema("first"),
        Err(DatabaseError::TableNotFound(_))
    ));
    assert!(matches!(
        database.schema("second"),
        Err(DatabaseError::TableNotFound(_))
    ));
}

#[test]
fn quoted_dotted_columns_remain_distinct_from_qualification() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE dotted (\"a.b\" Int64, plain String);
             INSERT INTO dotted VALUES (7, 'ok')",
        )
        .unwrap();

    let wildcard = query(&mut database, "SELECT * FROM dotted");
    assert_eq!(
        wildcard.rows,
        vec![vec![Value::Int64(7), Value::String("ok".into())]]
    );
    let direct = query(
        &mut database,
        "SELECT \"a.b\" AS dotted_name, dotted.plain FROM dotted",
    );
    assert_eq!(direct.rows, wildcard.rows);
}

#[test]
fn not_binds_to_comparisons_before_and_or() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE truth (id Int64, enabled Bool);
             INSERT INTO truth VALUES (1, true), (2, true), (3, false)",
        )
        .unwrap();
    let result = query(
        &mut database,
        "SELECT id FROM truth WHERE NOT id = 1 AND enabled = true ORDER BY id",
    );
    assert_eq!(result.rows, vec![vec![Value::Int64(2)]]);
}

#[test]
fn string_limit_applies_to_query_literals_before_execution() {
    let mut database = Database::with_limits(Limits {
        max_string_bytes: 1,
        ..Limits::default()
    });
    assert!(matches!(
        database.execute_one("SELECT 'oversized'"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::StringBytes,
            ..
        })
    ));
}

#[test]
fn limit_streams_projections_and_order_by_uses_bounded_top_k() {
    let values = (0..100)
        .rev()
        .map(|id| format!("({id})"))
        .collect::<Vec<_>>()
        .join(",");
    let mut database = Database::with_limits(Limits {
        max_intermediate_rows: 2,
        max_intermediate_bytes: 4 * 1024,
        max_result_bytes: 512,
        ..Limits::default()
    });
    database
        .execute(&format!(
            "CREATE TABLE bounded_query (id Int64); INSERT INTO bounded_query VALUES {values}"
        ))
        .unwrap();

    let projected = query(
        &mut database,
        "SELECT 'a moderately sized result string' AS text FROM bounded_query LIMIT 1",
    );
    assert_eq!(projected.rows.len(), 1);
    assert!(matches!(
        database
            .execute_one("SELECT 'a moderately sized result string' AS text FROM bounded_query"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ResultBytes,
            ..
        })
    ));
    let first_only = query(
        &mut database,
        "SELECT 10 / (id - 98) AS quotient FROM bounded_query LIMIT 1",
    );
    assert_eq!(first_only.rows, vec![vec![Value::Float64(10.0)]]);

    let sorted = query(
        &mut database,
        "SELECT id FROM bounded_query ORDER BY id ASC LIMIT 2",
    );
    assert_eq!(
        sorted.rows,
        vec![vec![Value::Int64(0)], vec![Value::Int64(1)]]
    );
    assert!(matches!(
        database.execute_one("SELECT id FROM bounded_query ORDER BY id LIMIT 3"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::IntermediateRows,
            ..
        })
    ));

    let mut byte_limited = Database::with_limits(Limits {
        max_intermediate_bytes: 1,
        ..Limits::default()
    });
    byte_limited
        .execute(
            "CREATE TABLE byte_limit (id Int64);
             INSERT INTO byte_limit VALUES (2), (1)",
        )
        .unwrap();
    assert!(matches!(
        byte_limited.execute_one("SELECT id FROM byte_limit ORDER BY id LIMIT 1"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::IntermediateBytes,
            ..
        })
    ));
}
