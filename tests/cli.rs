use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_with_input(input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn run_with_stdin(sql: &str) -> Output {
    run_with_input(sql.as_bytes())
}

#[test]
fn executes_a_stdin_session_and_preserves_semicolons_in_strings() {
    let output = run_with_stdin(
        "CREATE TABLE t (id Int64, label String);\n\
         INSERT INTO t VALUES (1, 'a;b'), (2, '東京');\n\
         SELECT * FROM T;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id,label\r\n1,a;b\r\n2,\xe6\x9d\xb1\xe4\xba\xac\r\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn executes_the_checked_in_file_workflow() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cli_workflow.sql"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg(fixture)
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "id,label\r\n1,first\r\n2,semi;colon\r\n3,it's UTF-8: \u{6771}\u{4eac}\r\n"
    );
}

#[test]
fn reports_the_failing_statement_and_exits_unsuccessfully() {
    let output = run_with_stdin(
        "CREATE TABLE t (id Int64);\n\
         INSERT INTO t VALUES ('wrong');\n\
         SELECT * FROM t;",
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("statement 2 failed"), "{stderr:?}");
    assert!(
        stderr.contains("has type String, expected Int64"),
        "{stderr:?}"
    );
}

#[test]
fn documents_the_command_line_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: rusthouse [FILE]"));
    assert!(stdout.contains("standard input"));
    assert!(stdout.contains("SELECT results"));
}

#[test]
fn rejects_non_utf8_input() {
    let output = run_with_input(&[0xff, 0xfe]);

    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: SQL input is not valid UTF-8\n"
    );
}

#[test]
fn rejects_input_over_the_script_limit() {
    let output = run_with_input(&vec![b' '; 8 * 1024 * 1024 + 1]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("exceeds the 8388608-byte script limit"));
}
