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
        "SELECT 7 AS first;\nSELECT -2.5 AS second;\nSELECT TRUE;\n",
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "first\n7\nsecond\n-2.5\nTRUE\ntrue\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn escapes_csv_strings_and_decodes_sql_quotes() {
    let output = run(
        &["--format=csv"],
        "SELECT 'plain' AS text;\n\
         SELECT 'a,\"b\"' AS punctuation;\n\
         SELECT 'line\nnext' AS lines;\n\
         SELECT 'it''s done' AS apostrophe;",
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "text\nplain\npunctuation\n\"a,\"\"b\"\"\"\nlines\n\"line\nnext\"\napostrophe\nit's done\n"
    );
    assert!(output.stderr.is_empty());
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
        "SELECT 1; SELECT nope;",
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
