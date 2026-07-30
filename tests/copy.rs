use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusthouse::{Database, Error, QueryResult, StatementResult, Value};

static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

struct TempCsv {
    path: PathBuf,
}

impl TempCsv {
    fn create(name: &str, write: impl FnOnce(&mut BufWriter<File>)) -> Self {
        let unique = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rusthouse-{name}-{}-{unique}.csv",
            std::process::id()
        ));
        let file = File::create(&path).expect("create temporary CSV");
        let mut writer = BufWriter::new(file);
        write(&mut writer);
        writer.flush().expect("flush temporary CSV");
        Self { path }
    }

    fn sql_path(&self) -> String {
        self.path
            .to_str()
            .expect("temporary path is UTF-8")
            .replace('\'', "''")
    }
}

impl Drop for TempCsv {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn last_query(results: Vec<StatementResult>) -> QueryResult {
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn row_count(database: &mut Database, table: &str) -> i64 {
    let result = last_query(
        database
            .execute(&format!("SELECT COUNT(*) AS rows FROM {table}"))
            .expect("count query succeeds"),
    );
    let Value::Int64(count) = result.rows[0][0] else {
        panic!("COUNT returns Int64");
    };
    count
}

#[test]
fn copy_ingests_headers_quoted_fields_and_exact_types_in_column_order() {
    let csv = TempCsv::create("typed", |writer| {
        writer
            .write_all(
                b"label,id,active,score\r\n\
                  \"hello, \"\"CSV\"\"\",9223372036854775807,TRUE,1.25e2\r\n\
                  \"line 1\r\nline 2\",-9223372036854775808,false,-0.5\r\n",
            )
            .expect("write CSV");
    });
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, score Float64, active Bool, label String)")
        .expect("create table");

    let results = database
        .execute(&format!(
            "COPY events (label, id, active, score) FROM '{}' FORMAT CSV;
             SELECT id, score, active, label FROM events ORDER BY id DESC",
            csv.sql_path()
        ))
        .expect("COPY and SELECT succeed");

    assert!(matches!(
        &results[0],
        StatementResult::Command {
            tag: "COPY",
            affected_rows: 2
        }
    ));
    assert_eq!(
        last_query(results).rows,
        vec![
            vec![
                Value::Int64(i64::MAX),
                Value::Float64(125.0),
                Value::Bool(true),
                Value::String("hello, \"CSV\"".to_owned()),
            ],
            vec![
                Value::Int64(i64::MIN),
                Value::Float64(-0.5),
                Value::Bool(false),
                Value::String("line 1\r\nline 2".to_owned()),
            ],
        ]
    );
}

#[test]
fn copy_rejects_inexact_types_without_appending_the_current_batch() {
    let csv = TempCsv::create("wrong-type", |writer| {
        writer
            .write_all(b"id,active,score\n1.0,true,2.5\n")
            .expect("write CSV");
    });
    let mut database = Database::new();
    database
        .execute("CREATE TABLE typed (id Int64, active Bool, score Float64)")
        .expect("create table");

    let error = database
        .execute(&format!("COPY typed FROM '{}' FORMAT CSV", csv.sql_path()))
        .expect_err("a decimal is not an Int64");
    assert!(matches!(
        error,
        Error::Copy {
            record: Some(2),
            message,
            ..
        } if message.contains("column 'id'") && message.contains("valid Int64")
    ));
    assert_eq!(row_count(&mut database, "typed"), 0);
}

#[test]
fn malformed_csv_preserves_prior_complete_batches_only() {
    const BATCH_SIZE: usize = 1_024;
    let csv = TempCsv::create("partial", |writer| {
        writeln!(writer, "id,label").expect("write header");
        for id in 0..BATCH_SIZE {
            writeln!(writer, "{id},valid-{id}").expect("write valid row");
        }
        writer
            .write_all(b"1024,\"unterminated")
            .expect("write malformed row");
    });
    let mut database = Database::new();
    database
        .execute("CREATE TABLE imported (id Int64, label String)")
        .expect("create table");

    let error = database
        .execute(&format!(
            "COPY imported FROM '{}' FORMAT CSV",
            csv.sql_path()
        ))
        .expect_err("malformed CSV is rejected");
    assert!(matches!(
        error,
        Error::Copy {
            record: Some(1026),
            message,
            ..
        } if message.contains("unterminated quoted field")
    ));
    assert_eq!(row_count(&mut database, "imported"), BATCH_SIZE as i64);
}

#[test]
fn large_copy_streams_through_many_fixed_size_batches() {
    const ROWS: usize = 20_000;
    let csv = TempCsv::create("large", |writer| {
        writeln!(writer, "id,category,amount,active").expect("write header");
        for id in 0..ROWS {
            writeln!(
                writer,
                "{id},\"group,{}\",{},{}",
                id % 10,
                id * 2,
                id % 2 == 0
            )
            .expect("write data row");
        }
    });
    let mut database = Database::new();
    database
        .execute("CREATE TABLE facts (id Int64, category String, amount Int64, active Bool)")
        .expect("create table");

    let result = database
        .execute(&format!("COPY facts FROM '{}' FORMAT CSV", csv.sql_path()))
        .expect("large COPY succeeds");
    assert!(matches!(
        &result[0],
        StatementResult::Command {
            tag: "COPY",
            affected_rows: ROWS
        }
    ));

    let aggregate = last_query(
        database
            .execute(
                "SELECT COUNT(*) AS rows, SUM(amount) AS total
                 FROM facts WHERE active = true",
            )
            .expect("aggregate succeeds"),
    );
    assert_eq!(
        aggregate.rows,
        vec![vec![
            Value::Int64((ROWS / 2) as i64),
            Value::Int64(199_980_000),
        ]]
    );
}

#[test]
fn copy_requires_matching_headers_and_a_complete_column_list() {
    let csv = TempCsv::create("header", |writer| {
        writer.write_all(b"other\n1\n").expect("write CSV");
    });
    let mut database = Database::new();
    database
        .execute("CREATE TABLE headers (id Int64, label String)")
        .expect("create table");

    let incomplete = database
        .execute(&format!(
            "COPY headers (id) FROM '{}' FORMAT CSV",
            csv.sql_path()
        ))
        .expect_err("missing columns have no default");
    assert!(
        matches!(incomplete, Error::InvalidQuery(message) if message.contains("must name all 2 columns"))
    );

    let mismatch = database
        .execute(&format!(
            "COPY headers FROM '{}' FORMAT CSV",
            csv.sql_path()
        ))
        .expect_err("header does not match table");
    assert!(matches!(
        mismatch,
        Error::Copy {
            record: Some(1),
            ..
        }
    ));
}

#[test]
fn missing_copy_file_reports_the_path() {
    let path = Path::new("/definitely/not/a/rusthouse-copy-file.csv");
    let mut database = Database::new();
    database
        .execute("CREATE TABLE missing_file (id Int64)")
        .expect("create table");

    let error = database
        .execute(&format!(
            "COPY missing_file FROM '{}' FORMAT CSV",
            path.display()
        ))
        .expect_err("missing file");
    assert!(matches!(
        error,
        Error::Copy {
            path: failed_path,
            record: None,
            ..
        } if failed_path == path.to_string_lossy()
    ));
}
