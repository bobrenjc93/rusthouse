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
            "rusthouse-order-test-{}-{sequence}",
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
            "sort workspaces should be removed"
        );
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn configured_database(max_in_memory_sort_rows: usize, temporary_directory: &Path) -> Database {
    Database::with_options(DatabaseOptions {
        max_in_memory_sort_rows,
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
fn forced_multi_run_spill_preserves_typed_multi_column_ordering() {
    let temporary_directory = TestDirectory::new();
    let rows = (0..97)
        .map(|index| {
            format!(
                "({}, {}, '{}')",
                index % 11,
                100 - (index % 7),
                if index % 2 == 0 { "even" } else { "odd" }
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let setup = format!(
        "CREATE TABLE events (bucket Int64, score Int64, label String);
         INSERT INTO events VALUES {rows};"
    );
    let query = "SELECT bucket, score, label FROM events
                 WHERE score >= 95
                 ORDER BY score DESC, label, bucket DESC;";

    let mut in_memory = configured_database(256, temporary_directory.path());
    in_memory.execute(&setup).expect("in-memory setup");
    let expected = last_query(in_memory.execute(query).expect("in-memory query"));

    let mut spilled = configured_database(2, temporary_directory.path());
    spilled.execute(&setup).expect("spilled setup");
    let actual = last_query(spilled.execute(query).expect("spilled query"));

    assert_eq!(actual, expected);
    temporary_directory.assert_empty();
}

#[test]
fn grouped_output_uses_the_same_external_ordering_path() {
    let temporary_directory = TestDirectory::new();
    let rows = (0..80)
        .map(|index| format!("('key-{}', {}, {})", index % 40, index % 3, index))
        .collect::<Vec<_>>()
        .join(",");
    let setup = format!(
        "CREATE TABLE grouped (key String, lane Int64, amount Int64);
         INSERT INTO grouped VALUES {rows};"
    );
    let query = "SELECT key, lane, COUNT(*) AS rows, SUM(amount) AS total
                 FROM grouped GROUP BY key, lane
                 ORDER BY total DESC, key, lane;";

    let mut in_memory = configured_database(256, temporary_directory.path());
    in_memory.execute(&setup).expect("in-memory setup");
    let expected = last_query(in_memory.execute(query).expect("in-memory query"));

    let mut spilled = configured_database(3, temporary_directory.path());
    spilled.execute(&setup).expect("spilled setup");
    let actual = last_query(spilled.execute(query).expect("spilled query"));

    assert_eq!(actual, expected);
    temporary_directory.assert_empty();
}

#[test]
fn small_top_k_does_not_create_a_spill_directory() {
    let temporary_directory = TestDirectory::new();
    let unused_parent = temporary_directory.path().join("unused");
    let mut database = configured_database(4, &unused_parent);
    database
        .execute(
            "CREATE TABLE scores (id Int64, score Int64);
             INSERT INTO scores VALUES (1, 3), (2, 9), (3, 9), (4, 1), (5, 8);",
        )
        .expect("setup succeeds");

    let result = last_query(
        database
            .execute("SELECT id, score FROM scores ORDER BY score DESC LIMIT 3;")
            .expect("top-k succeeds"),
    );

    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(2), Value::Int64(9)],
            vec![Value::Int64(3), Value::Int64(9)],
            vec![Value::Int64(5), Value::Int64(8)],
        ]
    );
    assert!(!unused_parent.exists());
}

#[test]
fn temporary_storage_errors_are_propagated_without_artifacts() {
    let temporary_directory = TestDirectory::new();
    let not_a_directory = temporary_directory.path().join("blocker");
    fs::write(&not_a_directory, b"unchanged").expect("create blocking file");
    let mut database = configured_database(2, &not_a_directory);
    database
        .execute(
            "CREATE TABLE valueset (value Int64);
             INSERT INTO valueset VALUES (4), (3), (2), (1);",
        )
        .expect("setup succeeds without sorting");

    let error = database
        .execute("SELECT value FROM valueset ORDER BY value;")
        .expect_err("spill directory creation fails");

    assert!(
        matches!(error, Error::TemporaryStorage(message) if message.contains("create temporary directory"))
    );
    assert_eq!(
        fs::read(&not_a_directory).expect("blocking file remains"),
        b"unchanged"
    );
    assert_eq!(
        fs::read_dir(temporary_directory.path())
            .expect("read test directory")
            .count(),
        1
    );
}
