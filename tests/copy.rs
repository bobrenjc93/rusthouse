use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::{Database, Error, StatementResult, Value};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

struct CsvFile(PathBuf);

impl CsvFile {
    fn new(contents: &[u8]) -> Self {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rusthouse-copy-{}-{id}.csv", std::process::id()));
        fs::write(&path, contents).expect("write temporary CSV");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CsvFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn copy_sql(table: &str, file: &CsvFile, header: bool) -> String {
    let escaped_path = file.path().to_string_lossy().replace('\'', "''");
    let header = if header { " HEADER" } else { "" };
    format!("COPY {table} FROM '{escaped_path}' FORMAT CSV{header}")
}

#[test]
fn copy_handles_reordered_headers_crlf_multiline_and_escaped_strings() {
    let file = CsvFile::new(
        b"note,id,score,active\r\n\"plain\",1,1.5,true\r\n\"line one\r\nline \"\"two\"\"\",2,-3.25,FALSE\r\n",
    );
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, note String, active Bool, score Float64)")
        .expect("create table");

    let results = database
        .execute(&copy_sql("events", &file, true))
        .expect("COPY succeeds");

    assert_eq!(
        results,
        vec![StatementResult::Command {
            tag: "COPY",
            affected_rows: 2,
        }]
    );
    let result = database
        .execute("SELECT id, note, active, score FROM events ORDER BY id")
        .expect("query succeeds")
        .pop()
        .expect("query result");
    let StatementResult::Query(result) = result else {
        panic!("expected query result");
    };
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::String("plain".to_owned()),
                Value::Bool(true),
                Value::Float64(1.5),
            ],
            vec![
                Value::Int64(2),
                Value::String("line one\r\nline \"two\"".to_owned()),
                Value::Bool(false),
                Value::Float64(-3.25),
            ],
        ]
    );
}

#[test]
fn copy_is_available_through_the_cli() {
    let file = CsvFile::new(b"id,label\n2,second\n1,first\n");
    let sql = format!(
        "CREATE TABLE events (id Int64, label String); {}; \
         SELECT id, label FROM events ORDER BY id",
        copy_sql("events", &file, true)
    );

    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format=csv", "--execute", &sql])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "id,label\n1,first\n2,second\n"
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("COPY 2"), "unexpected stderr: {stderr}");
}

#[test]
fn late_type_errors_report_the_csv_row_and_restore_existing_rows() {
    let mut contents = String::from("id,note\n");
    for id in 0..1_100 {
        contents.push_str(&format!("{id},valid\n"));
    }
    contents.push_str("not-an-integer,invalid\n");
    let file = CsvFile::new(contents.as_bytes());
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, note String); \
             INSERT INTO events VALUES (-1, 'existing')",
        )
        .expect("setup succeeds");

    let error = database
        .execute(&copy_sql("events", &file, true))
        .expect_err("invalid typed value");

    assert!(matches!(
        error,
        Error::Copy {
            row: Some(1_102),
            column: Some(column),
            message,
            ..
        } if column == "id"
            && message.contains("invalid Int64")
            && message.contains("not-an-integer")
    ));
    assert_eq!(
        database
            .catalog()
            .table("events")
            .expect("table remains")
            .row_count(),
        1
    );
}

#[test]
fn malformed_and_oversized_records_roll_back_completed_batches() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, note String); \
             INSERT INTO events VALUES (-1, 'existing')",
        )
        .expect("setup succeeds");

    let mut malformed = String::new();
    for id in 0..1_100 {
        malformed.push_str(&format!("{id},valid\n"));
    }
    malformed.push_str("1100,\"unterminated");
    let malformed = CsvFile::new(malformed.as_bytes());
    let parse_error = database
        .execute(&copy_sql("events", &malformed, false))
        .expect_err("malformed quote");
    assert!(matches!(
        parse_error,
        Error::Copy {
            row: Some(1_101),
            message,
            ..
        } if message.contains("unterminated quoted field")
    ));
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 1);

    let mut oversized = String::new();
    for id in 0..1_024 {
        oversized.push_str(&format!("{id},valid\n"));
    }
    oversized.push_str("1024,");
    oversized.push_str(&"x".repeat(1024 * 1024 + 1));
    oversized.push('\n');
    let oversized = CsvFile::new(oversized.as_bytes());
    let limit_error = database
        .execute(&copy_sql("events", &oversized, false))
        .expect_err("field limit");
    assert!(matches!(
        limit_error,
        Error::Copy {
            row: Some(1_025),
            message,
            ..
        } if message.contains("1048576-byte limit")
    ));
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 1);
}
