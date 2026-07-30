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

fn query_rows(results: Vec<StatementResult>) -> Vec<Vec<rusthouse::Value>> {
    match results.into_iter().last().expect("query result") {
        StatementResult::Query(result) => result.rows,
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
fn output_limit_caps_projection_capacity() {
    let values = (0..1_000)
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>()
        .join(",");
    let mut database = Database::new();
    database
        .execute(&format!(
            "CREATE TABLE many (id Int64); INSERT INTO many VALUES {values}"
        ))
        .expect("setup succeeds");

    assert_eq!(
        database
            .execute_with_options("SELECT id FROM many", limits(1_000, 0))
            .expect_err("zero output budget aborts before allocating result slots"),
        Error::ExecutionLimitExceeded {
            limit: ExecutionLimit::OutputRows,
            maximum: 0,
            actual: 1,
        }
    );
}

#[test]
fn controlled_sorting_preserves_values_ties_and_top_k_boundaries() {
    use rusthouse::Value::{Int64, String as Text};

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE ranked (id Int64, score Int64, label String);
             INSERT INTO ranked VALUES
                (5, 9, 'b'), (2, 9, 'a'), (3, 9, 'a'),
                (1, 5, 'z'), (4, 1, 'x');
             CREATE TABLE grouped (name String, amount Int64);
             INSERT INTO grouped VALUES ('b', 10), ('a', 10), ('c', 5);",
        )
        .expect("setup succeeds");

    let full = database
        .execute_with_options(
            "SELECT id, score, label FROM ranked \
             ORDER BY score DESC, label",
            limits(5, 5),
        )
        .expect("controlled full sort succeeds");
    assert_eq!(
        query_rows(full),
        vec![
            vec![Int64(2), Int64(9), Text("a".to_owned())],
            vec![Int64(3), Int64(9), Text("a".to_owned())],
            vec![Int64(5), Int64(9), Text("b".to_owned())],
            vec![Int64(1), Int64(5), Text("z".to_owned())],
            vec![Int64(4), Int64(1), Text("x".to_owned())],
        ]
    );

    let top = database
        .execute_with_options(
            "SELECT id, score, label FROM ranked \
             ORDER BY score DESC, label, id DESC LIMIT 3",
            limits(5, 3),
        )
        .expect("controlled top-k succeeds");
    assert_eq!(
        query_rows(top),
        vec![
            vec![Int64(3), Int64(9), Text("a".to_owned())],
            vec![Int64(2), Int64(9), Text("a".to_owned())],
            vec![Int64(5), Int64(9), Text("b".to_owned())],
        ]
    );

    let grouped = database
        .execute_with_options(
            "SELECT name, SUM(amount) AS total FROM grouped \
             GROUP BY name ORDER BY total DESC LIMIT 2",
            limits(3, 2),
        )
        .expect("controlled grouped top-k succeeds");
    assert_eq!(
        query_rows(grouped),
        vec![
            vec![Text("a".to_owned()), Int64(10)],
            vec![Text("b".to_owned()), Int64(10)],
        ]
    );
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
