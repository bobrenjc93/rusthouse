use std::io::Write;
use std::process::{Command, Output, Stdio};

use rusthouse::MAX_INPUT_BYTES;

fn run(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn help_describes_stdin_and_csv_without_reading_input() {
    const EXPECTED_HELP: &str = concat!(
        "RustHouse\n",
        "\n",
        "Usage: rusthouse [OPTIONS]\n",
        "\n",
        "Reads semicolon-separated literal SELECT statements from standard input.\n",
        "\n",
        "Options:\n",
        "      --format <FORMAT>  Output format [default: csv] [possible values: csv]\n",
        "  -h, --help             Print help\n",
    );
    let output = run(&["--help"], b"");

    assert!(output.status.success());
    assert_eq!(output.stdout, EXPECTED_HELP.as_bytes());
    assert!(output.stderr.is_empty());
}

#[test]
fn explicit_csv_format_emits_an_aliased_header() {
    let output = run(&["--format", "csv"], b"SELECT -42 AS signed_value;");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"signed_value\n-42\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn unknown_options_and_formats_fail() {
    let unknown = run(&["--unknown"], b"SELECT 1");
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        String::from_utf8(unknown.stderr)
            .unwrap()
            .contains("unknown option")
    );

    let format = run(&["--format", "json"], b"SELECT 1");
    assert_eq!(format.status.code(), Some(2));
    assert!(
        String::from_utf8(format.stderr)
            .unwrap()
            .contains("only supported format is csv")
    );
}

#[test]
fn multiple_statements_each_emit_a_header_and_row() {
    let output = run(&[], b"SELECT 1 AS one;\nselect -2; SELECT +3 AS three;");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"one\n1\nvalue\n-2\nthree\n3\n");
}

#[test]
fn overflowing_int64_literal_fails_without_partial_output() {
    let output = run(&[], b"SELECT 1; SELECT 9223372036854775808 AS too_large;");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("outside the Int64 range")
    );
}

#[test]
fn unsupported_sql_fails_without_output() {
    let output = run(&[], b"CREATE TABLE metrics (value Int64);");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unsupported SQL")
    );
}

#[test]
fn stdin_has_a_fixed_byte_limit() {
    let mut at_limit = b"SELECT 7".to_vec();
    at_limit.resize(MAX_INPUT_BYTES, b' ');
    let accepted = run(&[], &at_limit);
    assert!(accepted.status.success());
    assert_eq!(accepted.stdout, b"value\n7\n");

    at_limit.push(b' ');
    let rejected = run(&[], &at_limit);
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    assert!(
        String::from_utf8(rejected.stderr)
            .unwrap()
            .contains("byte limit")
    );
}
