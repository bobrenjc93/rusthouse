//! Deterministic execution-budget and spill boundary tests.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use rusthouse::format::{OutputFormat, render, render_with_limit};
use rusthouse::{Database, Error, ExecutionLimits, QueryResult, Resource, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .pop()
        .expect("statement result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn assert_limit(error: Error, resource: Resource, limit: usize, actual: usize) {
    assert_eq!(
        error,
        Error::ResourceLimitExceeded {
            resource,
            limit,
            actual,
        }
    );
}

#[test]
fn input_token_and_statement_limits_fail_at_the_first_excess() {
    let sql = "SELECT * FROM t";
    let mut database = Database::with_limits(ExecutionLimits {
        max_input_bytes: sql.len() - 1,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute(sql)
            .expect_err("input is one byte too long"),
        Resource::InputBytes,
        sql.len() - 1,
        sql.len(),
    );
    assert_eq!(database.last_execution_stats().input_bytes, sql.len());

    database.set_limits(ExecutionLimits {
        max_tokens: 3,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database.execute(sql).expect_err("fourth token is rejected"),
        Resource::Tokens,
        3,
        4,
    );

    database.set_limits(ExecutionLimits {
        max_statements: 1,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("SELECT * FROM t; SELECT * FROM t")
            .expect_err("second statement is rejected before execution"),
        Resource::Statements,
        1,
        2,
    );
    assert!(database.catalog().table("t").is_err());
}

#[test]
fn schema_and_stored_value_limits_leave_catalog_mutations_atomic() {
    let mut database = Database::with_limits(ExecutionLimits {
        max_schema_width: 2,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("CREATE TABLE wide (a Int64, b Int64, c Int64)")
            .expect_err("third column is rejected"),
        Resource::SchemaWidth,
        2,
        3,
    );
    assert!(database.catalog().table("wide").is_err());

    database.set_limits(ExecutionLimits {
        max_stored_values: 4,
        ..ExecutionLimits::default()
    });
    database
        .execute("CREATE TABLE narrow (a Int64, b String)")
        .expect("schema fits");
    database
        .execute("INSERT INTO narrow VALUES (1, 'a'), (2, 'b')")
        .expect("four values fit exactly");
    assert_limit(
        database
            .execute("INSERT INTO narrow VALUES (3, 'c')")
            .expect_err("fifth and sixth values are rejected"),
        Resource::StoredValues,
        4,
        6,
    );
    assert_eq!(database.catalog().table("narrow").unwrap().row_count(), 2);
    assert_eq!(database.last_execution_stats().stored_values, 4);
}

#[test]
fn intermediate_and_result_rows_have_independent_exact_limits() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE t (n Int64); INSERT INTO t VALUES (3), (1), (2)")
        .expect("setup succeeds");

    database.set_limits(ExecutionLimits {
        max_intermediate_rows: 2,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("SELECT n FROM t ORDER BY n")
            .expect_err("third sorter input is rejected"),
        Resource::IntermediateRows,
        2,
        3,
    );

    database.set_limits(ExecutionLimits {
        max_result_rows: 2,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("SELECT n FROM t")
            .expect_err("third result is rejected"),
        Resource::ResultRows,
        2,
        3,
    );
    assert_eq!(database.last_execution_stats().result_rows, 2);
}

#[test]
fn sorting_and_grouping_spill_with_deterministic_results_and_cleanup() {
    let rows = (0..120)
        .map(|number| format!("({}, {}, '{}')", number, number % 3, 120 - number))
        .collect::<Vec<_>>()
        .join(",");
    let spill_directory = std::env::temp_dir().join(format!(
        "rusthouse-spill-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    fs::create_dir(&spill_directory).expect("create isolated spill directory");
    let mut database = Database::with_limits_and_spill_directory(
        ExecutionLimits {
            max_memory_bytes: 768,
            ..ExecutionLimits::default()
        },
        &spill_directory,
    );
    database
        .execute(&format!(
            "CREATE TABLE t (n Int64, bucket Int64, label String); INSERT INTO t VALUES {rows}"
        ))
        .expect("setup succeeds");

    let ordered = query(
        &mut database,
        "SELECT n, label FROM t ORDER BY label, n DESC LIMIT 4",
    );
    assert_eq!(
        ordered.rows,
        vec![
            vec![Value::Int64(119), Value::String("1".to_owned())],
            vec![Value::Int64(110), Value::String("10".to_owned())],
            vec![Value::Int64(20), Value::String("100".to_owned())],
            vec![Value::Int64(19), Value::String("101".to_owned())],
        ]
    );
    assert!(database.last_execution_stats().spill_runs > 0);
    assert!(database.last_execution_stats().spilled_bytes > 0);
    assert_eq!(database.last_execution_stats().statements, 1);
    assert_eq!(database.last_execution_stats().stored_values, 360);
    assert_eq!(database.last_execution_stats().intermediate_rows, 120);
    assert_eq!(database.last_execution_stats().result_rows, 4);
    assert!(database.last_execution_stats().peak_memory_bytes <= 768);

    database.set_limits(ExecutionLimits {
        max_memory_bytes: 40,
        ..ExecutionLimits::default()
    });
    assert_limit(
        database
            .execute("SELECT n FROM t ORDER BY n")
            .expect_err("two merge heads do not fit"),
        Resource::MemoryBytes,
        40,
        64,
    );
    assert_eq!(
        fs::read_dir(&spill_directory)
            .expect("read spill directory after error")
            .count(),
        0,
        "temporary runs are removed after an error"
    );

    database.set_limits(ExecutionLimits {
        max_memory_bytes: 768,
        ..ExecutionLimits::default()
    });

    let grouped = query(
        &mut database,
        "SELECT bucket, COUNT(*) AS rows, SUM(n) AS total
         FROM t GROUP BY bucket ORDER BY total DESC LIMIT 2",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![Value::Int64(2), Value::Int64(40), Value::Int64(2420)],
            vec![Value::Int64(1), Value::Int64(40), Value::Int64(2380)],
        ]
    );
    assert!(database.last_execution_stats().spill_runs > 0);
    assert_eq!(database.last_execution_stats().intermediate_rows, 123);
    assert_eq!(database.last_execution_stats().result_rows, 2);
    assert!(database.last_execution_stats().peak_memory_bytes <= 768);
    assert_eq!(
        fs::read_dir(&spill_directory)
            .expect("read spill directory")
            .count(),
        0,
        "temporary runs are removed after each query"
    );
    fs::remove_dir(&spill_directory).expect("remove isolated spill directory");
}

#[test]
fn memory_and_rendered_byte_limits_report_deterministic_sizes() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE t (label String); INSERT INTO t VALUES ('abcdef')")
        .expect("setup succeeds");
    database.set_limits(ExecutionLimits {
        max_memory_bytes: 8,
        ..ExecutionLimits::default()
    });
    let error = database
        .execute("SELECT label FROM t")
        .expect_err("owned result cannot fit");
    assert!(matches!(
        error,
        Error::ResourceLimitExceeded {
            resource: Resource::MemoryBytes,
            limit: 8,
            actual
        } if actual > 8
    ));

    database.set_limits(ExecutionLimits::default());
    let result = query(&mut database, "SELECT label FROM t");
    let complete = render(&result, OutputFormat::Json);
    assert_eq!(
        render_with_limit(&result, OutputFormat::Json, complete.len())
            .expect("exact rendered size is accepted"),
        complete
    );
    assert_limit(
        render_with_limit(&result, OutputFormat::Json, complete.len() - 1)
            .expect_err("one byte below exact size is rejected"),
        Resource::RenderedBytes,
        complete.len() - 1,
        complete.len(),
    );
}
