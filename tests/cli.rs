use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_cli(input: &str, arguments: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rusthouse CLI");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write complete SQL batch");
    child.wait_with_output().expect("wait for rusthouse CLI")
}

#[test]
fn create_insert_select_batch_renders_all_types_as_csv() {
    let output = run_cli(
        "CREATE TABLE events (id Int64, score Float64, active Bool, label String);\n\
         INSERT INTO events VALUES\n\
           (1, 2.5, true, 'plain'),\n\
           (2, -3.25, false, 'comma, and \"quote\"');\n\
         SELECT * FROM events;\n",
        &["--format", "csv"],
    );

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "id,score,active,label\n\
         1,2.5,true,plain\n\
         2,-3.25,false,\"comma, and \"\"quote\"\"\"\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn typed_sql_failure_has_nonzero_status_and_stderr() {
    let output = run_cli(
        "CREATE TABLE events (id Int64);\n\
         INSERT INTO events VALUES ('not an integer');\n\
         SELECT * FROM events;",
        &["--format=csv"],
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("insert failed"),
        "unexpected stderr: {stderr}"
    );
    assert!(stderr.contains("expects Int64 but received String"));
}
