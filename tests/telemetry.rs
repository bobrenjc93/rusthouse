//! End-to-end telemetry tests at the public SQL boundary.

use rusthouse::{
    Database, Error, QueryFailure, QueryResult, QueryStatus, SqlTextRetention, StatementResult,
    TelemetryConfig, Value,
};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

#[test]
fn counters_are_exact_and_available_through_system_telemetry() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (kind String, amount Int64); \
             INSERT INTO events VALUES ('a', 2), ('b', 3), ('a', 5);",
        )
        .expect("setup succeeds");
    let result = query(
        &mut database,
        "SELECT kind, SUM(amount) AS total FROM events \
         WHERE amount >= 3 GROUP BY kind ORDER BY total DESC LIMIT 1",
    );
    assert_eq!(
        result.rows,
        vec![vec![Value::String("a".to_owned()), Value::Int64(5)]]
    );

    let counters = database.telemetry_counters();
    assert_eq!(counters.executions, 2);
    assert_eq!(counters.successful_executions, 2);
    assert_eq!(counters.failed_executions, 0);
    assert_eq!(counters.metrics.rows_scanned, 3);
    assert_eq!(counters.metrics.rows_matched, 2);
    assert_eq!(counters.metrics.groups_created, 2);
    assert_eq!(counters.metrics.rows_written, 3);
    assert_eq!(counters.metrics.result_rows, 1);

    let telemetry = query(
        &mut database,
        "SELECT executions, successful_executions, failed_executions, elapsed_micros, \
                rows_scanned, rows_matched, groups_created, rows_written, result_rows, \
                query_log_entries, query_log_capacity, sql_text_retention_enabled \
         FROM system.telemetry",
    );
    let [row] = telemetry.rows.as_slice() else {
        panic!("system.telemetry has one row");
    };
    assert!(matches!(row[3], Value::Int64(elapsed) if elapsed >= 0));
    assert_eq!(
        [
            &row[0], &row[1], &row[2], &row[4], &row[5], &row[6], &row[7], &row[8], &row[9],
            &row[10], &row[11]
        ],
        [
            &Value::Int64(2),
            &Value::Int64(2),
            &Value::Int64(0),
            &Value::Int64(3),
            &Value::Int64(2),
            &Value::Int64(2),
            &Value::Int64(3),
            &Value::Int64(1),
            &Value::Int64(2),
            &Value::Int64(128),
            &Value::Bool(false),
        ]
    );

    let counters = database.telemetry_counters();
    assert_eq!(counters.executions, 3);
    assert_eq!(counters.metrics.rows_scanned, 4);
    assert_eq!(counters.metrics.rows_matched, 3);
    assert_eq!(counters.metrics.result_rows, 2);
}

#[test]
fn query_log_evicts_oldest_entries_and_truncates_sql() {
    let mut database = Database::with_telemetry_config(TelemetryConfig {
        query_log_capacity: 2,
        sql_text_retention: SqlTextRetention::Truncate(12),
    });
    database
        .execute("CREATE TABLE numbers (n Int64)")
        .expect("create succeeds");
    database
        .execute("INSERT INTO numbers VALUES (1), (2)")
        .expect("insert succeeds");
    query(&mut database, "SELECT * FROM numbers ORDER BY n");

    let retained = database.query_log().collect::<Vec<_>>();
    assert_eq!(
        retained
            .iter()
            .map(|entry| entry.query_id)
            .collect::<Vec<_>>(),
        [2, 3]
    );
    assert!(retained.iter().all(|entry| entry.sql_text_truncated));
    assert!(
        retained
            .iter()
            .all(|entry| entry.sql_text.as_ref().is_some_and(|sql| sql.len() <= 12))
    );

    let log = query(
        &mut database,
        "SELECT query_id, status, rows_scanned, rows_written, result_rows, \
                sql_text_retained, sql_text_truncated \
         FROM system.query_log ORDER BY query_id",
    );
    assert_eq!(
        log.rows,
        vec![
            vec![
                Value::Int64(2),
                Value::String("Succeeded".to_owned()),
                Value::Int64(0),
                Value::Int64(2),
                Value::Int64(0),
                Value::Bool(true),
                Value::Bool(true),
            ],
            vec![
                Value::Int64(3),
                Value::String("Succeeded".to_owned()),
                Value::Int64(2),
                Value::Int64(0),
                Value::Int64(2),
                Value::Bool(true),
                Value::Bool(true),
            ],
        ]
    );

    assert_eq!(
        database
            .query_log()
            .map(|entry| entry.query_id)
            .collect::<Vec<_>>(),
        [3, 4]
    );
}

#[test]
fn failures_have_typed_status_and_preserve_completed_work_metrics() {
    let mut database = Database::new();
    let error = database
        .execute(
            "CREATE TABLE partial (n Int64); \
             INSERT INTO partial VALUES (1), (2); \
             SELECT * FROM missing",
        )
        .expect_err("last statement fails");
    assert!(matches!(error, Error::TableNotFound(name) if name == "missing"));

    let entry = database.query_log().next_back().expect("failure is logged");
    assert_eq!(
        entry.status,
        QueryStatus::Failed(QueryFailure::TableNotFound)
    );
    assert_eq!(entry.metrics.rows_written, 2);
    assert_eq!(entry.metrics.rows_scanned, 0);

    let parse_error = database
        .execute("SELECT FROM partial")
        .expect_err("invalid SQL fails");
    assert!(matches!(parse_error, Error::Sql { .. }));
    let entry = database
        .query_log()
        .next_back()
        .expect("parse failure is logged");
    assert_eq!(entry.status, QueryStatus::Failed(QueryFailure::Sql));
    assert_eq!(entry.metrics, Default::default());

    let counters = database.telemetry_counters();
    assert_eq!(counters.executions, 2);
    assert_eq!(counters.successful_executions, 0);
    assert_eq!(counters.failed_executions, 2);
    assert_eq!(counters.metrics.rows_written, 2);

    let failures = query(
        &mut database,
        "SELECT query_id, status, failure_type, rows_written \
         FROM system.query_log WHERE status = 'Failed' ORDER BY query_id",
    );
    assert_eq!(
        failures.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::String("Failed".to_owned()),
                Value::String("TableNotFound".to_owned()),
                Value::Int64(2),
            ],
            vec![
                Value::Int64(2),
                Value::String("Failed".to_owned()),
                Value::String("Sql".to_owned()),
                Value::Int64(0),
            ],
        ]
    );
}

#[test]
fn system_tables_are_read_only_and_default_sql_retention_is_disabled() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE user_data (n Int64)")
        .expect("user table succeeds");
    assert_eq!(
        database
            .query_log()
            .next_back()
            .expect("entry exists")
            .sql_text,
        None
    );

    let error = database
        .execute("INSERT INTO system.query_log VALUES (1)")
        .expect_err("system tables reject writes");
    assert!(matches!(
        error,
        Error::ReadOnlySystemTable(name) if name == "system.query_log"
    ));
    assert_eq!(
        database
            .query_log()
            .next_back()
            .expect("failure is logged")
            .status,
        QueryStatus::Failed(QueryFailure::ReadOnlySystemTable)
    );
}

#[test]
fn telemetry_configuration_does_not_change_query_outcomes() {
    let sql = "CREATE TABLE values_table (category String, value Int64); \
               INSERT INTO values_table VALUES ('x', 1), ('y', 8), ('x', 3); \
               SELECT category, SUM(value) AS total FROM values_table \
               WHERE value >= 2 GROUP BY category ORDER BY total DESC";
    let mut without_log = Database::with_telemetry_config(TelemetryConfig {
        query_log_capacity: 0,
        sql_text_retention: SqlTextRetention::Disabled,
    });
    let mut with_log = Database::with_telemetry_config(TelemetryConfig {
        query_log_capacity: 4,
        sql_text_retention: SqlTextRetention::Truncate(8),
    });

    assert_eq!(
        without_log.execute(sql).expect("query succeeds"),
        with_log.execute(sql).expect("query succeeds")
    );
    assert_eq!(without_log.query_log().len(), 0);
    assert_eq!(with_log.query_log().len(), 1);
    assert_eq!(
        without_log.telemetry_counters().metrics,
        with_log.telemetry_counters().metrics
    );
}
