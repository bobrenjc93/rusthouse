use std::io::Write;
use std::process::{Command, Output, Stdio};

fn rusthouse() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rusthouse"))
}

fn run_with_stdin(arguments: &[&str], input: &[u8]) -> Output {
    let mut child = rusthouse()
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn help_describes_the_supported_interface() {
    let output = rusthouse().arg("--help").output().unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: rusthouse [OPTIONS]"));
    assert!(stdout.contains("--execute <SQL>"));
    assert!(stdout.contains("--format <FORMAT>"));
    assert!(stdout.contains("read from standard input through EOF"));
    assert!(!stdout.contains("warming up"));
}

#[test]
fn execute_projects_all_types_as_csv_with_names() {
    let output = rusthouse()
        .args([
            "--execute",
            "SELECT -42 AS integer, 1.25e2 AS floating, false AS flag, 'it''s, \"ready\"' AS message;",
            "--format",
            "csv",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\"integer\",\"floating\",\"flag\",\"message\"\n-42,125,false,\"it's, \"\"ready\"\"\"\n"
    );
}

#[test]
fn stdin_is_read_to_eof_and_runs_semicolon_separated_selects() {
    let mut sql = b"SELECT 1 AS first; SELECT 'second' AS label;".to_vec();
    sql.resize(128 * 1024, b' ');

    let output = run_with_stdin(&["--format=csv"], &sql);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\"first\"\n1\n\"label\"\n\"second\"\n"
    );
}

#[test]
fn malformed_arguments_fail_without_query_output() {
    for arguments in [
        vec!["--unknown"],
        vec!["--execute"],
        vec!["--format", "json"],
        vec!["--format=csv", "--format=csv"],
        vec!["--execute=SELECT 1", "--execute=SELECT 2"],
    ] {
        let output = rusthouse().args(arguments).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("argument error"));
    }
}

#[test]
fn malformed_and_out_of_scope_sql_fails() {
    for sql in [
        "SELECT",
        "SELECT 1 FROM system.one",
        "SELECT 1 WHERE true",
        "SELECT count()",
        "CREATE TABLE data (value Int64)",
        "SELECT 'unterminated",
    ] {
        let output = rusthouse().args(["--execute", sql]).output().unwrap();
        assert!(!output.status.success(), "{sql:?} should fail");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("SQL error"));
    }
}

#[test]
fn oversized_stdin_fails() {
    let input = vec![b' '; rusthouse::cli::MAX_QUERY_BYTES + 1];

    let output = run_with_stdin(&[], &input);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("1048576-byte limit"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
