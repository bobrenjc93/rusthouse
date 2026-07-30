use std::io::Write;
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
fn profile_writes_one_ndjson_object_per_query_to_stderr() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--profile",
            "--format=json",
            "--execute",
            "CREATE TABLE numbers (n Int64);
             INSERT INTO numbers VALUES (1), (2), (3);
             SELECT n FROM numbers WHERE n >= 2 ORDER BY n DESC LIMIT 1;
             SELECT COUNT(*) AS rows FROM numbers;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        concat!(
            "{\"results\":[",
            "{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[3]]},",
            "{\"columns\":[{\"name\":\"rows\",\"type\":\"Int64\"}],\"rows\":[[3]]}",
            "]}\n"
        )
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    let lines = stderr.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        2,
        "stderr should contain only NDJSON: {stderr}"
    );
    assert!(lines[0].starts_with(
        "{\"rows_read\":3,\"blocks_read\":1,\"blocks_pruned\":0,\"predicate_matches\":2,\"groups_created\":0,\"sort_inputs\":2,\"output_rows\":1,\"elapsed_ns\":"
    ));
    assert!(lines[0].ends_with('}'));
    assert!(lines[1].starts_with(
        "{\"rows_read\":3,\"blocks_read\":1,\"blocks_pruned\":0,\"predicate_matches\":3,\"groups_created\":1,\"sort_inputs\":0,\"output_rows\":1,\"elapsed_ns\":"
    ));
    assert!(lines[1].ends_with('}'));
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
