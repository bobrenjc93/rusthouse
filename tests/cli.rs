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
        "id,name\n1,\"a,b\"\n2,two\nn\n2\n"
    );
}

#[test]
fn csv_cli_matches_the_benchmark_header_contract() {
    let output = run_cli(
        &["--format", "csv"],
        "CREATE TABLE benchmark_header (x Int64); \
         INSERT INTO benchmark_header VALUES (1); \
         SELECT count(*) AS row_count FROM benchmark_header;",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "row_count\n1\n");
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

#[test]
fn deeply_nested_input_is_rejected_without_aborting() {
    let depth = 20_000;
    let sql = format!(
        "SELECT {}1{} FROM missing",
        "(".repeat(depth),
        ")".repeat(depth)
    );
    let output = run_cli(&[], &sql);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("maximum depth"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn repeated_large_string_projections_respect_the_result_byte_limit() {
    let value = "x".repeat(1024 * 1024);
    let projections = std::iter::repeat_n("payload", 65)
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "CREATE TABLE large (payload String); \
         INSERT INTO large VALUES ('{value}'); \
         SELECT {projections} FROM large;"
    );
    let output = run_cli(&[], &sql);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("result byte limit"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn json_accounts_for_repeated_large_aliases_before_rendering() {
    let alias = "a".repeat(1024 * 1024);
    let rows = (0..65)
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>();
    let sql = format!(
        "CREATE TABLE aliases (x Int64); \
         INSERT INTO aliases VALUES {}; \
         SELECT x AS \"{alias}\" FROM aliases;",
        rows.join(",")
    );
    let output = run_cli(&["--format", "json"], &sql);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("encoded output limit"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn json_rejects_duplicate_projected_names() {
    let output = run_cli(
        &["--format", "json"],
        "CREATE TABLE duplicate_names (x Int64); \
         INSERT INTO duplicate_names VALUES (1); \
         SELECT x, x + 1 AS x FROM duplicate_names;",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("duplicate 'x'"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn repeated_selects_share_a_cumulative_result_budget() {
    let value = "x".repeat(1024 * 1024);
    let selects = std::iter::repeat_n("SELECT payload FROM retained;", 70)
        .collect::<Vec<_>>()
        .join("");
    let sql = format!(
        "CREATE TABLE retained (payload String); \
         INSERT INTO retained VALUES ('{value}'); \
         {selects}"
    );
    let output = run_cli(&[], &sql);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cumulative result byte limit"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
