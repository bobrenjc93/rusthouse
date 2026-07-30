use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn execute_argument_emits_clean_json_and_command_statuses() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE items (name String, n Int64);
             INSERT INTO items VALUES ('b', 2), ('a', 1);
             SELECT name, n FROM items ORDER BY n;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"name\",\"type\":\"String\"},{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[\"a\",1],[\"b\",2]]}]}\n"
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("CREATE TABLE"));
    assert!(stderr.contains("INSERT 2"));
}

#[test]
fn multiple_selects_emit_one_json_document() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE numbers (n Int64);
             INSERT INTO numbers VALUES (1), (2);
             SELECT n FROM numbers WHERE n = 1;
             SELECT n FROM numbers WHERE n = 2;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]},{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[2]]}]}\n"
    );
}

#[test]
fn positional_json_preserves_duplicate_alias_values() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE items (id Int64, label String);
             INSERT INTO items VALUES (1, 'one');
             SELECT id, label AS id FROM items;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"id\",\"type\":\"String\"}],\"rows\":[[1,\"one\"]]}]}\n"
    );
}

#[test]
fn stdin_and_csv_output_work_together() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");

    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(
            b"CREATE TABLE notes (label String, active Bool);
              INSERT INTO notes VALUES ('hello, world', true);
              SELECT * FROM notes;",
        )
        .expect("write SQL");

    let output = child.wait_with_output().expect("wait for CLI");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "label,active\n\"hello, world\",true\n"
    );
}

#[test]
fn interactive_mode_retains_state_across_multiline_inputs() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--interactive", "--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");

    let mut input = child.stdin.take().expect("stdin pipe");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr pipe"));

    input
        .write_all(b"CREATE TABLE notes (id Int64, label String);\n")
        .expect("write CREATE");
    input.flush().expect("flush CREATE");
    let mut command_status = String::new();
    stderr
        .read_line(&mut command_status)
        .expect("read CREATE status");
    assert_eq!(command_status, "CREATE TABLE\n");

    input
        .write_all(
            b"INSERT INTO notes VALUES\n  (2, 'semi;colon'),\n  (1, 'first'); -- ignored ;\n",
        )
        .expect("write multiline INSERT");
    input.flush().expect("flush INSERT");
    command_status.clear();
    stderr
        .read_line(&mut command_status)
        .expect("read INSERT status");
    assert_eq!(command_status, "INSERT 2\n");

    input
        .write_all(b"SELECT id, label\nFROM notes\nORDER BY id;\n")
        .expect("write multiline SELECT");
    input.flush().expect("flush SELECT");
    let mut query_output = String::new();
    for _ in 0..3 {
        stdout
            .read_line(&mut query_output)
            .expect("read SELECT output");
    }
    assert_eq!(query_output, "id,label\n1,first\n2,semi;colon\n");

    input.write_all(b".quit\n").expect("write quit command");
    assert!(child.wait().expect("wait for CLI").success());
    drop(input);
}

#[test]
fn interactive_mode_recovers_from_errors_and_exits_on_eof() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--interactive", "--format=json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");

    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(
            b"CREATE TABLE events (id Int64);\n\
              INSERT INTO events VALUES ('wrong');\n\
              INSERT INTO events VALUES (7);\n\
              SELECT id FROM events;\n",
        )
        .expect("write interactive SQL");

    let output = child.wait_with_output().expect("wait for CLI");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[7]]}]}\n"
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("CREATE TABLE"));
    assert!(stderr.contains("expected Int64, found String"));
    assert!(stderr.contains("INSERT 1"));
}

#[test]
fn interactive_and_execute_are_mutually_exclusive() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--interactive", "--execute", "SELECT * FROM t"])
        .output()
        .expect("run CLI");

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("UTF-8 stderr")
            .contains("--interactive cannot be used with --execute")
    );
}

#[test]
fn sql_errors_are_reported_with_nonzero_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--execute",
            "CREATE TABLE t (id Int64); INSERT INTO t VALUES ('wrong');",
        ])
        .output()
        .expect("run CLI");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("type mismatch for column 't.id'"));
    assert!(stderr.contains("expected Int64, found String"));
}

#[test]
fn excessive_predicates_return_cli_errors_without_aborting() {
    let cases = [
        (
            format!(
                "SELECT id FROM things WHERE {}id = 1{}",
                "(".repeat(50_000),
                ")".repeat(50_000)
            ),
            "predicate nesting exceeds limit of 64",
        ),
        (
            format!(
                "SELECT id FROM things WHERE {}",
                vec!["id = 1"; 50_000].join(" OR ")
            ),
            "predicate is too complex; maximum 256 expression nodes",
        ),
    ];

    for (sql, expected_error) in cases {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn CLI");
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(sql.as_bytes())
            .expect("write large SQL query");

        let output = child.wait_with_output().expect("wait for CLI");
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            stderr.contains(expected_error),
            "unexpected stderr: {stderr}"
        );
        assert!(!stderr.contains("stack overflow"));
    }
}
