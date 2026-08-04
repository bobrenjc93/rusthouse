use std::io::Write;
use std::process::{Child, Command, Output, Stdio};

use rusthouse::{DEFAULT_MAX_SESSION_BYTES, DEFAULT_MAX_SESSION_STATEMENTS};

fn run(args: &[&str], input: &[u8]) -> Output {
    let mut child = spawn(args);
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn spawn(args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn csv_batch_emits_typed_projection_and_all_scalar_aggregates() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE metrics (
              id Int64,
              score Float64,
              enabled Bool,
              label String
          );
          INSERT INTO metrics VALUES
              (1, 1.5, true, 'semi;colon'),
              (2, 2.5, false, 'comma,value'),
              (3, 4.0, true, 'quote''d');
          SELECT id, score, enabled, label FROM metrics ORDER BY id;
          SELECT COUNT(*) AS row_count,
                 SUM(id) AS id_sum,
                 MIN(score) AS score_min,
                 MAX(score) AS score_max,
                 AVG(score) AS score_avg
          FROM metrics;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id,score,enabled,label\n\
          1,1.5,true,semi;colon\n\
          2,2.5,false,\"comma,value\"\n\
          3,4.0,true,quote'd\n\
          row_count,id_sum,score_min,score_max,score_avg\n\
          3,6,1.5,4.0,2.6666666666666665\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn fixed_harness_style_write_completes_without_early_exit_or_broken_pipe() {
    const ROWS: usize = 4_096;
    let mut sql = String::from(
        "CREATE TABLE parity_data (id Int64, score Float64, flag Bool, label String);\n\
         INSERT INTO parity_data VALUES ",
    );
    for row in 0..ROWS {
        if row != 0 {
            sql.push(',');
        }
        let flag = row % 2 == 0;
        sql.push_str(&format!("({row},{}.5,{flag},'row_{row:05}')", row % 100));
    }
    sql.push_str(
        ";\nSELECT COUNT(*) AS row_count, SUM(id) AS total, MIN(score) AS low, \
         MAX(score) AS high, AVG(score) AS mean FROM parity_data;\n\
         SELECT COUNT(*) AS row_count FROM parity_data;\n",
    );
    assert!(
        sql.len() > 64 * 1024,
        "input must exceed a typical pipe buffer"
    );

    let mut child = spawn(&["--format", "csv"]);
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(sql.as_bytes())
        .expect("the process must keep stdin open for the complete batch");
    drop(stdin);
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("row_count,total,low,high,mean\n4096,8386560,"));
    assert!(stdout.ends_with("row_count\n4096\n"));
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_is_the_only_accepted_format_argument() {
    for args in [
        &["--format", "json"][..],
        &["--format", "CSV"][..],
        &["--format", "csv", "extra"][..],
    ] {
        let output = run(args, b"");
        assert!(!output.status.success(), "{args:?}");
    }
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
