use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rusthouse::{DEFAULT_MAX_INPUT_BYTES, DEFAULT_MAX_RESULT_BYTES};

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
fn ordered_top_n_renders_in_lexicographic_order_as_csv() {
    let output = run_cli(
        "CREATE TABLE events (
            id Int64, score Float64, active Bool, label String
         );
         INSERT INTO events VALUES
            (1, 2.0, true, 'beta'),
            (2, 1.5, true, 'alpha'),
            (3, 2.0, true, 'alpha'),
            (4, -1.0, false, 'alpha');
         SELECT id, label, score, active FROM events
         ORDER BY active DESC, label ASC, score DESC, id ASC
         LIMIT 3;",
        &["--format", "csv"],
    );

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "id,label,score,active\n\
         3,alpha,2,true\n\
         2,alpha,1.5,true\n\
         1,beta,2,true\n"
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

#[test]
fn oversized_open_stdin_is_rejected_without_waiting_for_eof() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rusthouse CLI");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let oversized = vec![b' '; DEFAULT_MAX_INPUT_BYTES + 1];
    if let Err(error) = stdin.write_all(&oversized) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait().expect("poll rusthouse CLI").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill hung rusthouse CLI");
            drop(stdin);
            child.wait().expect("reap rusthouse CLI");
            panic!("CLI waited for EOF after receiving oversized input");
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Keep the writer alive until after the child exits to prove EOF was not
    // needed to produce the failure.
    drop(stdin);
    let output = child.wait_with_output().expect("collect rusthouse output");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("exceeding the limit"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn duplicate_projection_cannot_amplify_one_string_past_the_result_byte_limit() {
    let payload = "a".repeat(1_040_000);
    let projection = std::iter::repeat_n("x", 1024).collect::<Vec<_>>().join(",");
    let batch = format!(
        "CREATE TABLE t (x String);\
         INSERT INTO t VALUES ('{payload}');\
         SELECT {projection} FROM t;"
    );
    assert!(
        batch.len() < DEFAULT_MAX_INPUT_BYTES,
        "regression batch must fit below the CLI input limit"
    );

    let output = run_cli(&batch, &["--format", "csv"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("query result requires"),
        "unexpected stderr: {stderr}"
    );
    assert!(stderr.contains(&DEFAULT_MAX_RESULT_BYTES.to_string()));
}
