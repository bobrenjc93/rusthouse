use std::io::Write;
use std::process::{Command, Output, Stdio};

fn rusthouse(arguments: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rusthouse binary starts");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .expect("test input is written");
    child.wait_with_output().expect("rusthouse binary exits")
}

#[test]
fn executes_every_scalar_literal_type_and_aliases() {
    let cases = [
        ("SELECT 42 AS answer", "\"answer\"\n\"42\"\n"),
        ("SELECT -3.25 AS delta;", "\"delta\"\n\"-3.25\"\n"),
        ("select TRUE as enabled", "\"enabled\"\n\"true\"\n"),
        (
            "SELECT 'Ada''s \"analysis\"' AS note",
            "\"note\"\n\"Ada's \"\"analysis\"\"\"\n",
        ),
    ];

    for (sql, expected) in cases {
        let output = rusthouse(&["--format", "csv"], sql.as_bytes());
        assert!(
            output.status.success(),
            "{sql:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn preserves_literal_spelling_as_the_unaliased_column_name() {
    let output = rusthouse(&["--format=csv"], b"SELECT 'customer''s note'");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\"'customer''s note'\"\n\"customer's note\"\n"
    );
}

#[test]
fn help_succeeds_without_reading_a_query() {
    let output = rusthouse(&["--help"], b"");

    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Usage: rusthouse --format csv")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_unsupported_and_multiple_statements_emit_no_output() {
    let cases: &[(&[&str], &[u8])] = &[
        (&["--format", "csv"], b"SELECT"),
        (&["--format", "csv"], b"SELECT NULL"),
        (&["--format", "csv"], b"SELECT 1 + 2"),
        (&["--format", "csv"], b"SELECT 1; SELECT 2"),
        (&["--format", "json"], b"SELECT 1"),
    ];

    for (arguments, sql) in cases {
        let output = rusthouse(arguments, sql);
        assert!(!output.status.success(), "input unexpectedly succeeded");
        assert!(output.stdout.is_empty(), "failure emitted partial stdout");
        assert!(!output.stderr.is_empty(), "failure omitted its diagnostic");
    }
}

#[test]
fn rejects_input_beyond_the_query_limit_without_output() {
    let oversized = vec![b' '; 1024 * 1024 + 1];
    let output = rusthouse(&["--format", "csv"], &oversized);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("1048576-byte limit")
    );
}
