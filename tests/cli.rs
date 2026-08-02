use std::io::Write;
use std::process::{Command, Output, Stdio};

use rusthouse::MAX_SQL_INPUT_BYTES;

const ARGUMENT_ERROR_SUFFIX: &str =
    "\n\nUsage: rusthouse [OPTIONS]\n\nFor more information, try '--help'.\n";

fn rusthouse(arguments: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rusthouse"));
    command
        .args(arguments)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("rusthouse should start");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin should be piped")
            .write_all(input)
            .expect("rusthouse should consume stdin");
    }
    child.wait_with_output().expect("rusthouse should finish")
}

#[test]
fn help_output_is_exact() {
    let output = rusthouse(&["--help"], None);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "rusthouse ",
            env!("CARGO_PKG_VERSION"),
            "\n",
            env!("CARGO_PKG_DESCRIPTION"),
            "\n\n",
            "Usage: rusthouse [OPTIONS]\n\n",
            "Options:\n",
            "  -e, --execute <SQL>    Execute SQL instead of reading stdin\n",
            "      --format <FORMAT>  Output format [default: csv] [possible value: csv]\n",
            "  -h, --help             Print help\n",
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn execute_emits_an_exact_csv_header_and_row() {
    let output = rusthouse(
        &["--execute", "SELECT -42 AS answer", "--format", "csv"],
        None,
    );

    assert!(output.status.success());
    assert_eq!(output.stdout, b"answer\n-42\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn stdin_is_used_when_execute_is_absent() {
    let output = rusthouse(&["--format=csv"], Some(b"  select +9223372036854775807;\n"));

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"+9223372036854775807\n9223372036854775807\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_arguments_fail_without_reading_stdin() {
    let output = rusthouse(&["--format", "json"], None);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("error: unsupported format 'json'; only 'csv' is available{ARGUMENT_ERROR_SUFFIX}")
    );

    let output = rusthouse(&["--unknown"], None);
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("error: unexpected argument '--unknown'{ARGUMENT_ERROR_SUFFIX}")
    );
}

#[test]
fn unsupported_sql_is_rejected_exactly() {
    let output = rusthouse(&["--execute", "SELECT 1 + 2"], None);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: unsupported SQL: expected SELECT <signed Int64> [AS <identifier>] with an optional trailing semicolon\n"
    );
}

#[test]
fn oversized_stdin_is_fully_drained_before_the_error() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rusthouse should start");
    let oversized_input = vec![b' '; MAX_SQL_INPUT_BYTES + 64 * 1024];

    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(&oversized_input)
        .expect("the complete oversized input should be drained");
    let output = child.wait_with_output().expect("rusthouse should finish");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("error: SQL input exceeds the {MAX_SQL_INPUT_BYTES}-byte limit\n")
    );
}
