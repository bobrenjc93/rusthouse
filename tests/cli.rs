use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(arguments: &[&str], stdin: &str) -> Output {
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
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn help_describes_the_csv_interface() {
    let output = run(&["--help"], "");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: rusthouse --format csv"));
    assert!(stdout.contains("--format <FORMAT>"));
    assert!(output.stderr.is_empty());
}

#[test]
fn executes_every_statement_received_before_eof() {
    let output = run(
        &["--format", "csv"],
        "SELECT 7 AS first;\n\
         CREATE TABLE readings (captured_at Int64, value Float64);\n\
         SELECT -2.5 AS second;\n\
         SELECT TRUE;\n",
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "first\n7\nsecond\n-2.5\nTRUE\ntrue\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn create_table_without_select_produces_no_csv() {
    let output = run(
        &["--format", "csv"],
        "CREATE TABLE events (id Int64, active Bool, label String);",
    );

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn evaluates_equality_truth_tables_and_renders_null() {
    let output = run(
        &["--format", "csv"],
        "SELECT 4 = 4 AS integer_equal;\n\
         SELECT 4 <> 4 AS integer_not_equal;\n\
         SELECT 1.5 = 2.5 AS float_equal;\n\
         SELECT TRUE <> FALSE AS boolean_not_equal;\n\
         SELECT 'x' = 'x' AS string_equal;\n\
         SELECT NULL AS null_literal;\n\
         SELECT NULL = 1 AS null_left;\n\
         SELECT 'x' <> NULL AS null_right;",
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "integer_equal\ntrue\n\
         integer_not_equal\nfalse\n\
         float_equal\nfalse\n\
         boolean_not_equal\ntrue\n\
         string_equal\ntrue\n\
         null_literal\n\\N\n\
         null_left\n\\N\n\
         null_right\n\\N\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn escapes_csv_strings_and_decodes_sql_quotes() {
    let output = run(
        &["--format=csv"],
        "SELECT '' AS empty;\n\
         SELECT 'plain' AS text;\n\
         SELECT 'a,\"b\"' AS punctuation;\n\
         SELECT 'line\nnext' AS lines;\n\
         SELECT 'it''s done' AS apostrophe;",
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "empty\n\"\"\ntext\nplain\npunctuation\n\"a,\"\"b\"\"\"\nlines\n\"line\nnext\"\napostrophe\nit's done\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn enforces_the_sql_input_size_limit_at_the_boundary() {
    let accepted_sql = padded_multibyte_sql(rusthouse::MAX_SQL_INPUT_BYTES);

    let accepted = run(&["--format", "csv"], &accepted_sql);
    assert!(accepted.status.success());
    assert_eq!(String::from_utf8(accepted.stdout).unwrap(), "1\n1\n");
    assert!(accepted.stderr.is_empty());

    let rejected_sql = padded_multibyte_sql(rusthouse::MAX_SQL_INPUT_BYTES + 1);
    let rejected = run(&["--format", "csv"], &rejected_sql);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    assert!(
        String::from_utf8(rejected.stderr)
            .unwrap()
            .contains(&format!(
                "SQL input exceeds the {}-byte limit",
                rusthouse::MAX_SQL_INPUT_BYTES
            ))
    );
}

fn padded_multibyte_sql(byte_len: usize) -> String {
    const PREFIX: &str = "SELECT 1;";
    const MULTIBYTE_WHITESPACE: &str = "\u{2003}";

    let padding_len = byte_len - PREFIX.len() - MULTIBYTE_WHITESPACE.len();
    let mut sql = String::with_capacity(byte_len);
    sql.push_str(PREFIX);
    sql.push_str(&" ".repeat(padding_len));
    sql.push_str(MULTIBYTE_WHITESPACE);
    assert_eq!(sql.len(), byte_len);
    assert!(sql.chars().count() < sql.len());
    sql
}

#[test]
fn rejects_invalid_arguments() {
    for arguments in [
        Vec::<&str>::new(),
        vec!["--format", "json"],
        vec!["--unknown"],
        vec!["--format", "csv", "extra"],
    ] {
        let output = run(&arguments, "");

        assert_eq!(output.status.code(), Some(2), "arguments: {arguments:?}");
        assert!(output.stdout.is_empty(), "arguments: {arguments:?}");
        assert!(
            String::from_utf8(output.stderr).unwrap().contains("error:"),
            "arguments: {arguments:?}"
        );
    }
}

#[test]
fn rejects_malformed_sql_without_partial_csv() {
    for sql in [
        "SELECT 1",
        "SELECT column_name;",
        "SELECT 'unterminated;",
        "SELECT 1e999;",
        "SELECT 1AS alias;",
        "SELECT 1; SELECT nope;",
        "CREATE TABLE empty ();",
        "CREATE TABLE duplicate (id Int64, id String);",
        "CREATE TABLE unknown (value Decimal);",
    ] {
        let output = run(&["--format", "csv"], sql);

        assert_eq!(output.status.code(), Some(1), "SQL: {sql}");
        assert!(output.stdout.is_empty(), "SQL: {sql}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("SQL error"),
            "SQL: {sql}"
        );
    }
}

#[test]
fn later_mixed_type_comparison_produces_no_partial_csv() {
    let output = run(
        &["--format", "csv"],
        "SELECT 1 = 1 AS valid; SELECT 1 = '1' AS invalid;",
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error: SQL error at line 1, column 33: operator '=' cannot compare Integer and String\n"
    );
}
