use rusthouse::MAX_SQL_INPUT_BYTES;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_cli(arguments: &[&str], input: &[u8]) -> Output {
    run_cli_with_input_policy(arguments, input, false)
}

fn run_cli_allowing_closed_stdin(arguments: &[&str], input: &[u8]) -> Output {
    run_cli_with_input_policy(arguments, input, true)
}

fn run_cli_with_input_policy(arguments: &[&str], input: &[u8], allow_broken_pipe: bool) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI should start");

    let write_result = child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(input);
    if let Err(error) = write_result {
        assert!(
            allow_broken_pipe && error.kind() == std::io::ErrorKind::BrokenPipe,
            "unexpected error writing SQL input: {error}"
        );
    }
    child.wait_with_output().expect("CLI should finish")
}

#[test]
fn executes_multiple_typed_selects_as_csv() {
    let output = run_cli(
        &["--format", "csv"],
        b"SELECT 42 AS integer_value, -3.5 AS float_value;\n\
          SELECT TRUE AS enabled, 'a,b \"quoted\" and it''s valid' AS \"text,value\";\n",
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "integer_value,float_value\n\
         42,-3.5\n\
         \n\
         enabled,\"text,value\"\n\
         true,\"a,b \"\"quoted\"\" and it's valid\"\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn help_is_available_without_sql_input() {
    let output = run_cli(&["--help"], b"");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: rusthouse [OPTIONS]"));
    assert!(stdout.contains("--format <FORMAT>"));
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_sql_has_a_stable_error_and_nonzero_status() {
    let output = run_cli(&[], b"SELECT 1 AS;");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: SQL syntax error at byte 12: expected an alias after AS\n"
    );
}

#[test]
fn default_table_output_escapes_multiline_fields() {
    let output = run_cli(&[], b"SELECT 'first\nsecond' AS \"line\nname\";");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "line\\nname   \n-------------\nfirst\\nsecond\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn unsupported_clauses_have_a_stable_error_and_nonzero_status() {
    let output = run_cli(&[], b"SELECT 1 AS one FROM numbers;");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: unsupported SQL clause `FROM` at byte 17; only literal SELECT projections are supported\n"
    );
}

#[test]
fn oversized_input_is_rejected() {
    let input = vec![b' '; MAX_SQL_INPUT_BYTES * 4];
    let output = run_cli_allowing_closed_stdin(&[], &input);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("rusthouse: SQL input exceeds the maximum size of {MAX_SQL_INPUT_BYTES} bytes\n")
    );
}

#[test]
fn invalid_format_is_a_usage_error() {
    let output = run_cli(&["--format", "json"], b"");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: unsupported output format `json`; expected table or csv\n\
         Try 'rusthouse --help' for more information.\n"
    );
}
