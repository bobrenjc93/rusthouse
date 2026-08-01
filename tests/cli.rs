use std::io::Write;
use std::process::{Command, Stdio};

fn run_cli(arguments: &[&str], input: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn help_does_not_wait_for_standard_input() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--format <FORMAT>"));
    assert!(stdout.contains("standard input"));
}

#[test]
fn csv_cli_executes_repeated_statements_in_one_process() {
    let output = run_cli(
        &["--format", "csv"],
        "CREATE TABLE t (id Int64, name String);\
         INSERT INTO t VALUES (1, 'a,b'), (2, 'two');\
         SELECT id, name FROM t ORDER BY id;\
         SELECT count(*) AS n FROM t;",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "1,\"a,b\"\n2,two\n2\n"
    );
}

#[test]
fn json_cli_preserves_types_and_nulls() {
    let output = run_cli(
        &["--format=json"],
        "CREATE TABLE t (id Int64, value Nullable(Float64), ok Bool);\
         INSERT INTO t VALUES (1, NULL, true);\
         SELECT * FROM t;",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "[{\"id\":1,\"value\":null,\"ok\":true}]\n"
    );
}

#[test]
fn cli_reports_bad_arguments_and_sql_on_stderr() {
    let bad_format = run_cli(&["--format", "xml"], "");
    assert!(!bad_format.status.success());
    assert!(String::from_utf8_lossy(&bad_format.stderr).contains("unsupported format"));

    let bad_sql = run_cli(&[], "SELECT FROM");
    assert!(!bad_sql.status.success());
    assert!(String::from_utf8_lossy(&bad_sql.stderr).contains("SQL error"));
}
