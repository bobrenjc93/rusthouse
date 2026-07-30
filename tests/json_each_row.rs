use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::format::{self, OutputFormat};
use rusthouse::{Database, QueryResult, StatementResult, Value};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

struct TestFile(PathBuf);

impl TestFile {
    fn new(contents: &str) -> Self {
        let path = unique_path();
        fs::write(&path, contents).expect("write test data");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn unique_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusthouse-json-each-row-{}-{}.jsonl",
        std::process::id(),
        NEXT_FILE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn copy_sql(table: &str, path: &Path) -> String {
    let path = path
        .to_str()
        .expect("temporary path is UTF-8")
        .replace('\'', "''");
    format!("COPY {table} FROM '{path}' FORMAT JSONEachRow")
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn copy_maps_reordered_fields_decodes_escapes_and_applies_defaults() {
    let file = TestFile::new(concat!(
        "{\"note\":\"quote: \\\" slash: \\\\ newline: \\n unicode: \\u2603\",\"id\":2}\n",
        "{\"active\":true,\"score\":1.25,\"id\":1,\"note\":\"first\"}\r\n",
    ));
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, note String, score Float64, active Bool)")
        .expect("create table");

    let results = database
        .execute(&copy_sql("events", file.path()))
        .expect("COPY succeeds");
    assert_eq!(
        results,
        vec![StatementResult::Command {
            tag: "COPY",
            affected_rows: 2,
        }]
    );

    let result = query(
        &mut database,
        "SELECT id, note, score, active FROM events ORDER BY id",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::String("first".to_owned()),
                Value::Float64(1.25),
                Value::Bool(true),
            ],
            vec![
                Value::Int64(2),
                Value::String("quote: \" slash: \\ newline: \n unicode: ☃".to_owned()),
                Value::Float64(0.0),
                Value::Bool(false),
            ],
        ]
    );
}

#[test]
fn any_bad_record_rolls_back_the_complete_copy() {
    let cases = [
        (r#"{"id":2,"id":3}"#, "duplicate field"),
        (r#"{"id":2,"extra":3}"#, "unknown field"),
        (r#"{"id":null}"#, "not nullable"),
        (r#"{"id":"wrong"}"#, "wrong JSON type"),
        (r#"{"id":2"#, "expected ',' or '}'"),
    ];

    for (bad_record, expected) in cases {
        let file = TestFile::new(&format!("{{\"id\":2}}\n{bad_record}\n"));
        let mut database = Database::new();
        database
            .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1)")
            .expect("setup table");

        let error = database
            .execute(&copy_sql("events", file.path()))
            .expect_err("COPY should fail");
        let message = error.to_string();
        assert!(message.contains("record 2"), "unexpected error: {message}");
        assert!(message.contains(expected), "unexpected error: {message}");
        assert_eq!(
            query(&mut database, "SELECT id FROM events").rows,
            vec![vec![Value::Int64(1)]],
            "valid prefix must not be committed"
        );
    }
}

#[test]
fn large_file_is_imported_incrementally() {
    const ROWS: usize = 50_000;
    let path = unique_path();
    let file = TestFile(path.clone());
    let mut writer = BufWriter::new(File::create(&path).expect("create large input"));
    for id in 0..ROWS {
        writeln!(writer, "{{\"id\":{id},\"even\":{}}}", id % 2 == 0).expect("write record");
    }
    writer.flush().expect("flush records");

    let mut database = Database::new();
    database
        .execute("CREATE TABLE numbers (id Int64, even Bool)")
        .expect("create table");
    database
        .execute(&copy_sql("numbers", file.path()))
        .expect("COPY succeeds");

    let result = query(
        &mut database,
        "SELECT COUNT(*) AS rows, SUM(id) AS total FROM numbers",
    );
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(ROWS as i64), Value::Int64(1_249_975_000),]]
    );
}

#[test]
fn json_each_row_output_round_trips_through_copy() {
    let mut source = Database::new();
    source
        .execute(
            "CREATE TABLE source (id Int64, score Float64, active Bool, note String);
             INSERT INTO source VALUES
                (1, 2.5, true, 'line
quote \\ slash'),
                (2, 0.0, false, 'snowman ☃');",
        )
        .expect("create source rows");
    let source_result = query(
        &mut source,
        "SELECT id, score, active, note FROM source ORDER BY id",
    );

    let mut output = Vec::new();
    format::write(&source_result, OutputFormat::JsonEachRow, &mut output)
        .expect("stream JSONEachRow");
    let file = TestFile::new(std::str::from_utf8(&output).expect("JSON output is UTF-8"));

    let mut target = Database::new();
    target
        .execute("CREATE TABLE target (id Int64, score Float64, active Bool, note String)")
        .expect("create target");
    target
        .execute(&copy_sql("target", file.path()))
        .expect("COPY output back in");
    let target_result = query(
        &mut target,
        "SELECT id, score, active, note FROM target ORDER BY id",
    );

    assert_eq!(target_result, source_result);
}
