use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use parquet::basic::{LogicalType, Type as PhysicalType};
use parquet::column::reader::get_typed_column_reader;
use parquet::data_type::{BoolType, ByteArrayType, DoubleType, Int64Type};
use parquet::file::reader::{FileReader, SerializedFileReader};

fn run_parquet(sql: &str, output: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--format=parquet")
        .arg("--output")
        .arg(output)
        .arg("--execute")
        .arg(sql)
        .output()
        .expect("run Parquet CLI")
}

fn parquet_reader(path: &Path) -> SerializedFileReader<File> {
    SerializedFileReader::new(File::open(path).expect("open Parquet output"))
        .expect("read Parquet output")
}

fn read_int64_column(path: &Path, index: usize) -> Vec<i64> {
    let reader = parquet_reader(path);
    let row_group = reader.get_row_group(0).expect("read row group");
    let mut column =
        get_typed_column_reader::<Int64Type>(row_group.get_column_reader(index).expect("column"));
    let mut values = Vec::new();
    column
        .read_records(usize::MAX, None, None, &mut values)
        .expect("read Int64 values");
    values
}

fn read_float64_column(path: &Path, index: usize) -> Vec<f64> {
    let reader = parquet_reader(path);
    let row_group = reader.get_row_group(0).expect("read row group");
    let mut column =
        get_typed_column_reader::<DoubleType>(row_group.get_column_reader(index).expect("column"));
    let mut values = Vec::new();
    column
        .read_records(usize::MAX, None, None, &mut values)
        .expect("read Float64 values");
    values
}

fn read_bool_column(path: &Path, index: usize) -> Vec<bool> {
    let reader = parquet_reader(path);
    let row_group = reader.get_row_group(0).expect("read row group");
    let mut column =
        get_typed_column_reader::<BoolType>(row_group.get_column_reader(index).expect("column"));
    let mut values = Vec::new();
    column
        .read_records(usize::MAX, None, None, &mut values)
        .expect("read Bool values");
    values
}

fn read_string_column(path: &Path, index: usize) -> Vec<String> {
    let reader = parquet_reader(path);
    let row_group = reader.get_row_group(0).expect("read row group");
    let mut column = get_typed_column_reader::<ByteArrayType>(
        row_group.get_column_reader(index).expect("column"),
    );
    let mut values = Vec::new();
    column
        .read_records(usize::MAX, None, None, &mut values)
        .expect("read String values");
    values
        .iter()
        .map(|value| value.as_utf8().expect("UTF-8 string").to_owned())
        .collect()
}

#[test]
fn execute_argument_emits_clean_json_and_command_statuses() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE items (name String, n Int64);
             INSERT INTO items VALUES ('b', 2), ('a', 1);
             SELECT name, n FROM items ORDER BY n;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"name\",\"type\":\"String\"},{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[\"a\",1],[\"b\",2]]}]}\n"
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("CREATE TABLE"));
    assert!(stderr.contains("INSERT 2"));
}

#[test]
fn multiple_selects_emit_one_json_document() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE numbers (n Int64);
             INSERT INTO numbers VALUES (1), (2);
             SELECT n FROM numbers WHERE n = 1;
             SELECT n FROM numbers WHERE n = 2;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]},{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[2]]}]}\n"
    );
}

#[test]
fn positional_json_preserves_duplicate_alias_values() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE items (id Int64, label String);
             INSERT INTO items VALUES (1, 'one');
             SELECT id, label AS id FROM items;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"id\",\"type\":\"String\"}],\"rows\":[[1,\"one\"]]}]}\n"
    );
}

#[test]
fn stdin_and_csv_output_work_together() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");

    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(
            b"CREATE TABLE notes (label String, active Bool);
              INSERT INTO notes VALUES ('hello, world', true);
              SELECT * FROM notes;",
        )
        .expect("write SQL");

    let output = child.wait_with_output().expect("wait for CLI");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "label,active\n\"hello, world\",true\n"
    );
}

#[test]
fn parquet_round_trip_preserves_all_physical_types() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("typed.parquet");
    std::fs::write(&path, b"previous contents").expect("seed existing destination");

    let output = run_parquet(
        "CREATE TABLE values (id Int64, ratio Float64, active Bool, label String);
         INSERT INTO values VALUES (7, 3.5, true, 'sample'), (-2, -0.25, false, '');
         SELECT * FROM values ORDER BY id;",
        &path,
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    let reader = parquet_reader(&path);
    let schema = reader.metadata().file_metadata().schema_descr();
    let columns = schema.columns();
    assert_eq!(columns.len(), 4);
    assert_eq!(columns[0].name(), "id");
    assert_eq!(columns[0].physical_type(), PhysicalType::INT64);
    assert_eq!(columns[1].physical_type(), PhysicalType::DOUBLE);
    assert_eq!(columns[2].physical_type(), PhysicalType::BOOLEAN);
    assert_eq!(columns[3].physical_type(), PhysicalType::BYTE_ARRAY);
    assert_eq!(columns[3].logical_type_ref(), Some(&LogicalType::String));
    assert_eq!(reader.metadata().file_metadata().num_rows(), 2);
    drop(reader);

    assert_eq!(read_int64_column(&path, 0), vec![-2, 7]);
    assert_eq!(read_float64_column(&path, 1), vec![-0.25, 3.5]);
    assert_eq!(read_bool_column(&path, 2), vec![false, true]);
    assert_eq!(read_string_column(&path, 3), vec!["", "sample"]);
}

#[test]
fn parquet_empty_result_keeps_typed_schema() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("empty.parquet");

    let output = run_parquet(
        "CREATE TABLE events (id Int64, name String); SELECT * FROM events;",
        &path,
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    let reader = parquet_reader(&path);
    assert_eq!(reader.metadata().file_metadata().num_rows(), 0);
    assert_eq!(reader.metadata().num_row_groups(), 1);
    let columns = reader.metadata().file_metadata().schema_descr().columns();
    assert_eq!(columns[0].physical_type(), PhysicalType::INT64);
    assert_eq!(columns[1].logical_type_ref(), Some(&LogicalType::String));
    drop(reader);
    assert!(read_int64_column(&path, 0).is_empty());
    assert!(read_string_column(&path, 1).is_empty());
}

#[test]
fn parquet_round_trip_preserves_unicode_strings() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("unicode.parquet");

    let output = run_parquet(
        "CREATE TABLE labels (value String);
         INSERT INTO labels VALUES ('東京 café'), ('Здравствуйте'), ('emoji 🚀');
         SELECT * FROM labels;",
        &path,
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        read_string_column(&path, 0),
        vec!["東京 café", "Здравствуйте", "emoji 🚀"]
    );
}

#[test]
fn parquet_round_trip_preserves_numeric_boundaries() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("boundaries.parquet");

    let output = run_parquet(
        "CREATE TABLE boundaries (integer Int64, float Float64);
         INSERT INTO boundaries VALUES
           (-9223372036854775808, -1.7976931348623157e308),
           (9223372036854775807, 1.7976931348623157e308),
           (0, 5e-324);
         SELECT * FROM boundaries;",
        &path,
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(read_int64_column(&path, 0), vec![i64::MIN, i64::MAX, 0]);
    assert_eq!(
        read_float64_column(&path, 1),
        vec![-f64::MAX, f64::MAX, f64::from_bits(1)]
    );
}

#[test]
fn parquet_rejects_multiple_selects_without_touching_output() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("result.parquet");
    std::fs::write(&path, b"keep me").expect("seed destination");

    let output = run_parquet(
        "CREATE TABLE values (id Int64); SELECT * FROM values; SELECT * FROM values;",
        &path,
    );

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("UTF-8 stderr")
            .contains("requires exactly one SELECT")
    );
    assert_eq!(std::fs::read(&path).expect("read destination"), b"keep me");
}

#[test]
fn parquet_rejects_batches_without_a_select() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("result.parquet");

    let output = run_parquet("CREATE TABLE values (id Int64);", &path);

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("UTF-8 stderr")
            .contains("requires exactly one SELECT")
    );
    assert!(!path.exists());
}

#[test]
fn parquet_rename_failure_removes_temporary_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("destination");
    std::fs::create_dir(&path).expect("create destination directory");

    let output = run_parquet(
        "CREATE TABLE values (id Int64); INSERT INTO values VALUES (1); SELECT * FROM values;",
        &path,
    );

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("UTF-8 stderr")
            .contains("could not atomically replace Parquet output")
    );
    let entries = directory
        .path()
        .read_dir()
        .expect("read temporary directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![path.file_name().expect("destination name")]);
}

#[test]
fn sql_errors_are_reported_with_nonzero_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--execute",
            "CREATE TABLE t (id Int64); INSERT INTO t VALUES ('wrong');",
        ])
        .output()
        .expect("run CLI");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("type mismatch for column 't.id'"));
    assert!(stderr.contains("expected Int64, found String"));
}

#[test]
fn excessive_predicates_return_cli_errors_without_aborting() {
    let cases = [
        (
            format!(
                "SELECT id FROM things WHERE {}id = 1{}",
                "(".repeat(50_000),
                ")".repeat(50_000)
            ),
            "predicate nesting exceeds limit of 64",
        ),
        (
            format!(
                "SELECT id FROM things WHERE {}",
                vec!["id = 1"; 50_000].join(" OR ")
            ),
            "predicate is too complex; maximum 256 expression nodes",
        ),
    ];

    for (sql, expected_error) in cases {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn CLI");
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(sql.as_bytes())
            .expect("write large SQL query");

        let output = child.wait_with_output().expect("wait for CLI");
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            stderr.contains(expected_error),
            "unexpected stderr: {stderr}"
        );
        assert!(!stderr.contains("stack overflow"));
    }
}
