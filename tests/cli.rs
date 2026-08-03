use std::io::{Cursor, Write};
use std::process::{Command, Output, Stdio};

use rusthouse::cli::{
    BatchError, MAX_BATCH_BYTES, MAX_BATCH_STATEMENTS, MAX_STATEMENT_BYTES, execute_batch,
};
use rusthouse::{Catalog, DEFAULT_MAX_TABLES};

const BINARY: &str = env!("CARGO_BIN_EXE_rusthouse");

fn run(arguments: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(BINARY)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start rusthouse");

    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("write stdin");
    child.wait_with_output().expect("wait for rusthouse")
}

fn run_with_closed_stdout(arguments: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(BINARY)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start rusthouse");

    drop(child.stdout.take().expect("piped stdout"));
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("write stdin");
    child.wait_with_output().expect("wait for rusthouse")
}

#[test]
fn help_describes_the_bounded_batch_contract() {
    for argument in ["--help", "-h"] {
        let output = run(&[argument], b"");

        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage: rusthouse [--format csv]"));
        assert!(stdout.contains("CREATE TABLE, INSERT INTO ... VALUES, and SELECT"));
        assert!(stdout.contains("--format csv"));
        assert!(stdout.contains("1048576 bytes per statement"));
        assert!(stdout.contains("4  unsupported statement"));
        assert!(stdout.contains("6  stdout write error"));
        assert_eq!(stdout.matches("Exit codes:").count(), 1);
    }
}

#[test]
fn rejects_arguments_with_the_usage_exit_code() {
    for arguments in [
        &["--unknown"][..],
        &["--format"][..],
        &["--format", "json"][..],
        &["--format", "CSV"][..],
        &["--format", "csv", "extra"][..],
    ] {
        let output = run(arguments, b"");

        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "rusthouse: invalid arguments; try 'rusthouse --help'\n"
        );
    }
}

#[test]
fn executes_mixed_create_and_insert_lines_in_one_catalog() {
    let output = run(
        &[],
        b"\n  \r\nCREATE TABLE Events (id Int64, ratio Float64, active Bool, label String)\n\
          INSERT INTO events VALUES (1, 1.5, true, 'first')\n\
          insert into EVENTS values (2, -3.25, FALSE, 'second')\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn reports_malformed_supported_statements_with_line_numbers() {
    let output = run(
        &[],
        b"\nCREATE TABLE events (id Int64)\nINSERT INTO events VALUES ('wrong')\n",
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: execution error on line 3: could not insert into table `events`: batch row 0, column 0 (`id`) has type String; expected Int64\n"
    );
}

#[test]
fn rejects_non_utf8_and_unsupported_statements_deterministically() {
    let invalid_utf8 = run(
        &[],
        &[b'C', b'R', b'E', b'A', b'T', b'E', b' ', 0xff, b'\n'],
    );
    assert_eq!(invalid_utf8.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(invalid_utf8.stderr).unwrap(),
        "rusthouse: input error on line 1: statement is not valid UTF-8\n"
    );

    let unsupported = run(&[], b"DELETE FROM events\n");
    assert_eq!(unsupported.status.code(), Some(4));
    assert!(unsupported.stdout.is_empty());
    assert_eq!(
        String::from_utf8(unsupported.stderr).unwrap(),
        "rusthouse: unsupported statement on line 1: expected CREATE TABLE, INSERT INTO, or SELECT\n"
    );
}

#[test]
fn writes_each_select_with_projected_columns_and_filtered_rows() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, active Bool, label String)\n\
          INSERT INTO events VALUES (1, true, 'first'), (2, false, 'second'), (3, true, 'third')\n\
          SELECT label, id, label FROM events WHERE active = true\n\
          SELECT active FROM events WHERE id >= 2\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        concat!(
            "\"label\",\"id\",\"label\"\n",
            "\"first\",1,\"first\"\n",
            "\"third\",3,\"third\"\n",
            "\"active\"\n",
            "false\n",
            "true\n",
        )
        .as_bytes()
    );
}

#[test]
fn writes_only_projected_headers_for_empty_results() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, label String)\n\
          INSERT INTO events VALUES (1, 'first')\n\
          SELECT label, id FROM events WHERE id > 10\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, b"\"label\",\"id\"\n");
}

#[test]
fn applies_limit_before_streaming_select_rows() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, active Bool)\n\
          INSERT INTO events VALUES (1, true), (2, false), (3, true), (4, true)\n\
          SELECT id FROM events WHERE active = true LIMIT 2\n\
          SELECT active, id FROM events LIMIT 0\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, b"\"id\"\n1\n3\n\"active\",\"id\"\n");
}

#[test]
fn streams_grouped_aggregate_results_in_order() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, category String, score Float64)\n\
          INSERT INTO events VALUES (1, 'a', 1.5), (2, 'b', 7.0), (3, 'a', 2.5)\n\
          SELECT category, count(*) AS rows, sum(score) AS total FROM events GROUP BY category ORDER BY total DESC\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        b"\"category\",\"rows\",\"total\"\n\"b\",1,7\n\"a\",2,4\n"
    );
}

#[test]
fn escapes_select_strings_as_csv() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE messages (id Int64, body String)\n\
          INSERT INTO messages VALUES (7, 'comma, \"quote\" and apostrophe ''')\n\
          SELECT body, id FROM messages\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        b"\"body\",\"id\"\n\"comma, \"\"quote\"\" and apostrophe '\",7\n"
    );
}

#[test]
fn reports_stdout_failures_against_the_select_line() {
    let output = run_with_closed_stdout(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64)\n\
          INSERT INTO events VALUES (1)\n\
          SELECT id FROM events\n",
    );

    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: output error on line 3: could not write SELECT result\n"
    );
}

#[test]
fn enforces_the_per_statement_byte_bound() {
    let prefix = b"CREATE TABLE exact (id Int64)";
    let mut exact = Vec::with_capacity(MAX_STATEMENT_BYTES + 1);
    exact.extend_from_slice(prefix);
    exact.resize(MAX_STATEMENT_BYTES, b' ');
    exact.push(b'\n');

    let accepted = run(&[], &exact);
    assert_eq!(accepted.status.code(), Some(0));
    assert!(accepted.stderr.is_empty());

    exact.insert(MAX_STATEMENT_BYTES, b' ');
    let rejected = run(&[], &exact);
    assert_eq!(rejected.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(rejected.stderr).unwrap(),
        format!(
            "rusthouse: input limit exceeded on line 1: statement exceeds {MAX_STATEMENT_BYTES} bytes\n"
        )
    );
}

#[test]
fn enforces_the_nonempty_statement_count_bound() {
    let mut input = String::from("CREATE TABLE bounded (id Int64)\n");
    for _ in 1..MAX_BATCH_STATEMENTS {
        input.push_str("INSERT INTO bounded VALUES (1)\n");
    }
    input.push_str("INSERT INTO bounded VALUES (2)\n");

    let output = run(&[], input.as_bytes());

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "rusthouse: input limit exceeded on line {}: batch exceeds {MAX_BATCH_STATEMENTS} statements\n",
            MAX_BATCH_STATEMENTS + 1
        )
    );
}

#[test]
fn reports_catalog_capacity_as_a_limit() {
    let mut input = String::new();
    for table in 0..=DEFAULT_MAX_TABLES {
        input.push_str(&format!("CREATE TABLE t{table} (id Int64)\n"));
    }

    let output = run(&[], input.as_bytes());

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "rusthouse: resource limit exceeded on line {}: catalog table count exceeds limit of {DEFAULT_MAX_TABLES}\n",
            DEFAULT_MAX_TABLES + 1
        )
    );
}

#[test]
fn enforces_the_total_stdin_byte_bound() {
    let mut input = Vec::with_capacity(MAX_BATCH_BYTES + MAX_STATEMENT_BYTES);
    while input.len() <= MAX_BATCH_BYTES {
        input.resize(input.len() + MAX_STATEMENT_BYTES, b' ');
        input.push(b'\n');
    }

    let output = run(&[], &input);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "rusthouse: input limit exceeded on line 16: stdin exceeds {MAX_BATCH_BYTES} bytes\n"
        )
    );

    let mut input = Cursor::new(input);
    let error = execute_batch(&mut input, &mut Catalog::new()).unwrap_err();
    assert!(matches!(error, BatchError::BatchTooLarge { .. }));
    assert_eq!(input.position(), MAX_BATCH_BYTES as u64 + 1);
}
