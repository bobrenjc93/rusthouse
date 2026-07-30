use std::fs;
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
fn execute_reads_csv_import_from_stdin_and_queries_it() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE people (id Int64, name String); \
             INSERT INTO people FORMAT CSV; \
             SELECT id, name FROM people ORDER BY id;",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");

    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"2,Grace Hopper\r\n1,\"Ada, Lovelace\"\r\n")
        .expect("write CSV");
    let output = child.wait_with_output().expect("wait for CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"name\",\"type\":\"String\"}],\"rows\":[[1,\"Ada, Lovelace\"],[2,\"Grace Hopper\"]]}]}\n"
    );
    assert!(
        String::from_utf8(output.stderr)
            .expect("UTF-8 stderr")
            .contains("INSERT 2")
    );
}

#[test]
fn input_option_reads_csv_with_names_from_a_file() {
    let path = std::env::temp_dir().join(format!(
        "rusthouse-csv-input-{}-{}.csv",
        std::process::id(),
        line!()
    ));
    fs::write(&path, "id,enabled,label\n1,true,東京\n2,0,quoted\n").expect("write input");

    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--format=csv")
        .arg("--execute")
        .arg(
            "CREATE TABLE flags (id Int64, enabled Bool, label String); \
             INSERT INTO flags FORMAT CSVWithNames; \
             SELECT label, enabled FROM flags ORDER BY label;",
        )
        .arg("--input")
        .arg(&path)
        .output()
        .expect("run CLI");
    fs::remove_file(path).expect("remove input");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "label,enabled\nquoted,false\n東京,true\n"
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
