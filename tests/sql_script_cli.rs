use std::io::Write;
use std::process::{Command, Output, Stdio};

fn rusthouse(input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", "csv"])
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
fn executes_create_insert_and_select_and_emits_selects_in_order() {
    let output = rusthouse(
        br#"
            CREATE TABLE events (id Int64, score Float64, active Bool, label String);
            INSERT INTO events VALUES (1, 9.5, true, 'first');
            INSERT INTO events VALUES (2, -3.25, false, 'second');
            SELECT label, id, active, score FROM events;
            SELECT 42 AS answer;
        "#,
    );

    assert!(
        output.status.success(),
        "script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "\"label\",\"id\",\"active\",\"score\"\n",
            "\"first\",\"1\",\"true\",\"9.5\"\n",
            "\"second\",\"2\",\"false\",\"-3.25\"\n",
            "\"answer\"\n",
            "\"42\"\n",
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn command_only_scripts_emit_nothing() {
    let output = rusthouse(b"CREATE TABLE events (id Int64); INSERT INTO events VALUES (1);");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_scripts_fail_closed_without_partial_select_output() {
    let scripts: &[&[u8]] = &[
        b"SELECT 1 AS completed; SELECT",
        b"SELECT 1 AS completed; DROP TABLE missing",
        b"CREATE TABLE events (id Int64);; SELECT id FROM events",
        b"CREATE TABLE events (id Int64); INSERT INTO events VALUES ('wrong')",
        b"CREATE TABLE events (id Int64) INSERT INTO events VALUES (1)",
        b"SELECT 'unterminated",
    ];

    for script in scripts {
        let output = rusthouse(script);
        assert!(
            !output.status.success(),
            "script unexpectedly succeeded: {}",
            String::from_utf8_lossy(script)
        );
        assert!(output.stdout.is_empty(), "failure emitted partial stdout");
        assert!(!output.stderr.is_empty(), "failure omitted its diagnostic");
    }
}
