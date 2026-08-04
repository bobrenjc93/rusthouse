use std::io::Write;
use std::process::{Command, Output, Stdio};

use rusthouse::{DEFAULT_MAX_SESSION_BYTES, DEFAULT_MAX_SESSION_STATEMENTS};

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
fn executes_a_catalog_lifecycle_and_formats_nullable_rows() {
    let output = run(
        &[],
        b"\nCREATE TABLE readings (value Int64)\r\n\
          INSERT INTO readings VALUES (7)\n\
          INSERT INTO readings VALUES (NULL)\n\
          INSERT INTO readings VALUES (-2)\n\
          SELECT value FROM readings\n",
    );

    assert!(output.status.success());
    assert_eq!(output.stdout, b"[7, NULL, -2]\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn executes_checked_addition_projections() {
    let output = run(
        &[],
        b"CREATE TABLE readings (value Int64)\n\
          INSERT INTO readings VALUES (2)\n\
          INSERT INTO readings VALUES (NULL)\n\
          INSERT INTO readings VALUES (4)\n\
          SELECT value + 3 FROM readings\n",
    );

    assert!(output.status.success());
    assert_eq!(output.stdout, b"[5, NULL, 7]\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn reports_checked_addition_overflow() {
    let output = run(
        &[],
        b"CREATE TABLE readings (value Int64 NOT NULL)\n\
          INSERT INTO readings VALUES (9223372036854775807)\n\
          SELECT value + 1 FROM readings\n",
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: line 3: could not execute SELECT: adding 1 to 9223372036854775807 in the SELECT projection overflows Int64\n"
    );
}

#[test]
fn prints_each_select_result_in_statement_order() {
    let output = run(
        &[],
        b"CREATE TABLE readings (value Int64 NOT NULL)\n\
          INSERT INTO readings VALUES (3)\n\
          SELECT value FROM readings\n\
          INSERT INTO readings VALUES (5)\n\
          SELECT value FROM readings WHERE value >= 5\n\
          SELECT value FROM readings LIMIT 0\n",
    );

    assert!(output.status.success());
    assert_eq!(output.stdout, b"[3]\n[5]\n[]\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn help_prints_usage_without_reading_a_session() {
    for argument in ["--help", "-h"] {
        let output = run(&[argument], b"not SQL\n");

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage: rusthouse [OPTIONS]"));
        assert!(stdout.contains("65536 input bytes, 1024 statements, 64 tables"));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn malformed_statement_is_reported_on_stderr_with_failure_status() {
    let output = run(
        &[],
        b"CREATE TABLE readings (value Int64)\nSELECT FROM readings\n",
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("error: line 2: could not parse SQL:"));
    assert!(stderr.contains("expected identifier"));
}

#[test]
fn accepts_exact_session_byte_bound() {
    let input = vec![b' '; DEFAULT_MAX_SESSION_BYTES];
    let output = run(&[], &input);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn rejects_exceeded_session_byte_bound() {
    let input = vec![b' '; DEFAULT_MAX_SESSION_BYTES + 1];
    let output = run(&[], &input);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "error: session input has at least {} bytes, exceeding the limit of {} bytes\n",
            DEFAULT_MAX_SESSION_BYTES + 1,
            DEFAULT_MAX_SESSION_BYTES
        )
    );
}

#[test]
fn accepts_exact_session_statement_bound() {
    let mut input = String::from("CREATE TABLE readings (value Int64 NOT NULL)\n");
    for _ in 1..DEFAULT_MAX_SESSION_STATEMENTS {
        input.push_str("INSERT INTO readings VALUES (1)\n");
    }
    let output = run(&[], input.as_bytes());

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn rejects_exceeded_session_statement_bound() {
    let mut input = String::from("CREATE TABLE readings (value Int64 NOT NULL)\n");
    for _ in 1..DEFAULT_MAX_SESSION_STATEMENTS {
        input.push_str("INSERT INTO readings VALUES (1)\n");
    }
    input.push_str("SELECT value FROM readings LIMIT 0\n");
    let output = run(&[], input.as_bytes());

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "error: line {} raises the session to {} statements, exceeding the limit of {}\n",
            DEFAULT_MAX_SESSION_STATEMENTS + 1,
            DEFAULT_MAX_SESSION_STATEMENTS + 1,
            DEFAULT_MAX_SESSION_STATEMENTS
        )
    );
}
