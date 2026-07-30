use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::{Database, DatabaseOptions, Error, QueryResult, StatementResult, Value};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rusthouse-spill-test-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique test directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn assert_empty(&self) {
        assert_eq!(
            fs::read_dir(&self.path)
                .expect("read test directory")
                .count(),
            0,
            "spill workspaces should be removed"
        );
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn configured_database(max_in_memory_groups: usize, temporary_directory: &Path) -> Database {
    Database::with_options(DatabaseOptions {
        max_in_memory_groups,
        temporary_directory: Some(temporary_directory.to_path_buf()),
    })
}

fn last_query(results: Vec<StatementResult>) -> QueryResult {
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn forced_spill_matches_in_memory_aggregation() {
    let temporary_directory = TestDirectory::new();
    let setup = "CREATE TABLE sales (
            region String, online Bool, units Int64, revenue Float64
         );
         INSERT INTO sales VALUES
            ('west', true, 4, 5.5),
            ('east', false, 7, 11.25),
            ('north', true, -2, 3.0),
            ('west', true, 6, 7.25),
            ('south', false, 9, 20.0),
            ('east', false, 5, 2.5),
            ('west', false, 1, 8.0),
            ('north', true, 8, 4.75);";
    let query = "SELECT region, online,
                    COUNT(*) AS rows,
                    SUM(units) AS units,
                    SUM(revenue) AS revenue,
                    MIN(units) AS low,
                    MAX(revenue) AS high,
                    AVG(units) AS mean_units,
                    AVG(revenue) AS mean_revenue
                 FROM sales
                 GROUP BY region, online
                 ORDER BY revenue DESC
                 LIMIT 3;";

    let mut in_memory = configured_database(128, temporary_directory.path());
    in_memory.execute(setup).expect("in-memory setup");
    let expected = last_query(in_memory.execute(query).expect("in-memory query"));

    let mut spilled = configured_database(2, temporary_directory.path());
    spilled.execute(setup).expect("spilled setup");
    let actual = last_query(spilled.execute(query).expect("spilled query"));

    assert_eq!(actual, expected);
    temporary_directory.assert_empty();
}

#[test]
fn recursively_repartitions_skewed_partitions() {
    let temporary_directory = TestDirectory::new();
    let mut database = configured_database(1, temporary_directory.path());
    let mut rows = (0..500)
        .map(|_| "('hot', 1)".to_owned())
        .collect::<Vec<_>>();
    rows.extend((0..40).map(|key| format!("('key-{key}', {key})")));
    database
        .execute(&format!(
            "CREATE TABLE events (key String, amount Int64);
             INSERT INTO events VALUES {};",
            rows.join(",")
        ))
        .expect("setup succeeds");

    let result = last_query(
        database
            .execute(
                "SELECT key, COUNT(*) AS rows, SUM(amount) AS total
                 FROM events GROUP BY key ORDER BY key;",
            )
            .expect("recursive spill succeeds"),
    );

    assert_eq!(result.rows.len(), 41);
    assert!(result.rows.contains(&vec![
        Value::String("hot".to_owned()),
        Value::Int64(500),
        Value::Int64(500),
    ]));
    temporary_directory.assert_empty();
}

#[test]
fn spill_preserves_overflow_errors_and_cleans_up() {
    let temporary_directory = TestDirectory::new();
    let setup = "CREATE TABLE totals (key String, amount Int64);
                 INSERT INTO totals VALUES
                    ('overflow', 9223372036854775807),
                    ('overflow', 1),
                    ('other', 0);";
    let query = "SELECT key, SUM(amount) FROM totals GROUP BY key;";

    let mut in_memory = configured_database(8, temporary_directory.path());
    in_memory.execute(setup).expect("in-memory setup");
    let expected = in_memory.execute(query).expect_err("sum overflows");

    let mut spilled = configured_database(1, temporary_directory.path());
    spilled.execute(setup).expect("spilled setup");
    let actual = spilled.execute(query).expect_err("spilled sum overflows");

    assert_eq!(actual, expected);
    assert_eq!(actual, Error::NumericOverflow("SUM(Int64)".to_owned()));
    temporary_directory.assert_empty();
}

#[test]
fn in_memory_grouping_does_not_create_a_spill_directory() {
    let temporary_directory = TestDirectory::new();
    let unused_parent = temporary_directory.path().join("unused");
    let mut database = configured_database(10, &unused_parent);

    let result = last_query(
        database
            .execute(
                "CREATE TABLE flags (enabled Bool);
                 INSERT INTO flags VALUES (true), (false), (true);
                 SELECT enabled, COUNT(*) AS rows
                 FROM flags GROUP BY enabled ORDER BY enabled;",
            )
            .expect("in-memory grouping succeeds"),
    );

    assert_eq!(result.rows.len(), 2);
    assert!(!unused_parent.exists());
}
