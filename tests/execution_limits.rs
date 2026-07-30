use std::sync::{Arc, Barrier};
use std::time::Instant;

use rusthouse::{
    CancellationToken, Database, Error, ExecutionLimit, ExecutionLimits, ExecutionOptions,
    StatementResult,
};

fn database_with_rows() -> Database {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, active Bool, amount Int64);
             INSERT INTO events VALUES
                (1, true, 40),
                (2, false, 10),
                (3, true, 30),
                (4, false, 20);",
        )
        .expect("setup succeeds");
    database
}

fn limits(max_scan_rows: usize, max_output_rows: usize) -> ExecutionLimits {
    ExecutionLimits {
        max_scan_rows: Some(max_scan_rows),
        max_output_rows: Some(max_output_rows),
        deadline: None,
    }
}

fn query_row_count(results: Vec<StatementResult>) -> usize {
    match results.into_iter().last().expect("query result") {
        StatementResult::Query(result) => result.rows.len(),
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn row_limit_boundaries_are_inclusive_and_report_the_attempted_count() {
    let mut database = database_with_rows();

    let rows = database
        .execute_with_options(
            "SELECT id FROM events WHERE active = true ORDER BY id",
            limits(4, 2),
        )
        .expect("exact scan and output limits succeed");
    assert_eq!(query_row_count(rows), 2);

    let scan_error = database
        .execute_with_options("SELECT id FROM events WHERE active = true", limits(3, 2))
        .expect_err("fourth source row exceeds scan maximum");
    assert_eq!(
        scan_error,
        Error::ExecutionLimitExceeded {
            limit: ExecutionLimit::ScanRows,
            maximum: 3,
            actual: 4,
        }
    );

    let output_error = database
        .execute_with_options("SELECT id FROM events WHERE active = true", limits(4, 1))
        .expect_err("second matching row exceeds output maximum");
    assert_eq!(
        output_error,
        Error::ExecutionLimitExceeded {
            limit: ExecutionLimit::OutputRows,
            maximum: 1,
            actual: 2,
        }
    );

    assert_eq!(
        query_row_count(
            database
                .execute("SELECT id FROM events ORDER BY id")
                .expect("database remains reusable after aborts")
        ),
        4
    );
}

#[test]
fn limits_cover_projection_filter_aggregation_grouping_and_sorting_shapes() {
    let cases = [
        ("SELECT id FROM events LIMIT 2", 2),
        ("SELECT id FROM events WHERE active = true", 2),
        ("SELECT id, amount FROM events ORDER BY amount LIMIT 2", 2),
        ("SELECT COUNT(*) AS rows FROM events", 1),
        (
            "SELECT active, COUNT(*) AS rows FROM events GROUP BY active",
            2,
        ),
        (
            "SELECT active, SUM(amount) AS total FROM events \
             GROUP BY active ORDER BY total DESC",
            2,
        ),
    ];

    for (sql, expected_rows) in cases {
        let mut database = database_with_rows();
        let results = database
            .execute_with_options(sql, limits(4, expected_rows))
            .unwrap_or_else(|error| panic!("exact limits failed for {sql:?}: {error}"));
        assert_eq!(query_row_count(results), expected_rows, "query: {sql}");

        let error = database
            .execute_with_options(sql, limits(3, expected_rows))
            .expect_err("each query shape scans every source row");
        assert_eq!(
            error,
            Error::ExecutionLimitExceeded {
                limit: ExecutionLimit::ScanRows,
                maximum: 3,
                actual: 4,
            },
            "query: {sql}"
        );

        let error = database
            .execute_with_options(sql, limits(4, expected_rows - 1))
            .expect_err("each query shape enforces its output boundary");
        assert_eq!(
            error,
            Error::ExecutionLimitExceeded {
                limit: ExecutionLimit::OutputRows,
                maximum: expected_rows - 1,
                actual: expected_rows,
            },
            "query: {sql}"
        );
    }
}

#[test]
fn sql_limit_can_keep_results_within_the_output_maximum() {
    let mut database = database_with_rows();
    let results = database
        .execute_with_options(
            "SELECT id, amount FROM events ORDER BY amount DESC LIMIT 1",
            limits(4, 1),
        )
        .expect("SQL LIMIT reduces emitted rows");
    assert_eq!(query_row_count(results), 1);

    let results = database
        .execute_with_options("SELECT id FROM events LIMIT 0", limits(4, 0))
        .expect("zero SQL and execution limits agree");
    assert_eq!(query_row_count(results), 0);
}

#[test]
fn row_counters_are_cumulative_across_a_batch() {
    let mut database = database_with_rows();
    let sql = "SELECT id FROM events LIMIT 2; SELECT id FROM events LIMIT 2;";

    let results = database
        .execute_with_options(sql, limits(8, 4))
        .expect("exact cumulative limits succeed");
    assert_eq!(results.len(), 2);

    let error = database
        .execute_with_options(sql, limits(7, 4))
        .expect_err("second scan crosses the cumulative maximum");
    assert_eq!(
        error,
        Error::ExecutionLimitExceeded {
            limit: ExecutionLimit::ScanRows,
            maximum: 7,
            actual: 8,
        }
    );
}

#[test]
fn cloned_token_cancels_execution_and_does_not_poison_the_database() {
    let mut database = database_with_rows();
    let token = CancellationToken::new();
    let canceller = token.clone();
    let options = ExecutionOptions::new(ExecutionLimits::unlimited(), token);

    let error = std::thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(2));
        let query_barrier = Arc::clone(&barrier);
        let query_database = &mut database;
        let handle = scope.spawn(move || {
            query_barrier.wait();
            query_database.execute_with_options(
                "SELECT active, SUM(amount) AS total FROM events \
                 GROUP BY active ORDER BY total",
                options,
            )
        });
        canceller.cancel();
        barrier.wait();
        handle
            .join()
            .expect("query thread does not panic")
            .expect_err("cancellation is observed")
    });
    assert_eq!(error, Error::ExecutionCancelled);
    assert!(canceller.is_cancelled());

    assert_eq!(
        query_row_count(
            database
                .execute("SELECT COUNT(*) FROM events")
                .expect("unlimited execution remains usable")
        ),
        1
    );
}

#[test]
fn expired_deadline_returns_a_distinct_structured_error() {
    let mut database = database_with_rows();
    let options = ExecutionLimits {
        max_scan_rows: None,
        max_output_rows: None,
        deadline: Some(Instant::now()),
    };

    assert_eq!(
        database
            .execute_with_options("SELECT * FROM events", options)
            .expect_err("deadline has already expired"),
        Error::DeadlineExceeded
    );
}
