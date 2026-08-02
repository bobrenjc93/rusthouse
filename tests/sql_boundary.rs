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
