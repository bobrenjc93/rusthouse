use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parquet::basic::Compression;
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int32Type, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use rusthouse::{Database, Error, QueryResult, StatementResult, Value};

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(name: &str) -> Self {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rusthouse-{name}-{}-{id}.parquet",
            std::process::id()
        ));
        Self { path }
    }

    fn sql_path(&self) -> String {
        self.path.to_string_lossy().replace('\'', "''")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn parquet_writer(
    path: &Path,
    schema: &str,
    compression: Compression,
) -> SerializedFileWriter<File> {
    let schema = Arc::new(parse_message_type(schema).expect("valid test schema"));
    let properties = Arc::new(
        WriterProperties::builder()
            .set_compression(compression)
            .build(),
    );
    SerializedFileWriter::new(
        File::create(path).expect("create fixture"),
        schema,
        properties,
    )
    .expect("create Parquet writer")
}

fn last_query(results: Vec<StatementResult>) -> QueryResult {
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn generated_parquet_imports_supported_types_and_reorders_columns() {
    let fixture = TempFile::new("all-types");
    let mut writer = parquet_writer(
        &fixture.path,
        "message dataset {
            required BYTE_ARRAY source_label (UTF8);
            optional INT64 source_id;
            required DOUBLE source_score;
            required BOOLEAN source_active;
        }",
        Compression::SNAPPY,
    );
    let mut row_group = writer.next_row_group().expect("row group");

    let labels = ["beta", "alpha", "gamma"].map(|value| ByteArray::from(value.as_bytes()));
    let mut column = row_group.next_column().expect("column").expect("label");
    column
        .typed::<ByteArrayType>()
        .write_batch(&labels, None, None)
        .expect("write labels");
    column.close().expect("close labels");

    let mut column = row_group.next_column().expect("column").expect("id");
    column
        .typed::<Int64Type>()
        .write_batch(&[2, 1, 3], Some(&[1, 1, 1]), None)
        .expect("write ids");
    column.close().expect("close ids");

    let mut column = row_group.next_column().expect("column").expect("score");
    column
        .typed::<DoubleType>()
        .write_batch(&[2.5, 1.25, 9.0], None, None)
        .expect("write scores");
    column.close().expect("close scores");

    let mut column = row_group.next_column().expect("column").expect("active");
    column
        .typed::<BoolType>()
        .write_batch(&[false, true, true], None, None)
        .expect("write active flags");
    column.close().expect("close active flags");
    row_group.close().expect("close row group");
    writer.close().expect("close file");

    let mut database = Database::new();
    let results = database
        .execute(&format!(
            "CREATE TABLE events (id Int64, label String, score Float64, active Bool);
             COPY events (label, id, score, active) FROM '{}' FORMAT PARQUET;",
            fixture.sql_path()
        ))
        .expect("COPY succeeds");
    assert!(matches!(
        results[1],
        StatementResult::Command {
            tag: "COPY",
            affected_rows: 3
        }
    ));

    let result = last_query(
        database
            .execute("SELECT id, label, score, active FROM events ORDER BY id")
            .expect("query imported rows"),
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::String("alpha".to_owned()),
                Value::Float64(1.25),
                Value::Bool(true),
            ],
            vec![
                Value::Int64(2),
                Value::String("beta".to_owned()),
                Value::Float64(2.5),
                Value::Bool(false),
            ],
            vec![
                Value::Int64(3),
                Value::String("gamma".to_owned()),
                Value::Float64(9.0),
                Value::Bool(true),
            ],
        ]
    );

    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            &format!(
                "CREATE TABLE events (id Int64, label String, score Float64, active Bool);
                 COPY events (label, id, score, active) FROM '{}' FORMAT PARQUET;
                 SELECT id, label FROM events ORDER BY id LIMIT 1;",
                fixture.sql_path()
            ),
        ])
        .output()
        .expect("run CLI COPY");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[1,\"alpha\"]]}]}\n"
    );
    assert!(
        String::from_utf8(output.stderr)
            .expect("UTF-8 stderr")
            .contains("COPY 3")
    );
}

#[test]
fn malformed_parquet_is_rejected_without_mutating_the_table() {
    let fixture = TempFile::new("malformed");
    fs::write(&fixture.path, b"not a parquet file").expect("write malformed fixture");

    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64)")
        .expect("create table");
    let error = database
        .execute(&format!(
            "COPY events FROM '{}' FORMAT PARQUET",
            fixture.sql_path()
        ))
        .expect_err("malformed file must fail");

    assert!(matches!(error, Error::Copy { message, .. } if message.contains("metadata")));
    assert_eq!(
        database
            .catalog()
            .table("events")
            .expect("table remains")
            .row_count(),
        0
    );
}

#[test]
fn schema_mismatch_is_rejected_before_any_rows_are_appended() {
    let fixture = TempFile::new("schema-mismatch");
    let mut writer = parquet_writer(
        &fixture.path,
        "message dataset { required INT32 id; }",
        Compression::UNCOMPRESSED,
    );
    let mut row_group = writer.next_row_group().expect("row group");
    let mut column = row_group.next_column().expect("column").expect("id");
    column
        .typed::<Int32Type>()
        .write_batch(&[1, 2], None, None)
        .expect("write ids");
    column.close().expect("close id");
    row_group.close().expect("close row group");
    writer.close().expect("close file");

    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64)")
        .expect("create table");
    let error = database
        .execute(&format!(
            "COPY events FROM '{}' FORMAT PARQUET",
            fixture.sql_path()
        ))
        .expect_err("INT32 must not be widened implicitly");

    assert!(
        matches!(error, Error::Copy { message, .. } if message.contains("INT32") && message.contains("requires Int64"))
    );
    assert_eq!(
        database
            .catalog()
            .table("events")
            .expect("table remains")
            .row_count(),
        0
    );
}

#[test]
fn later_row_group_failure_preserves_completed_row_groups() {
    let fixture = TempFile::new("partial-failure");
    let mut writer = parquet_writer(
        &fixture.path,
        "message dataset { optional INT64 id; }",
        Compression::UNCOMPRESSED,
    );

    let mut row_group = writer.next_row_group().expect("first row group");
    let mut column = row_group.next_column().expect("column").expect("id");
    column
        .typed::<Int64Type>()
        .write_batch(&[1, 2, 3], Some(&[1, 1, 1]), None)
        .expect("write first row group");
    column.close().expect("close id");
    row_group.close().expect("close first row group");

    let mut row_group = writer.next_row_group().expect("second row group");
    let mut column = row_group.next_column().expect("column").expect("id");
    column
        .typed::<Int64Type>()
        .write_batch(&[4, 6], Some(&[1, 0, 1]), None)
        .expect("write nullable row group");
    column.close().expect("close id");
    row_group.close().expect("close second row group");
    writer.close().expect("close file");

    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64)")
        .expect("create table");
    let error = database
        .execute(&format!(
            "COPY events FROM '{}' FORMAT PARQUET",
            fixture.sql_path()
        ))
        .expect_err("NULL cannot enter non-nullable table");
    assert!(matches!(error, Error::Copy { message, .. } if message.contains("NULL at row 5")));

    let result = last_query(
        database
            .execute("SELECT id FROM events ORDER BY id")
            .expect("query retained rows"),
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)]
        ]
    );
}

#[test]
fn large_parquet_import_streams_across_row_groups_and_batches() {
    const ROWS: i64 = 10_000;
    const ROW_GROUP_ROWS: i64 = 1_500;

    let fixture = TempFile::new("large");
    let mut writer = parquet_writer(
        &fixture.path,
        "message dataset { required INT64 id; }",
        Compression::ZSTD(Default::default()),
    );
    for start in (0..ROWS).step_by(ROW_GROUP_ROWS as usize) {
        let end = (start + ROW_GROUP_ROWS).min(ROWS);
        let values = (start..end).collect::<Vec<_>>();
        let mut row_group = writer.next_row_group().expect("row group");
        let mut column = row_group.next_column().expect("column").expect("id");
        column
            .typed::<Int64Type>()
            .write_batch(&values, None, None)
            .expect("write row group");
        column.close().expect("close id");
        row_group.close().expect("close row group");
    }
    writer.close().expect("close file");

    let mut database = Database::new();
    let result = last_query(
        database
            .execute(&format!(
                "CREATE TABLE events (id Int64);
                 COPY events FROM '{}' FORMAT PARQUET;
                 SELECT COUNT(*) AS rows, SUM(id) AS total FROM events;",
                fixture.sql_path()
            ))
            .expect("large COPY succeeds"),
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Int64(ROWS),
            Value::Int64((ROWS - 1) * ROWS / 2)
        ]]
    );
}
