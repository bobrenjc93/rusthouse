use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::{Database, Error, QueryResult, StatementResult, Value};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

struct CsvFile(PathBuf);

impl CsvFile {
    fn new(contents: impl AsRef<[u8]>) -> Self {
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rusthouse-copy-{}-{sequence}.csv",
            std::process::id()
        ));
        fs::write(&path, contents).expect("write CSV fixture");
        Self(path)
    }

    fn sql_path(&self) -> String {
        self.0
            .to_str()
            .expect("temporary path is UTF-8")
            .replace('\'', "''")
    }
}

impl Drop for CsvFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn copy(database: &mut Database, table: &str, file: &CsvFile, header: bool) -> StatementResult {
    let header = if header { " WITH HEADER" } else { "" };
    database
        .execute(&format!(
            "COPY {table} FROM '{}' FORMAT CSV{header}",
            file.sql_path()
        ))
        .expect("COPY succeeds")
        .pop()
        .expect("COPY result")
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .pop()
        .expect("query result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn copy_handles_headers_quoted_delimiters_newlines_and_escaped_quotes() {
    let csv = CsvFile::new(
        b"id,note,active\r\n\
          1,\"hello, world\",true\r\n\
          2,\"line one\r\nline two\",FALSE\r\n\
          3,\"He said \"\"hi\"\"\",true\r\n",
    );
    let mut database = Database::new();
    database
        .execute("CREATE TABLE notes (id Int64, note String, active Bool)")
        .expect("create table");

    assert_eq!(
        copy(&mut database, "notes", &csv, true),
        StatementResult::Command {
            tag: "COPY",
            affected_rows: 3,
        }
    );
    assert_eq!(
        query(&mut database, "SELECT * FROM notes ORDER BY id").rows,
        vec![
            vec![
                Value::Int64(1),
                Value::String("hello, world".to_owned()),
                Value::Bool(true),
            ],
            vec![
                Value::Int64(2),
                Value::String("line one\r\nline two".to_owned()),
                Value::Bool(false),
            ],
            vec![
                Value::Int64(3),
                Value::String("He said \"hi\"".to_owned()),
                Value::Bool(true),
            ],
        ]
    );
}

#[test]
fn copy_accepts_numeric_boundaries_and_rejects_overflow_atomically() {
    let valid = CsvFile::new(format!(
        "{},1.7976931348623157e308\n{},-1.7976931348623157e308\n",
        i64::MIN,
        i64::MAX
    ));
    let overflow = CsvFile::new("0,1\n9223372036854775808,2\n");
    let mut database = Database::new();
    database
        .execute("CREATE TABLE bounds (n Int64, f Float64)")
        .expect("create table");

    copy(&mut database, "bounds", &valid, false);
    let error = database
        .execute(&format!(
            "COPY bounds FROM '{}' FORMAT CSV",
            overflow.sql_path()
        ))
        .expect_err("overflow fails");
    assert!(matches!(
        error,
        Error::Csv {
            record: 2,
            field: Some(1),
            message,
            ..
        } if message.contains("valid Int64")
    ));

    assert_eq!(
        query(&mut database, "SELECT n FROM bounds ORDER BY n").rows,
        vec![vec![Value::Int64(i64::MIN)], vec![Value::Int64(i64::MAX)]]
    );
}

#[test]
fn corruption_after_a_flushed_batch_restores_original_column_lengths() {
    let mut contents = String::new();
    for id in 0..1_100 {
        contents.push_str(&format!("{id},row {id}\n"));
    }
    contents.push_str("corrupt,wrong type\n");
    let corrupt = CsvFile::new(contents);
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String);\
             INSERT INTO events VALUES (-1, 'existing')",
        )
        .expect("setup table");

    let error = database
        .execute(&format!(
            "COPY events FROM '{}' FORMAT CSV",
            corrupt.sql_path()
        ))
        .expect_err("mid-file corruption fails");
    assert!(matches!(error, Error::Csv { record: 1_101, .. }));
    assert_eq!(
        query(&mut database, "SELECT * FROM events").rows,
        vec![vec![Value::Int64(-1), Value::String("existing".to_owned())]]
    );
}

#[test]
fn copy_streams_large_inputs_across_multiple_batches() {
    let mut contents = String::new();
    for id in 0..5_000 {
        contents.push_str(&format!("{id},{}\n", id as f64 / 10.0));
    }
    let csv = CsvFile::new(contents);
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (id Int64, reading Float64)")
        .expect("create table");

    assert_eq!(
        copy(&mut database, "samples", &csv, false),
        StatementResult::Command {
            tag: "COPY",
            affected_rows: 5_000,
        }
    );
    assert_eq!(
        query(&mut database, "SELECT COUNT(*) AS rows FROM samples").rows,
        vec![vec![Value::Int64(5_000)]]
    );
}

#[test]
fn copy_bounds_fields_and_reports_io_failures_without_mutating_the_table() {
    let oversized = CsvFile::new(format!("1,{}\n", "x".repeat(1024 * 1024 + 1)));
    let missing = std::env::temp_dir().join(format!(
        "rusthouse-copy-missing-{}.csv",
        NEXT_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE bounded (id Int64, label String);\
             INSERT INTO bounded VALUES (0, 'existing')",
        )
        .expect("setup table");

    let error = database
        .execute(&format!(
            "COPY bounded FROM '{}' FORMAT CSV",
            oversized.sql_path()
        ))
        .expect_err("oversized field fails");
    assert!(matches!(
        error,
        Error::Csv {
            record: 1,
            field: Some(2),
            message,
            ..
        } if message.contains("field exceeds")
    ));

    let missing = sql_path(&missing);
    let error = database
        .execute(&format!("COPY bounded FROM '{missing}' FORMAT CSV"))
        .expect_err("missing file fails");
    assert!(matches!(error, Error::Io { .. }));
    assert_eq!(
        query(&mut database, "SELECT * FROM bounded").rows,
        vec![vec![Value::Int64(0), Value::String("existing".to_owned())]]
    );
}

fn sql_path(path: &Path) -> String {
    path.to_str()
        .expect("temporary path is UTF-8")
        .replace('\'', "''")
}
