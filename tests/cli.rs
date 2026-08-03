use rusthouse::MAX_SQL_INPUT_BYTES;
use std::ffi::OsString;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_cli(arguments: &[&str], input: &[u8]) -> Output {
    let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
    run_cli_with_input_policy(&arguments, input, false)
}

fn run_cli_allowing_closed_stdin(arguments: &[&str], input: &[u8]) -> Output {
    let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
    run_cli_with_input_policy(&arguments, input, true)
}

#[cfg(unix)]
fn run_cli_os(arguments: &[OsString], input: &[u8]) -> Output {
    run_cli_with_input_policy(arguments, input, false)
}

fn run_cli_with_input_policy(
    arguments: &[OsString],
    input: &[u8],
    allow_broken_pipe: bool,
) -> Output {
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

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_usage_errors() {
    use std::os::unix::ffi::OsStringExt;

    let output = run_cli_os(&[OsString::from_vec(vec![b'-', 0xff])], b"");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: argument is not valid UTF-8\n\
         Try 'rusthouse --help' for more information.\n"
    );
}

#[test]
fn argument_diagnostics_escape_terminal_controls() {
    let output = run_cli(&["bad\x1b\nargument"], b"");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        "rusthouse: unrecognized argument `bad\\u{1b}\\nargument`\n\
         Try 'rusthouse --help' for more information.\n"
    );
    assert!(!stderr.contains('\x1b'));
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
fn evaluates_int64_arithmetic_with_precedence_aliases_and_negative_operands() {
    let output = run_cli(
        &["--format", "csv"],
        b"SELECT 2 + 3 * 4 AS precedence, -5 * -2 + +1 signed_value;",
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "precedence,signed_value\n14,11\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn arithmetic_overflow_emits_no_partial_batch_results() {
    let output = run_cli(
        &["--format", "csv"],
        b"SELECT 1 AS completed; SELECT 9223372036854775807 + 1 AS overflowed;",
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: SQL evaluation error at byte 51: Int64 overflow while evaluating operator `+`\n"
    );
}

#[test]
fn unaliased_strings_preserve_escaped_quotes_in_csv_headers() {
    let output = run_cli(&["--format", "csv"], b"SELECT 'it''s';");

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "'it''s'\nit's\n");
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
fn diagnostics_escape_terminal_control_characters() {
    let output = run_cli(&[], b"SELECT 1 \x1b;");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        "rusthouse: SQL syntax error at byte 10: unexpected character `\\u{1b}`\n"
    );
    assert!(!stderr.contains('\x1b'));

    let output = run_cli(&[], b"SELECT \"\x1b\";");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("identifier `\\u{1b}`"));
    assert!(!stderr.contains('\x1b'));
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
    let output = run_cli(&[], b"SELECT 1 AS one FROM numbers WHERE value = 1;");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: unsupported SQL clause `FROM` at byte 17; only literal SELECT projections are supported\n"
    );
}

#[test]
fn near_limit_projection_batches_are_bounded() {
    let mut input = String::with_capacity(MAX_SQL_INPUT_BYTES);
    input.push_str("SELECT ");
    while input.len() + 2 < MAX_SQL_INPUT_BYTES {
        input.push_str("1,");
    }
    input.pop();
    input.push(';');
    assert!(input.len() >= MAX_SQL_INPUT_BYTES - 1);

    let output = run_cli(&[], input.as_bytes());

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("SQL projection limit exceeded")
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
